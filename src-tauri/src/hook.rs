//! Low-level keyboard hook for dictation hotkeys on Windows.
//!
//! RegisterHotKey cannot bind a lone modifier. This WH_KEYBOARD_LL hook can see
//! VK_RCONTROL vs VK_RMENU and consumes matching keys so they do not leak.
//! Lone Right Alt is AltGr-safe: Ctrl+Right Alt (AltGr) is never consumed.
//!
//! Windows sends Alt as a SYSKEY. Down and up are matched from WM_KEY* and
//! WM_SYSKEY*, from LLKHF_UP, and from VK_MENU plus the extended bit (Right
//! Alt). Identity is the bound side only: Left Alt does not end a Right Alt
//! hold. Generic VK_MENU matches that bound side, never the other Alt.
//! Hold state is Idle or Recording (armed). First matching down while
//! Idle starts. Matching downs while Recording are typematic and swallow.
//! Matching up while Recording stops. Lost-up recovery is only the long
//! safety timeout (max recording + 5s). Do not poll GetAsyncKeyState on
//! the consumed Alt. A different vk (Ctrl for AltGr) is fine. Do not
//! SendInput from the hook callback; a synthetic unstick is queued on
//! vocawin-hotkey-actor after a real bound-side up.
//!
//! The same hook also carries VocaMac's extra shortcuts: Escape cancels a
//! live dictation (only while one is armed, so Escape reaches apps otherwise),
//! and the hands-free and paste-last shortcuts fire once per press. A
//! WH_MOUSE_LL hook is installed only while a mouse button is bound, so the
//! middle or side buttons can dictate like the hotkey.
//!
//! Settings' Record button captures a new shortcut here too (`begin_capture`),
//! as Handy does: the hook sees keys before an IME or the webview can take
//! them (Ctrl+Space never reached the page), with the side of each modifier.
//! Captured keys are eaten until they come up; Escape cancels, and a capture
//! nobody finishes lets go after `CAPTURE_TIMEOUT`.

#![allow(dead_code)] // Hook symbols are Windows-only; Linux CI still typechecks the module.

use crate::hotkey::HotkeySpec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

// Aggregate modifier VKs used with GetAsyncKeyState for combos.
const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: u32 = 0x12;
const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_INJECTED: u32 = 0x10;
const LLKHF_UP: u32 = 0x80;
const WH_KEYBOARD_LL: i32 = 13;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
pub const VK_ESCAPE: u32 = 0x1B;
const WH_MOUSE_LL: i32 = 14;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const LLMHF_INJECTED: u32 = 0x01;
/// Posted to the hook thread when the mouse binding changes.
const WM_APP_MOUSE_BINDING: u32 = 0x8000 + 1;

/// A shortcut capture nobody finishes stops eating keys after this long.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

/// Mac uses max recording + 5s. Default max is 60s, so 65s.
pub const DEFAULT_SAFETY_TIMEOUT: Duration = Duration::from_secs(65);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    Pressed,
    Released,
    /// Escape while a dictation is armed for cancel, with that take's id,
    /// so a late Escape can never throw away the take after it.
    Cancel(u64),
    /// Hands-free shortcut: start, or stop a running session.
    HandsFree,
    /// Type the last dictation again.
    PasteLast,
    MouseDown,
    MouseUp,
}

/// A mouse button that can dictate like the hotkey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Middle,
    X1,
    X2,
}

impl MouseButton {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "middle" => Some(Self::Middle),
            "x1" | "back" => Some(Self::X1),
            "x2" | "forward" => Some(Self::X2),
            _ => None,
        }
    }
}

/// How a shortcut capture ended, sent to Settings as `hotkey-captured`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum CaptureOutcome {
    /// The canonical settings string, such as "Ctrl+Space" or "AltRight".
    Shortcut(String),
    /// Keys that cannot be a shortcut; capture keeps listening.
    Refused(String),
    Cancelled,
}

/// Keys seen while Settings records a shortcut.
#[derive(Debug)]
struct Capture {
    started: Instant,
    /// Keys down now whose up must be eaten too.
    held: Vec<u32>,
    /// Modifiers pressed in this attempt, for a lone-modifier shortcut.
    modifiers: Vec<u32>,
    /// A shortcut was taken or the capture cancelled; only ups are eaten.
    finished: bool,
}

impl Capture {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            held: Vec::new(),
            modifiers: Vec::new(),
            finished: false,
        }
    }
}

/// One key event during a capture: whether to eat it, and any outcome.
fn capture_step(capture: &mut Capture, vk: u32, edge: KeyEdge) -> (bool, Option<CaptureOutcome>) {
    use crate::hotkey::is_modifier_vk;
    match edge {
        KeyEdge::Other => (false, None),
        KeyEdge::Up => {
            let Some(position) = capture.held.iter().position(|held| *held == vk) else {
                return (false, None);
            };
            capture.held.remove(position);
            let modifiers_up = !capture.held.iter().any(|held| is_modifier_vk(*held));
            if capture.finished || !is_modifier_vk(vk) || !modifiers_up || capture.modifiers.is_empty() {
                return (true, None);
            }
            // Only modifiers were pressed, and all are up again.
            let pressed = std::mem::take(&mut capture.modifiers);
            if pressed.len() == 1 {
                match crate::hotkey::from_keys(false, false, false, pressed[0]) {
                    Ok(spec) => {
                        capture.finished = true;
                        (true, Some(CaptureOutcome::Shortcut(spec)))
                    }
                    Err(error) => (true, Some(CaptureOutcome::Refused(error))),
                }
            } else {
                (
                    true,
                    Some(CaptureOutcome::Refused(
                        "Modifiers alone only work one at a time, such as Right Alt. Add a key, for example Ctrl+Space.".into(),
                    )),
                )
            }
        }
        KeyEdge::Down if capture.finished => {
            // Typematic repeats of a captured key stay eaten; anything new
            // belongs to the user again.
            (capture.held.contains(&vk), None)
        }
        KeyEdge::Down => {
            if !capture.held.contains(&vk) {
                capture.held.push(vk);
            }
            if is_modifier_vk(vk) {
                if !capture.modifiers.contains(&vk) {
                    capture.modifiers.push(vk);
                }
                return (true, None);
            }
            let held_modifiers: Vec<u32> = capture
                .held
                .iter()
                .copied()
                .filter(|held| is_modifier_vk(*held))
                .collect();
            if vk == VK_ESCAPE && held_modifiers.is_empty() {
                capture.finished = true;
                return (true, Some(CaptureOutcome::Cancelled));
            }
            // Whatever happens, this press is not a lone-modifier shortcut.
            capture.modifiers.clear();
            if vk == VK_LWIN || vk == VK_RWIN {
                return (
                    true,
                    Some(CaptureOutcome::Refused(
                        "Win/Super shortcuts are reserved on Windows. Pick another key.".into(),
                    )),
                );
            }
            let ctrl = held_modifiers.iter().any(|held| is_ctrl_vk(*held));
            let alt = held_modifiers.iter().any(|held| is_alt_vk(*held));
            let shift = held_modifiers.iter().any(|held| is_shift_vk(*held));
            match crate::hotkey::from_keys(ctrl, alt, shift, vk) {
                Ok(spec) => {
                    capture.finished = true;
                    (true, Some(CaptureOutcome::Shortcut(spec)))
                }
                Err(error) => (true, Some(CaptureOutcome::Refused(error))),
            }
        }
    }
}

/// What the hook does with a key before the hold logic sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpecialAction {
    Pass,
    Swallow,
    Emit(HookEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyEdge {
    Down,
    Up,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HoldAction {
    None,
    Start,
    /// Bound-side up. Unstick only this side, and only after a real up.
    Stop,
    /// Extra down while holding (Windows typematic). Eat it, keep the hold.
    Swallow,
}

/// OpenWhispr-style hold: idle until the first down, then recording
/// until the matching up. Typematic downs stay in Recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HoldSession {
    Idle,
    Recording { vk: u32 },
}

impl HoldSession {
    fn armed(self) -> bool {
        matches!(self, HoldSession::Recording { .. })
    }

    fn held_vk(self) -> Option<u32> {
        match self {
            HoldSession::Idle => None,
            HoldSession::Recording { vk } => Some(vk),
        }
    }
}

struct HookShared {
    app: Option<AppHandle>,
    binding: Option<HotkeySpec>,
    /// True while Settings Record is capturing a new combo (Mac-style pause).
    capture_paused: bool,
    /// True while auto-pause apps are running.
    dictation_paused: bool,
    /// Listener is bound. Not the same as a live hold.
    listener_enabled: bool,
    session: HoldSession,
    hold_gen: u64,
    safety_timeout: Duration,
    /// Hands-free and paste-last shortcuts.
    actions: Vec<(HotkeySpec, HookEvent)>,
    /// Keys of actions that fired and are still down (their up is eaten).
    latched: Vec<u32>,
    /// The take Escape cancels while it records or transcribes, if any.
    cancel_armed: Option<u64>,
    escape_swallowed: bool,
    mouse_button: Option<MouseButton>,
    mouse_held: bool,
    /// Settings is recording a new shortcut.
    capture: Option<Capture>,
}

enum ActorMsg {
    Event(HookEvent),
    Captured(CaptureOutcome),
    /// Bound-side key-up only, and only after the hook has returned.
    Unstick(u32),
}

static SHARED: OnceLock<Mutex<HookShared>> = OnceLock::new();
static HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);
static ACTOR_TX: OnceLock<mpsc::Sender<ActorMsg>> = OnceLock::new();
static HOOK_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn shared() -> &'static Mutex<HookShared> {
    SHARED.get_or_init(|| {
        Mutex::new(HookShared {
            app: None,
            binding: None,
            capture_paused: false,
            dictation_paused: false,
            listener_enabled: false,
            session: HoldSession::Idle,
            hold_gen: 0,
            safety_timeout: DEFAULT_SAFETY_TIMEOUT,
            actions: Vec::new(),
            latched: Vec::new(),
            cancel_armed: None,
            escape_swallowed: false,
            mouse_button: None,
            mouse_held: false,
            capture: None,
        })
    })
}

pub fn start(app: AppHandle) {
    {
        let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
        guard.app = Some(app.clone());
    }
    if HOOK_ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let _ = ACTOR_TX.set(tx);
    std::thread::Builder::new()
        .name("vocawin-hotkey-actor".into())
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    ActorMsg::Event(event) => crate::on_hotkey_event(&app, event),
                    ActorMsg::Captured(outcome) => {
                        let _ = app.emit("hotkey-captured", outcome);
                    }
                    ActorMsg::Unstick(vk) => unstick_modifier(vk),
                }
            }
        })
        .ok();
    std::thread::Builder::new()
        .name("vocawin-hotkey".into())
        .spawn(|| {
            if let Err(error) = hook_thread_main() {
                eprintln!("VocaWin hotkey hook stopped: {error}");
            }
            HOOK_ACTIVE.store(false, Ordering::SeqCst);
        })
        .ok();
}

pub fn set_binding(spec: HotkeySpec) {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.binding = Some(spec);
    guard.listener_enabled = true;
}

pub fn set_safety_timeout(timeout: Duration) {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.safety_timeout = timeout;
}

pub fn clear_held_vk() {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.session = HoldSession::Idle;
    guard.hold_gen = guard.hold_gen.wrapping_add(1);
}

pub fn clear_binding() {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.binding = None;
    guard.listener_enabled = false;
    let held = guard.session.held_vk();
    guard.session = HoldSession::Idle;
    guard.hold_gen = guard.hold_gen.wrapping_add(1);
    drop(guard);
    if let Some(vk) = held {
        emit_released();
        queue_unstick(vk);
    }
}

/// Start capturing a shortcut for Settings. Dictation shortcuts pause until
/// `end_capture`. False when the hook is not running (other platforms), so
/// the page records keys itself.
pub fn begin_capture() -> bool {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.capture_paused = true;
    if !cfg!(windows) || !HOOK_ACTIVE.load(Ordering::SeqCst) {
        return false;
    }
    guard.capture = Some(Capture::new(Instant::now()));
    true
}

/// Stop capturing and let dictation shortcuts work again. A finished
/// capture still eats the ups of the keys it took, then clears itself.
pub fn end_capture() {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    if guard.capture.as_ref().is_some_and(|capture| !capture.finished) {
        guard.capture = None;
    }
    guard.capture_paused = false;
}

fn send_captured(outcome: CaptureOutcome) {
    if let Some(tx) = ACTOR_TX.get() {
        let _ = tx.send(ActorMsg::Captured(outcome));
    }
}

/// Runs a key through an active capture. `Some(eat)` when the capture
/// handled it; `None` when no capture is running (or it just timed out).
fn capture_key(guard: &mut HookShared, vk: u32, edge: KeyEdge, now: Instant) -> Option<bool> {
    let capture = guard.capture.as_mut()?;
    if now.duration_since(capture.started) >= CAPTURE_TIMEOUT {
        let finished = capture.finished;
        guard.capture = None;
        if !finished {
            guard.capture_paused = false;
            send_captured(CaptureOutcome::Cancelled);
        }
        return None;
    }
    let (eat, outcome) = capture_step(capture, vk, edge);
    if capture.finished && capture.held.is_empty() {
        guard.capture = None;
    }
    if let Some(outcome) = outcome {
        send_captured(outcome);
    }
    Some(eat)
}

pub fn set_capture_paused(paused: bool) {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.capture_paused = paused;
}

pub fn set_dictation_paused(paused: bool) {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.dictation_paused = paused;
}

/// Hands-free and paste-last bindings. Replaces the previous set.
pub fn set_action_bindings(actions: Vec<(HotkeySpec, HookEvent)>) {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.actions = actions;
    guard.latched.clear();
}

/// Arm Escape for take `session` while it records or transcribes, or disarm
/// with `None`. Disarming leaves an Escape that is already down swallowed
/// until it comes up.
pub fn set_cancel_armed(session: Option<u64>) {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    guard.cancel_armed = session;
}

/// Disarm Escape only if it is still armed for `session`: a newer take may
/// have armed it for itself meanwhile.
pub fn disarm_cancel_for(session: u64) {
    let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
    if guard.cancel_armed == Some(session) {
        guard.cancel_armed = None;
    }
}

/// Bind a mouse button, or `None` to release it and remove the mouse hook.
pub fn set_mouse_button(button: Option<MouseButton>) {
    {
        let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
        guard.mouse_button = button;
        if button.is_none() {
            guard.mouse_held = false;
        }
    }
    let thread = HOOK_THREAD_ID.load(Ordering::SeqCst);
    if thread != 0 {
        post_mouse_binding_changed(thread);
    }
}

#[cfg(windows)]
fn post_mouse_binding_changed(thread: u32) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
    unsafe {
        let _ = PostThreadMessageW(thread, WM_APP_MOUSE_BINDING, WPARAM(0), LPARAM(0));
    }
}

#[cfg(not(windows))]
fn post_mouse_binding_changed(_thread: u32) {}

/// Escape handling, decided before the hold logic sees the key.
fn escape_action(
    guard: &mut HookShared,
    vk: u32,
    edge: KeyEdge,
) -> SpecialAction {
    if vk != VK_ESCAPE {
        return SpecialAction::Pass;
    }
    match edge {
        KeyEdge::Down if guard.escape_swallowed => SpecialAction::Swallow,
        KeyEdge::Down if guard.cancel_armed.is_some() && !guard.capture_paused => {
            guard.escape_swallowed = true;
            let session = guard.cancel_armed.take().unwrap_or_default();
            SpecialAction::Emit(HookEvent::Cancel(session))
        }
        KeyEdge::Up if guard.escape_swallowed => {
            guard.escape_swallowed = false;
            SpecialAction::Swallow
        }
        _ => SpecialAction::Pass,
    }
}

/// Hands-free / paste-last: fire once on the first down, eat repeats and the
/// matching up. `matches` decides whether a binding's key and modifiers are
/// down (GetAsyncKeyState on Windows).
fn shortcut_action(
    guard: &mut HookShared,
    vk: u32,
    edge: KeyEdge,
    matches: impl Fn(&HotkeySpec, u32) -> bool,
) -> SpecialAction {
    match edge {
        KeyEdge::Down => {
            if guard.latched.contains(&vk) {
                return SpecialAction::Swallow;
            }
            if guard.capture_paused || guard.dictation_paused || !guard.listener_enabled {
                return SpecialAction::Pass;
            }
            let hit = guard
                .actions
                .iter()
                .find(|(spec, _)| matches(spec, vk))
                .map(|(_, event)| *event);
            match hit {
                Some(event) => {
                    guard.latched.push(vk);
                    SpecialAction::Emit(event)
                }
                None => SpecialAction::Pass,
            }
        }
        KeyEdge::Up => {
            if let Some(position) = guard.latched.iter().position(|held| *held == vk) {
                guard.latched.remove(position);
                SpecialAction::Swallow
            } else {
                SpecialAction::Pass
            }
        }
        KeyEdge::Other => SpecialAction::Pass,
    }
}

/// Mouse button edge → what to do with it. Only the bound button is touched,
/// and a paused listener lets every click through.
fn mouse_action(guard: &mut HookShared, button: MouseButton, down: bool) -> SpecialAction {
    if guard.mouse_button != Some(button) {
        return SpecialAction::Pass;
    }
    if down {
        if guard.capture_paused || guard.dictation_paused || !guard.listener_enabled {
            return SpecialAction::Pass;
        }
        guard.mouse_held = true;
        SpecialAction::Emit(HookEvent::MouseDown)
    } else if guard.mouse_held {
        guard.mouse_held = false;
        SpecialAction::Emit(HookEvent::MouseUp)
    } else {
        SpecialAction::Pass
    }
}

fn emit(event: HookEvent) {
    // Cancel must not wait behind the actor: it may be busy transcribing the
    // take Escape is meant to stop, and would only see Cancel after typing.
    if let HookEvent::Cancel(_) = event {
        let app = shared()
            .lock()
            .map(|guard| guard.app.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().app.clone());
        if let Some(app) = app {
            let _ = std::thread::Builder::new()
                .name("vocawin-cancel".into())
                .spawn(move || crate::on_hotkey_event(&app, event));
        }
        return;
    }
    if let Some(tx) = ACTOR_TX.get() {
        let _ = tx.send(ActorMsg::Event(event));
    }
}

fn emit_released() {
    emit(HookEvent::Released);
}

fn queue_unstick(vk: u32) {
    if let Some(tx) = ACTOR_TX.get() {
        let _ = tx.send(ActorMsg::Unstick(vk));
    }
}

fn arm_safety_timer(timeout: Duration, gen: u64) {
    std::thread::Builder::new()
        .name("vocawin-hotkey-safety".into())
        .spawn(move || {
            std::thread::sleep(timeout);
            let mut guard = shared().lock().unwrap_or_else(|e| e.into_inner());
            if guard.hold_gen != gen || !guard.session.armed() {
                return;
            }
            guard.session = HoldSession::Idle;
            drop(guard);
            emit_released();
        })
        .ok();
}

fn bump_hold_gen(guard: &mut HookShared) -> u64 {
    guard.hold_gen = guard.hold_gen.wrapping_add(1);
    guard.hold_gen
}

#[cfg(windows)]
fn hook_thread_main() -> Result<(), String> {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        HHOOK, MSG, WINDOWS_HOOK_ID,
    };

    unsafe {
        let module = GetModuleHandleW(None).map_err(|error| error.to_string())?;
        let hook = SetWindowsHookExW(
            WINDOWS_HOOK_ID(WH_KEYBOARD_LL),
            Some(low_level_proc),
            HINSTANCE(module.0),
            0,
        )
        .map_err(|error| format!("Could not install keyboard hook: {error}"))?;

        // A mouse hook sees every mouse move system-wide, so it is only
        // installed while a button is bound.
        let mut mouse_hook: Option<HHOOK> = None;
        let reconcile_mouse = |mouse_hook: &mut Option<HHOOK>| {
            let wanted = shared()
                .lock()
                .map(|guard| guard.mouse_button.is_some())
                .unwrap_or(false);
            if wanted && mouse_hook.is_none() {
                match SetWindowsHookExW(
                    WINDOWS_HOOK_ID(WH_MOUSE_LL),
                    Some(mouse_proc),
                    HINSTANCE(module.0),
                    0,
                ) {
                    Ok(handle) => *mouse_hook = Some(handle),
                    Err(error) => {
                        crate::logbuf::error(format!("Could not install mouse hook: {error}"))
                    }
                }
            } else if !wanted {
                if let Some(handle) = mouse_hook.take() {
                    let _ = UnhookWindowsHookEx(handle);
                }
            }
        };
        HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
        reconcile_mouse(&mut mouse_hook);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, windows::Win32::Foundation::HWND::default(), 0, 0).into() {
            if msg.hwnd.0.is_null() && msg.message == WM_APP_MOUSE_BINDING {
                reconcile_mouse(&mut mouse_hook);
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        HOOK_THREAD_ID.store(0, Ordering::SeqCst);
        if let Some(handle) = mouse_hook.take() {
            let _ = UnhookWindowsHookEx(handle);
        }
        let _ = UnhookWindowsHookEx(hook);
    }
    Ok(())
}

#[cfg(not(windows))]
fn hook_thread_main() -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn low_level_proc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, KBDLLHOOKSTRUCT};

    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if info.flags.0 & LLKHF_INJECTED != 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let flags = info.flags.0;
    let edge = classify_edge(wparam.0 as u32, flags);
    if edge == KeyEdge::Other {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let vk = resolve_vk(info.vkCode, flags & LLKHF_EXTENDED != 0);
    let consume = {
        let mut guard = match shared().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(eat) = capture_key(&mut guard, vk, edge, Instant::now()) {
            if eat {
                return LRESULT(1);
            }
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        match escape_action(&mut guard, vk, edge) {
            SpecialAction::Pass => {}
            SpecialAction::Swallow => return LRESULT(1),
            SpecialAction::Emit(event) => {
                drop(guard);
                emit(event);
                return LRESULT(1);
            }
        }
        match shortcut_action(&mut guard, vk, edge, down_matches) {
            SpecialAction::Pass => {}
            SpecialAction::Swallow => return LRESULT(1),
            SpecialAction::Emit(event) => {
                drop(guard);
                emit(event);
                return LRESULT(1);
            }
        }

        let altgr_blocks = edge == KeyEdge::Down
            && matches!(guard.binding, Some(HotkeySpec::Lone { vk: bound }) if bound == crate::hotkey::VK_RMENU)
            && ctrl_is_down();
        let action = hold_action(
            guard.session,
            vk,
            edge,
            guard.binding.as_ref(),
            altgr_blocks,
        );
        if action == HoldAction::None
            && combo_modifier_dropped(guard.binding.as_ref(), guard.session.held_vk(), vk, edge)
        {
            // Stay Recording so the later base key-up is still eaten.
            let _gen = bump_hold_gen(&mut guard);
            drop(guard);
            emit_released();
            return LRESULT(1);
        }

        match action {
            HoldAction::None => {
                return unsafe { CallNextHookEx(None, code, wparam, lparam) };
            }
            HoldAction::Swallow => {
                return LRESULT(1);
            }
            HoldAction::Start => {
                if guard.capture_paused || guard.dictation_paused || !guard.listener_enabled {
                    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                }
                guard.session = HoldSession::Recording { vk };
                let gen = bump_hold_gen(&mut guard);
                let timeout = guard.safety_timeout;
                drop(guard);
                emit(HookEvent::Pressed);
                arm_safety_timer(timeout, gen);
                true
            }
            HoldAction::Stop => {
                let held = guard.session.held_vk();
                guard.session = HoldSession::Idle;
                let _ = bump_hold_gen(&mut guard);
                drop(guard);
                emit_released();
                if let Some(held) = held {
                    queue_unstick(held);
                }
                true
            }
        }
    };

    if consume {
        return LRESULT(1);
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(windows)]
unsafe extern "system" fn mouse_proc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, MSLLHOOKSTRUCT};

    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    if info.flags & LLMHF_INJECTED != 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let Some((button, down)) = mouse_edge(wparam.0 as u32, info.mouseData) else {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    };
    let action = {
        let mut guard = match shared().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        mouse_action(&mut guard, button, down)
    };
    match action {
        SpecialAction::Pass => unsafe { CallNextHookEx(None, code, wparam, lparam) },
        SpecialAction::Swallow => LRESULT(1),
        SpecialAction::Emit(event) => {
            emit(event);
            LRESULT(1)
        }
    }
}

/// Which button a mouse message is about, and whether it went down.
fn mouse_edge(message: u32, mouse_data: u32) -> Option<(MouseButton, bool)> {
    match message {
        WM_MBUTTONDOWN => Some((MouseButton::Middle, true)),
        WM_MBUTTONUP => Some((MouseButton::Middle, false)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let button = match (mouse_data >> 16) & 0xFFFF {
                1 => MouseButton::X1,
                2 => MouseButton::X2,
                _ => return None,
            };
            Some((button, message == WM_XBUTTONDOWN))
        }
        _ => None,
    }
}

fn classify_edge(wparam: u32, flags: u32) -> KeyEdge {
    if is_key_up(wparam, flags) {
        KeyEdge::Up
    } else if is_key_down(wparam, flags) {
        KeyEdge::Down
    } else {
        KeyEdge::Other
    }
}

fn bound_vk(binding: Option<&HotkeySpec>) -> Option<u32> {
    match binding {
        Some(HotkeySpec::Lone { vk }) | Some(HotkeySpec::Combo { vk, .. }) => Some(*vk),
        None => None,
    }
}

/// Generic VK_MENU matches the bound side only. Left Alt is not Right Alt.
fn vks_are_same_hold(held: u32, incoming: u32, bound: u32) -> bool {
    if held == incoming {
        return true;
    }
    (incoming == VK_MENU && held == bound) || (held == VK_MENU && incoming == bound)
}

fn is_same_hold(session: HoldSession, incoming: u32, binding: Option<&HotkeySpec>) -> bool {
    let Some(held) = session.held_vk() else {
        return false;
    };
    match bound_vk(binding) {
        Some(bound) => vks_are_same_hold(held, incoming, bound),
        None => held == incoming,
    }
}

fn hold_action(
    session: HoldSession,
    vk: u32,
    edge: KeyEdge,
    binding: Option<&HotkeySpec>,
    altgr_blocks_down: bool,
) -> HoldAction {
    match edge {
        KeyEdge::Other => HoldAction::None,
        KeyEdge::Up => {
            if session.armed() && is_same_hold(session, vk, binding) {
                HoldAction::Stop
            } else {
                HoldAction::None
            }
        }
        KeyEdge::Down => {
            if session.armed() {
                if is_same_hold(session, vk, binding) {
                    HoldAction::Swallow
                } else {
                    HoldAction::None
                }
            } else if altgr_blocks_down {
                HoldAction::None
            } else if binding.is_some_and(|spec| down_matches(spec, vk)) {
                HoldAction::Start
            } else {
                HoldAction::None
            }
        }
    }
}

fn combo_modifier_dropped(
    binding: Option<&HotkeySpec>,
    held: Option<u32>,
    vk: u32,
    edge: KeyEdge,
) -> bool {
    let Some(HotkeySpec::Combo {
        ctrl,
        alt,
        shift,
        vk: base,
    }) = binding
    else {
        return false;
    };
    if held != Some(*base) || edge != KeyEdge::Up {
        return false;
    }
    (*ctrl && is_ctrl_vk(vk)) || (*alt && is_alt_vk(vk)) || (*shift && is_shift_vk(vk))
}

fn is_key_down(wparam: u32, flags: u32) -> bool {
    (wparam == WM_KEYDOWN || wparam == WM_SYSKEYDOWN) && flags & LLKHF_UP == 0
}

fn is_key_up(wparam: u32, flags: u32) -> bool {
    wparam == WM_KEYUP || wparam == WM_SYSKEYUP || flags & LLKHF_UP != 0
}

/// Map generic VK_MENU onto the side Windows meant (extended = Right Alt).
fn resolve_vk(vk: u32, extended: bool) -> u32 {
    if vk == VK_MENU {
        if extended {
            crate::hotkey::VK_RMENU
        } else {
            crate::hotkey::VK_LMENU
        }
    } else {
        vk
    }
}

fn down_matches(binding: &HotkeySpec, vk: u32) -> bool {
    match binding {
        HotkeySpec::Lone { vk: bound } => {
            if vk != *bound {
                return false;
            }
            // AltGr is Left Ctrl + Right Alt. Apply this only on key-down so a
            // Ctrl flicker cannot drop the matching key-up.
            if *bound == crate::hotkey::VK_RMENU && ctrl_is_down() {
                return false;
            }
            true
        }
        HotkeySpec::Combo {
            ctrl,
            alt,
            shift,
            vk: bound,
        } => vk == *bound && mods_match(*ctrl, *alt, *shift),
    }
}

fn is_alt_vk(vk: u32) -> bool {
    vk == VK_MENU || vk == crate::hotkey::VK_LMENU || vk == crate::hotkey::VK_RMENU
}

fn is_ctrl_vk(vk: u32) -> bool {
    vk == VK_CONTROL as u32 || vk == crate::hotkey::VK_LCONTROL || vk == crate::hotkey::VK_RCONTROL
}

fn is_shift_vk(vk: u32) -> bool {
    vk == VK_SHIFT as u32 || vk == crate::hotkey::VK_LSHIFT || vk == crate::hotkey::VK_RSHIFT
}

fn ctrl_is_down() -> bool {
    #[cfg(windows)]
    {
        // Ctrl is a different vk from the consumed Right Alt. MSDN: async
        // state of an eaten key never updates, so never poll that key.
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        (unsafe { GetAsyncKeyState(VK_CONTROL) } as u16) & 0x8000 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn mods_match(want_ctrl: bool, want_alt: bool, want_shift: bool) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        let ctrl_down = unsafe { GetAsyncKeyState(VK_CONTROL) } as u16 & 0x8000 != 0;
        let alt_down = unsafe { GetAsyncKeyState(VK_MENU as i32) } as u16 & 0x8000 != 0;
        let shift_down = unsafe { GetAsyncKeyState(VK_SHIFT) } as u16 & 0x8000 != 0;
        ctrl_down == want_ctrl && alt_down == want_alt && shift_down == want_shift
    }
    #[cfg(not(windows))]
    {
        let _ = (want_ctrl, want_alt, want_shift);
        false
    }
}

/// Inject a key-up for the bound side only. Call this from the actor
/// thread after the hook has returned, never from low_level_proc.
fn unstick_modifier(vk: u32) {
    #[cfg(windows)]
    {
        if vk == crate::hotkey::VK_RMENU {
            inject_key_up(crate::hotkey::VK_RMENU as u16, true);
        } else if vk == crate::hotkey::VK_LMENU {
            inject_key_up(crate::hotkey::VK_LMENU as u16, false);
        } else if vk == crate::hotkey::VK_RCONTROL {
            inject_key_up(vk as u16, true);
        } else if vk == crate::hotkey::VK_LCONTROL {
            inject_key_up(vk as u16, false);
        } else if vk == crate::hotkey::VK_RSHIFT || vk == crate::hotkey::VK_LSHIFT {
            inject_key_up(vk as u16, false);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = vk;
    }
}

#[cfg(windows)]
fn inject_key_up(vk: u16, extended: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };
    let mut flags = KEYEVENTF_KEYUP;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{VK_LMENU, VK_RMENU};

    fn right_alt() -> HotkeySpec {
        HotkeySpec::Lone { vk: VK_RMENU }
    }

    #[test]
    fn syskey_and_keyup_flag_both_count_as_up() {
        assert!(is_key_up(WM_SYSKEYUP, 0));
        assert!(is_key_up(WM_KEYUP, 0));
        assert!(is_key_up(WM_SYSKEYDOWN, LLKHF_UP));
        assert!(is_key_down(WM_SYSKEYDOWN, 0));
        assert!(!is_key_down(WM_SYSKEYDOWN, LLKHF_UP));
        assert!(is_key_down(WM_KEYDOWN, 0));
        assert_eq!(classify_edge(WM_SYSKEYUP, 0), KeyEdge::Up);
        assert_eq!(classify_edge(WM_KEYUP, LLKHF_UP), KeyEdge::Up);
    }

    #[test]
    fn menu_plus_extended_is_right_alt() {
        assert_eq!(resolve_vk(VK_MENU, true), VK_RMENU);
        assert_eq!(resolve_vk(VK_MENU, false), VK_LMENU);
        assert_eq!(resolve_vk(VK_RMENU, true), VK_RMENU);
    }

    fn idle() -> HoldSession {
        HoldSession::Idle
    }

    fn recording() -> HoldSession {
        HoldSession::Recording { vk: VK_RMENU }
    }

    fn apply(session: HoldSession, vk: u32, edge: KeyEdge) -> HoldAction {
        hold_action(session, vk, edge, Some(&right_alt()), false)
    }

    #[test]
    fn left_alt_does_not_end_right_alt_hold() {
        assert_eq!(apply(recording(), VK_LMENU, KeyEdge::Up), HoldAction::None);
        assert_eq!(
            apply(recording(), VK_LMENU, KeyEdge::Down),
            HoldAction::None
        );
        assert_eq!(apply(recording(), VK_RMENU, KeyEdge::Up), HoldAction::Stop);
        assert_eq!(apply(recording(), VK_MENU, KeyEdge::Up), HoldAction::Stop);
    }

    #[test]
    fn generic_menu_matches_bound_side_only() {
        assert!(vks_are_same_hold(VK_RMENU, VK_RMENU, VK_RMENU));
        assert!(vks_are_same_hold(VK_RMENU, VK_MENU, VK_RMENU));
        assert!(vks_are_same_hold(VK_MENU, VK_RMENU, VK_RMENU));
        assert!(vks_are_same_hold(VK_LMENU, VK_MENU, VK_LMENU));
        assert!(!vks_are_same_hold(VK_RMENU, VK_LMENU, VK_RMENU));
        assert!(!vks_are_same_hold(VK_LMENU, VK_RMENU, VK_RMENU));
        assert!(!vks_are_same_hold(VK_LMENU, VK_MENU, VK_RMENU));
    }

    #[test]
    fn start_then_down_two_seconds_later_is_still_swallow() {
        assert_eq!(apply(idle(), VK_RMENU, KeyEdge::Down), HoldAction::Start);
        assert_eq!(
            apply(recording(), VK_RMENU, KeyEdge::Down),
            HoldAction::Swallow
        );
    }

    #[test]
    fn start_then_up_stops() {
        assert_eq!(apply(idle(), VK_RMENU, KeyEdge::Down), HoldAction::Start);
        assert_eq!(apply(recording(), VK_RMENU, KeyEdge::Up), HoldAction::Stop);
    }

    #[test]
    fn no_duplicate_press_events() {
        let binding = right_alt();
        let mut session = HoldSession::Idle;
        let mut starts = 0;
        let mut stops = 0;
        for _ in 0..40 {
            let action = hold_action(session, VK_RMENU, KeyEdge::Down, Some(&binding), false);
            match action {
                HoldAction::Start => {
                    starts += 1;
                    session = HoldSession::Recording { vk: VK_RMENU };
                }
                HoldAction::Swallow => {
                    assert!(session.armed());
                }
                other => panic!("unexpected down action {other:?}"),
            }
        }
        let up = hold_action(session, VK_RMENU, KeyEdge::Up, Some(&binding), false);
        assert_eq!(up, HoldAction::Stop);
        stops += 1;
        assert_eq!((starts, stops), (1, 1));
    }

    #[test]
    fn consumed_alt_repeat_is_not_treated_as_up() {
        assert_eq!(
            apply(recording(), VK_RMENU, KeyEdge::Down),
            HoldAction::Swallow
        );
        assert!(DEFAULT_SAFETY_TIMEOUT >= Duration::from_secs(60));
    }

    #[test]
    fn right_alt_down_starts_and_does_not_match_left() {
        let binding = right_alt();
        assert_eq!(apply(idle(), VK_RMENU, KeyEdge::Down), HoldAction::Start);
        assert_eq!(apply(idle(), VK_LMENU, KeyEdge::Down), HoldAction::None);
        assert!(!down_matches(&binding, VK_LMENU));
        assert!(down_matches(&binding, VK_RMENU) || cfg!(not(windows)));
    }

    fn test_shared() -> HookShared {
        HookShared {
            app: None,
            binding: Some(right_alt()),
            capture_paused: false,
            dictation_paused: false,
            listener_enabled: true,
            session: HoldSession::Idle,
            hold_gen: 0,
            safety_timeout: DEFAULT_SAFETY_TIMEOUT,
            actions: Vec::new(),
            latched: Vec::new(),
            cancel_armed: None,
            escape_swallowed: false,
            mouse_button: None,
            mouse_held: false,
            capture: None,
        }
    }

    use crate::hotkey::{VK_LCONTROL, VK_LSHIFT, VK_RCONTROL, VK_SPACE};

    fn run(capture: &mut Capture, keys: &[(u32, KeyEdge)]) -> Vec<(bool, Option<CaptureOutcome>)> {
        keys.iter().map(|(vk, edge)| capture_step(capture, *vk, *edge)).collect()
    }

    fn shortcut(spec: &str) -> Option<CaptureOutcome> {
        Some(CaptureOutcome::Shortcut(spec.into()))
    }

    #[test]
    fn ctrl_space_is_captured_and_its_keys_are_eaten_until_up() {
        let mut capture = Capture::new(Instant::now());
        let steps = run(
            &mut capture,
            &[
                (VK_LCONTROL, KeyEdge::Down),
                (VK_LCONTROL, KeyEdge::Down),
                (VK_SPACE, KeyEdge::Down),
                (VK_SPACE, KeyEdge::Down),
                (VK_SPACE, KeyEdge::Up),
                (VK_LCONTROL, KeyEdge::Up),
            ],
        );
        assert_eq!(steps[2], (true, shortcut("Ctrl+Space")));
        assert!(steps.iter().all(|(eat, _)| *eat));
        assert_eq!(steps.iter().filter(|(_, outcome)| outcome.is_some()).count(), 1);
        assert!(capture.finished && capture.held.is_empty());
        // After the capture, new keys belong to the user.
        assert_eq!(capture_step(&mut capture, 0x41, KeyEdge::Down), (false, None));
    }

    #[test]
    fn a_lone_modifier_is_captured_on_release_with_its_side() {
        let mut capture = Capture::new(Instant::now());
        let steps = run(&mut capture, &[(VK_RMENU, KeyEdge::Down), (VK_RMENU, KeyEdge::Up)]);
        assert_eq!(steps[1], (true, shortcut("AltRight")));
        let mut capture = Capture::new(Instant::now());
        let steps = run(&mut capture, &[(VK_LSHIFT, KeyEdge::Down), (VK_LSHIFT, KeyEdge::Up)]);
        assert_eq!(steps[1], (true, shortcut("ShiftLeft")));
    }

    #[test]
    fn two_modifiers_alone_are_refused_and_capture_goes_on() {
        let mut capture = Capture::new(Instant::now());
        let steps = run(
            &mut capture,
            &[
                (VK_LCONTROL, KeyEdge::Down),
                (VK_RMENU, KeyEdge::Down),
                (VK_RMENU, KeyEdge::Up),
                (VK_LCONTROL, KeyEdge::Up),
            ],
        );
        assert!(matches!(steps[3].1, Some(CaptureOutcome::Refused(_))));
        assert!(!capture.finished);
        let steps = run(&mut capture, &[(VK_RCONTROL, KeyEdge::Down), (VK_RCONTROL, KeyEdge::Up)]);
        assert_eq!(steps[1], (true, shortcut("ControlRight")));
    }

    #[test]
    fn a_refused_key_does_not_turn_into_a_lone_modifier() {
        let mut capture = Capture::new(Instant::now());
        let steps = run(
            &mut capture,
            &[
                (VK_LSHIFT, KeyEdge::Down),
                (0x41, KeyEdge::Down),
                (0x41, KeyEdge::Up),
                (VK_LSHIFT, KeyEdge::Up),
            ],
        );
        assert!(matches!(steps[1].1, Some(CaptureOutcome::Refused(_))));
        assert_eq!(steps[3], (true, None));
        assert!(!capture.finished);
    }

    #[test]
    fn escape_cancels_but_escape_with_a_modifier_is_refused() {
        let mut capture = Capture::new(Instant::now());
        assert_eq!(
            capture_step(&mut capture, VK_ESCAPE, KeyEdge::Down),
            (true, Some(CaptureOutcome::Cancelled))
        );
        assert_eq!(capture_step(&mut capture, VK_ESCAPE, KeyEdge::Up), (true, None));
        assert!(capture.held.is_empty());
        let mut capture = Capture::new(Instant::now());
        let steps = run(&mut capture, &[(VK_LMENU, KeyEdge::Down), (VK_ESCAPE, KeyEdge::Down)]);
        assert!(matches!(steps[1].1, Some(CaptureOutcome::Refused(_))));
    }

    #[test]
    fn the_windows_key_is_refused() {
        let mut capture = Capture::new(Instant::now());
        let steps = run(&mut capture, &[(VK_LWIN, KeyEdge::Down), (VK_LWIN, KeyEdge::Up)]);
        assert!(matches!(steps[0].1, Some(CaptureOutcome::Refused(_))));
        assert_eq!(steps[1], (true, None));
    }

    #[test]
    fn an_abandoned_capture_lets_go_after_the_timeout() {
        let mut shared = test_shared();
        let started = Instant::now();
        shared.capture = Some(Capture::new(started));
        shared.capture_paused = true;
        assert_eq!(capture_key(&mut shared, 0x41, KeyEdge::Down, started), Some(true));
        assert_eq!(
            capture_key(&mut shared, 0x42, KeyEdge::Down, started + CAPTURE_TIMEOUT),
            None
        );
        assert!(shared.capture.is_none() && !shared.capture_paused);
    }

    #[test]
    fn escape_passes_through_unless_armed() {
        let mut shared = test_shared();
        assert_eq!(escape_action(&mut shared, VK_ESCAPE, KeyEdge::Down), SpecialAction::Pass);
        assert_eq!(escape_action(&mut shared, VK_ESCAPE, KeyEdge::Up), SpecialAction::Pass);
    }

    #[test]
    fn armed_escape_cancels_once_and_eats_its_up() {
        let mut shared = test_shared();
        shared.cancel_armed = Some(7);
        assert_eq!(
            escape_action(&mut shared, VK_ESCAPE, KeyEdge::Down),
            SpecialAction::Emit(HookEvent::Cancel(7))
        );
        // Typematic repeats and the up are eaten; nothing fires twice.
        assert_eq!(escape_action(&mut shared, VK_ESCAPE, KeyEdge::Down), SpecialAction::Swallow);
        assert_eq!(escape_action(&mut shared, VK_ESCAPE, KeyEdge::Up), SpecialAction::Swallow);
        assert_eq!(escape_action(&mut shared, VK_ESCAPE, KeyEdge::Down), SpecialAction::Pass);
        assert_eq!(escape_action(&mut shared, 0x41, KeyEdge::Down), SpecialAction::Pass);
    }

    #[test]
    fn escape_is_left_alone_while_recording_a_hotkey() {
        let mut shared = test_shared();
        shared.cancel_armed = Some(7);
        shared.capture_paused = true;
        assert_eq!(escape_action(&mut shared, VK_ESCAPE, KeyEdge::Down), SpecialAction::Pass);
    }

    #[test]
    fn shortcuts_fire_once_per_press() {
        let mut shared = test_shared();
        shared.actions = vec![(HotkeySpec::Lone { vk: crate::hotkey::VK_F9 }, HookEvent::HandsFree)];
        let matches = |spec: &HotkeySpec, vk: u32| matches!(spec, HotkeySpec::Lone { vk: bound } if *bound == vk);
        let f9 = crate::hotkey::VK_F9;
        assert_eq!(
            shortcut_action(&mut shared, f9, KeyEdge::Down, matches),
            SpecialAction::Emit(HookEvent::HandsFree)
        );
        assert_eq!(shortcut_action(&mut shared, f9, KeyEdge::Down, matches), SpecialAction::Swallow);
        assert_eq!(shortcut_action(&mut shared, f9, KeyEdge::Up, matches), SpecialAction::Swallow);
        assert_eq!(shortcut_action(&mut shared, f9, KeyEdge::Up, matches), SpecialAction::Pass);
        assert_eq!(shortcut_action(&mut shared, 0x41, KeyEdge::Down, matches), SpecialAction::Pass);
        shared.dictation_paused = true;
        assert_eq!(shortcut_action(&mut shared, f9, KeyEdge::Down, matches), SpecialAction::Pass);
    }

    #[test]
    fn only_the_bound_mouse_button_dictates() {
        let mut shared = test_shared();
        assert_eq!(mouse_action(&mut shared, MouseButton::Middle, true), SpecialAction::Pass);
        shared.mouse_button = Some(MouseButton::X1);
        assert_eq!(mouse_action(&mut shared, MouseButton::Middle, true), SpecialAction::Pass);
        assert_eq!(
            mouse_action(&mut shared, MouseButton::X1, true),
            SpecialAction::Emit(HookEvent::MouseDown)
        );
        assert_eq!(
            mouse_action(&mut shared, MouseButton::X1, false),
            SpecialAction::Emit(HookEvent::MouseUp)
        );
        assert_eq!(mouse_action(&mut shared, MouseButton::X1, false), SpecialAction::Pass);
        shared.dictation_paused = true;
        assert_eq!(mouse_action(&mut shared, MouseButton::X1, true), SpecialAction::Pass);
    }

    #[test]
    fn mouse_messages_map_to_buttons() {
        assert_eq!(mouse_edge(WM_MBUTTONDOWN, 0), Some((MouseButton::Middle, true)));
        assert_eq!(mouse_edge(WM_XBUTTONUP, 2 << 16), Some((MouseButton::X2, false)));
        assert_eq!(mouse_edge(WM_XBUTTONDOWN, 1 << 16), Some((MouseButton::X1, true)));
        assert_eq!(mouse_edge(0x0201, 0), None);
        assert_eq!(MouseButton::parse("X2"), Some(MouseButton::X2));
        assert_eq!(MouseButton::parse(""), None);
    }

    #[test]
    fn altgr_filter_is_down_only() {
        let binding = right_alt();
        assert_eq!(
            hold_action(idle(), VK_RMENU, KeyEdge::Down, Some(&binding), true),
            HoldAction::None
        );
        assert_eq!(
            hold_action(recording(), VK_RMENU, KeyEdge::Up, Some(&binding), true),
            HoldAction::Stop
        );
    }
}
