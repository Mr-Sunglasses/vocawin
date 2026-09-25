//! Dictation output polish (parity with VocaMac DictationOutputFormatter)
//! and Windows text injection.
//!
//! Default insertion matches VocaMac (Accessibility first) and VocaLinux
//! (IBus/wtype first): type into the focused window and leave the clipboard
//! alone. Clipboard + Ctrl+V is the fallback, and that path restores the
//! previous clipboard unless the user opts into copy-to-clipboard.
//!
//! Notepad and WordPad are an exception: `KEYEVENTF_UNICODE` SendInput is
//! accepted (caret advances) but glyphs are dropped or blank, so those
//! targets prefer clipboard paste with restore. If the clipboard cannot
//! be fully restored, injection fails closed rather than reporting
//! SendInput success while the transcript never appears.

pub fn append_trailing_space(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    if text.ends_with(char::is_whitespace) {
        return text.to_string();
    }
    format!("{text} ")
}

pub fn capitalize_sentences(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(text.len());
    let mut capitalize_next = true;
    for ch in text.chars() {
        if capitalize_next && ch.is_ascii_lowercase() {
            out.push(ch.to_ascii_uppercase());
            capitalize_next = false;
            continue;
        }
        out.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            capitalize_next = true;
        } else if !ch.is_whitespace() {
            capitalize_next = false;
        }
    }
    out
}

pub fn apply_output_polish(text: &str, auto_capitalize: bool, trailing_space: bool) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut result = trimmed.to_string();
    if auto_capitalize {
        result = capitalize_sentences(&result);
    }
    if trailing_space {
        result = append_trailing_space(&result);
    }
    result
}

/// Wait until Ctrl, Alt, Shift and Win are all up, or `timeout` passes.
/// Text typed while a shortcut's modifiers are still held would arrive as
/// shortcuts instead (paste-last fires on key-down).
pub fn wait_for_modifiers_released(timeout: std::time::Duration) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        const MODIFIERS: [i32; 5] = [0x10, 0x11, 0x12, 0x5B, 0x5C];
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let held = MODIFIERS
                .iter()
                .any(|vk| (unsafe { GetAsyncKeyState(*vk) } as u16) & 0x8000 != 0);
            if !held {
                // Let the key-up reach the target app before typing.
                std::thread::sleep(std::time::Duration::from_millis(30));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
    #[cfg(not(windows))]
    let _ = timeout;
}

/// Controls whether dictation is also left on the system clipboard.
///
/// Matches VocaLinux `copy_to_clipboard` (default off) and VocaMac
/// `preserveClipboard` (default on): do not take over the clipboard unless
/// the user asks for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InjectOptions {
    pub copy_to_clipboard: bool,
    /// Paste (with clipboard restore) instead of typing, in every app.
    pub paste_everywhere: bool,
    /// Process names (`slack.exe`) that get a paste instead of typing: apps
    /// that drop typed characters.
    pub paste_apps: Vec<String>,
}

impl InjectOptions {
    pub fn restore_clipboard(&self) -> bool {
        !self.copy_to_clipboard
    }

    /// Whether the app in front should get a paste rather than typing.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn pastes_into(&self, process_name: Option<&str>) -> bool {
        self.paste_everywhere
            || process_name.is_some_and(|name| {
                let name = normalize_process_name(name);
                self.paste_apps
                    .iter()
                    .any(|app| normalize_process_name(app) == name)
            })
    }
}

// The typing and paste constants below are used by the Windows injector only.
#[cfg_attr(not(windows), allow(dead_code))]
/// Characters typed per SendInput call. One huge burst makes some apps
/// (Electron, terminals, remote sessions) drop or reorder characters.
const TYPING_CHUNK_CHARS: usize = 24;
/// Pause between chunks, so the target's input queue keeps up.
#[cfg_attr(not(windows), allow(dead_code))]
const TYPING_CHUNK_PAUSE_MS: u64 = 4;

/// Longest wait for the user to let go of Ctrl/Alt/Shift/Win before typing.
#[cfg_attr(not(windows), allow(dead_code))]
const MODIFIER_RELEASE_WAIT_MS: u64 = 600;

/// After Ctrl+V, how long the target gets to read the clipboard before the
/// previous contents come back. Slow apps (Office, Electron under load) read
/// it lazily; too short and they paste the old clipboard.
#[cfg_attr(not(windows), allow(dead_code))]
const PASTE_SETTLE_MS: u64 = 250;

/// Shown when the app in front runs as administrator. Windows (UIPI) drops
/// typed and pasted input into it without telling the sender.
#[cfg_attr(not(windows), allow(dead_code))]
const ELEVATED_TARGET: &str =
    "Admin app blocks typing. Text is on the clipboard: press Ctrl+V.";

/// Classic Notepad / WordPad accept UNICODE SendInput (caret moves) but
/// drop or blank the glyphs. Clipboard Ctrl+V usually works.
const CLIPBOARD_INJECT_PROCESS_NAMES: &[&str] = &["notepad.exe", "wordpad.exe"];

/// Lowercase basename, ensure `.exe` — same shape as `autopause`.
fn normalize_process_name(name: &str) -> String {
    let trimmed = name.trim().trim_matches('"').to_ascii_lowercase();
    let file_name = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&trimmed)
        .to_string();
    if file_name.ends_with(".exe") {
        file_name
    } else if file_name.is_empty() {
        file_name
    } else {
        format!("{file_name}.exe")
    }
}

fn prefers_clipboard_inject(process_name: &str) -> bool {
    CLIPBOARD_INJECT_PROCESS_NAMES.contains(&normalize_process_name(process_name).as_str())
}

pub fn inject(text: &str, options: &InjectOptions) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        inject_windows(text, options)
    }
    #[cfg(not(windows))]
    {
        let _ = options;
        Err("Text injection is available in Windows builds only.".into())
    }
}

#[cfg(windows)]
fn inject_windows(text: &str, options: &InjectOptions) -> Result<(), String> {
    // A combo hotkey or paste-last shortcut may still be held: typed text
    // would arrive as shortcuts (a newline as Ctrl+Enter sends a chat).
    wait_for_modifiers_released(std::time::Duration::from_millis(MODIFIER_RELEASE_WAIT_MS));
    if foreground_blocks_input() {
        // Hand the text over instead of reporting a success that typed
        // nothing. The user's own Ctrl+V is real input, which UIPI allows.
        write_clipboard_unicode(text)?;
        crate::logbuf::warn("Foreground app runs elevated; left the text on the clipboard.");
        return Err(ELEVATED_TARGET.into());
    }
    let foreground = foreground_process_name();
    // Prefer SendInput so the default path never opens the clipboard.
    // Clipboard + Ctrl+V is the fallback (layout-independent, like VocaLinux
    // ydotool paste) and restores the previous clipboard unless the user
    // enabled copy-to-clipboard.
    //
    // Notepad-like targets are the other way around: UNICODE SendInput
    // reports success but the app drops the glyphs, so paste first —
    // but only when the clipboard can be fully restored afterward.
    // GDI formats (bitmap / metafile / palette) cannot; EmptyClipboard
    // would drop them. Capture failure is the same: unknown must not
    // EmptyClipboard GDI. In both cases — and if paste itself fails —
    // do not fall through to SendInput (that would claim success while
    // dropping the transcript). Copy-to-clipboard paste failure is the
    // same for those targets: fail closed instead of SendInput.
    if options.copy_to_clipboard {
        return match inject_via_clipboard(text, false) {
            Ok(()) => {
                crate::logbuf::debug("Injected via clipboard (copy-to-clipboard on).");
                Ok(())
            }
            Err(clipboard_error) => match copy_to_clipboard_paste_failure_decision(
                foreground_prefers_clipboard(),
            ) {
                CopyToClipboardPasteFailureDecision::FailClosed => {
                    crate::logbuf::warn(format!(
                        "Clipboard paste failed for Notepad-like target ({clipboard_error}); not falling back to SendInput (glyphs would drop)."
                    ));
                    Err(notepad_like_copy_to_clipboard_paste_failed(&clipboard_error))
                }
                CopyToClipboardPasteFailureDecision::TrySendInput => inject_send_input(text)
                    .map_err(|failure| failure.message)
                    .and_then(|_| write_clipboard_unicode(text))
                    .map_err(|send_input_error| {
                        crate::logbuf::warn("Clipboard paste failed; SendInput also failed.");
                        format!(
                            "Clipboard paste failed ({clipboard_error}); SendInput also failed ({send_input_error})"
                        )
                    }),
            },
        };
    }
    if foreground.as_deref().is_some_and(prefers_clipboard_inject) {
        return inject_notepad_like(text);
    }
    if options.pastes_into(foreground.as_deref()) {
        match inject_via_clipboard(text, true) {
            Ok(()) => {
                crate::logbuf::debug("Injected via clipboard (paste chosen for this app).");
                return Ok(());
            }
            Err(error) => {
                crate::logbuf::warn(format!("Paste failed ({error}); typing instead."));
            }
        }
    }
    match inject_send_input(text) {
        Ok(()) => {
            crate::logbuf::debug("Injected via SendInput.");
            Ok(())
        }
        // Part of the text is already in the app: pasting all of it again
        // would duplicate what went in.
        Err(failure) if failure.typed_any => Err(failure.message),
        Err(failure) => inject_via_clipboard(text, true)
            .map(|()| {
                crate::logbuf::warn("SendInput failed; fell back to clipboard paste.");
            })
            .map_err(|clipboard_error| {
                crate::logbuf::error("SendInput and clipboard paste both failed.");
                format!(
                    "SendInput failed ({}); clipboard paste also failed ({clipboard_error})",
                    failure.message
                )
            }),
    }
}

/// Whether the foreground app runs at a higher integrity level than
/// VocaWin (an app run as administrator while VocaWin is not). SendInput
/// into it is dropped silently, so success cannot be detected afterwards.
#[cfg(windows)]
fn foreground_blocks_input() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let pid = unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    };
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    let ours = current_integrity_level();
    match process_integrity_level(pid) {
        Some(theirs) => ours.is_some_and(|ours| theirs > ours),
        // Same-user, same-level processes always allow this query; a refusal
        // means the target sits above us, unless we are elevated ourselves.
        None => ours.is_some_and(|ours| ours < HIGH_INTEGRITY_RID),
    }
}

#[cfg(windows)]
const HIGH_INTEGRITY_RID: u32 = 0x3000;

#[cfg(windows)]
fn current_integrity_level() -> Option<u32> {
    use windows::Win32::System::Threading::GetCurrentProcess;
    token_integrity_level(unsafe { GetCurrentProcess() })
}

#[cfg(windows)]
fn process_integrity_level(pid: u32) -> Option<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let level = token_integrity_level(process);
    unsafe {
        let _ = CloseHandle(process);
    }
    level
}

/// The mandatory-label RID of a process token (0x2000 medium, 0x3000 high).
#[cfg(windows)]
fn token_integrity_level(process: windows::Win32::Foundation::HANDLE) -> Option<u32> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::OpenProcessToken;
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut buffer = vec![0u8; 256];
        let mut needed = 0u32;
        let read = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            buffer.len() as u32,
            &mut needed,
        );
        let _ = CloseHandle(token);
        read.ok()?;
        let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, count as u32 - 1))
    }
}

/// Paste into Notepad/WordPad via clipboard+restore. Capture once and
/// reuse that snapshot; never treat UNICODE SendInput Ok as success.
#[cfg(windows)]
fn inject_notepad_like(text: &str) -> Result<(), String> {
    match notepad_like_clipboard_decision(capture_clipboard_snapshot()) {
        NotepadLikeClipboardDecision::PasteAndRestore(snapshot) => {
            match inject_via_clipboard_with_snapshot(text, snapshot) {
                Ok(()) => {
                    crate::logbuf::debug("Injected via clipboard (Notepad-like target).");
                    Ok(())
                }
                Err(clipboard_error) => {
                    crate::logbuf::warn(format!(
                        "Clipboard paste failed for Notepad-like target ({clipboard_error}); not falling back to SendInput (glyphs would drop)."
                    ));
                    Err(format!(
                        "Clipboard paste into Notepad/WordPad failed ({clipboard_error}). UNICODE SendInput would drop glyphs, so the transcript was not injected."
                    ))
                }
            }
        }
        NotepadLikeClipboardDecision::RejectUnpreservable => {
            crate::logbuf::warn(
                "Cannot inject into Notepad-like target: clipboard has unpreservable formats.",
            );
            Err(NOTEPAD_LIKE_UNPRESERVABLE.into())
        }
        NotepadLikeClipboardDecision::RejectCaptureFailed(detail) => {
            crate::logbuf::warn(format!(
                "Cannot inject into Notepad-like target: clipboard capture failed ({detail})."
            ));
            Err(NOTEPAD_LIKE_CAPTURE_FAILED.into())
        }
    }
}

#[cfg(windows)]
fn foreground_prefers_clipboard() -> bool {
    match foreground_process_name() {
        Some(name) => prefers_clipboard_inject(&name),
        None => false,
    }
}

#[cfg(windows)]
fn foreground_process_name() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == HWND::default() {
            return None;
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            cntUsage: 0,
            th32ProcessID: 0,
            th32DefaultHeapID: 0,
            th32ModuleID: 0,
            cntThreads: 0,
            th32ParentProcessID: 0,
            pcPriClassBase: 0,
            dwFlags: 0,
            szExeFile: [0; 260],
        };
        let mut name = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == pid {
                    let len = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
                    if !exe.is_empty() {
                        name = Some(exe);
                    }
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        name
    }
}

/// Why typing stopped. `typed_any` is set once some text already reached
/// the app, so the caller must not paste the whole transcript again.
#[cfg(windows)]
struct SendInputFailure {
    typed_any: bool,
    message: String,
}

/// Type `text` as UNICODE key events, newlines as Enter, a few characters at
/// a time. Each character's key-down/up pairs (two for a surrogate pair,
/// such as an emoji) stay in one call.
#[cfg(windows)]
fn inject_send_input(text: &str) -> Result<(), SendInputFailure> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, VK_RETURN};
    let groups = typing_groups(text);
    let chunks: Vec<&[TypedKey]> = groups.chunks(TYPING_CHUNK_CHARS).collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let inputs: Vec<INPUT> = chunk
            .iter()
            .flat_map(|key| match key {
                TypedKey::Enter => vec![key_down(VK_RETURN), key_up(VK_RETURN)],
                TypedKey::Units(units) => units
                    .iter()
                    .flat_map(|unit| [unicode_key(*unit, false), unicode_key(*unit, true)])
                    .collect(),
            })
            .collect();
        let mut sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent == 0 {
            // Another thread's input can block the queue for a moment.
            std::thread::sleep(std::time::Duration::from_millis(20));
            sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        }
        if sent as usize != inputs.len() {
            return Err(SendInputFailure {
                typed_any: index > 0 || sent > 0,
                message: if index > 0 || sent > 0 {
                    "Windows stopped accepting typed input partway through".into()
                } else {
                    "Windows rejected SendInput".into()
                },
            });
        }
        if index + 1 < chunks.len() {
            std::thread::sleep(std::time::Duration::from_millis(TYPING_CHUNK_PAUSE_MS));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn unicode_key(unit: u16, up: bool) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY,
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: if up {
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                } else {
                    KEYEVENTF_UNICODE
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// One typed character: Enter for a line break (`\r\n`, `\r`, or `\n`), or
/// the UTF-16 units of anything else.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum TypedKey {
    Enter,
    Units(Vec<u16>),
}

#[cfg_attr(not(windows), allow(dead_code))]
fn typing_groups(text: &str) -> Vec<TypedKey> {
    let mut groups = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                groups.push(TypedKey::Enter);
            }
            '\n' => groups.push(TypedKey::Enter),
            _ => {
                let mut units = [0u16; 2];
                groups.push(TypedKey::Units(ch.encode_utf16(&mut units).to_vec()));
            }
        }
    }
    groups
}

#[cfg(windows)]
const CF_UNICODETEXT: u32 = 13;

/// GDI clipboard formats that are not HGLOBAL and cannot be round-tripped
/// with GetClipboardData/SetClipboardData the same way text can.
///
/// CF_BITMAP=2, CF_METAFILEPICT=3, CF_PALETTE=9, CF_ENHMETAFILE=14,
/// CF_OWNERDISPLAY=0x0080, CF_DSPBITMAP=0x0082, CF_DSPMETAFILEPICT=0x0083,
/// CF_DSPENHMETAFILE=0x008E.
fn is_gdi_clipboard_format(format: u32) -> bool {
    matches!(format, 2 | 3 | 9 | 14 | 0x0080 | 0x0082 | 0x0083 | 0x008E)
}

/// True when every enumerated format can be snapshotted and restored.
/// False if any GDI / unpreservable format is present.
fn clipboard_formats_are_preservable(formats: impl IntoIterator<Item = u32>) -> bool {
    formats
        .into_iter()
        .all(|format| !is_gdi_clipboard_format(format))
}

/// User-facing errors for the Notepad/WordPad prefer-clipboard path.
/// UNICODE SendInput reports success but those apps drop glyphs, so we
/// never claim Ok via SendInput when clipboard paste is unsafe or failed.
#[cfg(windows)]
const NOTEPAD_LIKE_UNPRESERVABLE: &str = concat!(
    "Cannot inject into Notepad/WordPad: the clipboard has image or other ",
    "formats that cannot be restored after paste. Clear the clipboard or copy ",
    "text first, then try again.",
);

#[cfg(windows)]
const NOTEPAD_LIKE_CAPTURE_FAILED: &str = concat!(
    "Cannot inject into Notepad/WordPad: the clipboard could not be captured, ",
    "so it cannot be restored after paste. Clear the clipboard or copy text ",
    "first, then try again.",
);

/// Trailing copy-to-clipboard paste-failure text. Prefixed with the
/// clipboard error the same way `inject_notepad_like` formats paste failure.
const NOTEPAD_LIKE_COPY_TO_CLIPBOARD_PASTE_FAILED: &str = concat!(
    "UNICODE SendInput would drop glyphs, so the transcript was not injected. ",
    "Copy-to-clipboard may have left the text on the clipboard.",
);

/// Generic restore-path reject (SendInput fallback). Notepad/WordPad uses
/// `NOTEPAD_LIKE_UNPRESERVABLE` instead so that path stays fail-closed.
const CLIPBOARD_RESTORE_UNPRESERVABLE: &str = concat!(
    "Clipboard has image or other formats that cannot be restored after paste; ",
    "refusing clipboard paste restore. Clear the clipboard or copy text first, ",
    "then try again.",
);

/// Outcome of the Notepad/WordPad clipboard-prefer path *before* paste.
/// `Ok(snapshot)` pastes when `is_preservable()`; otherwise reject
/// unpreservable. `Err` owns the capture-failure detail.
#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum NotepadLikeClipboardDecision {
    PasteAndRestore(ClipboardSnapshot),
    RejectUnpreservable,
    RejectCaptureFailed(String),
}

#[cfg(windows)]
fn notepad_like_clipboard_decision(
    capture: Result<ClipboardSnapshot, String>,
) -> NotepadLikeClipboardDecision {
    match capture {
        Ok(snapshot) => {
            if snapshot.is_preservable() {
                NotepadLikeClipboardDecision::PasteAndRestore(snapshot)
            } else {
                NotepadLikeClipboardDecision::RejectUnpreservable
            }
        }
        Err(detail) => NotepadLikeClipboardDecision::RejectCaptureFailed(detail),
    }
}

/// After copy-to-clipboard Ctrl+V fails: Notepad/WordPad must not fall
/// through to UNICODE SendInput (Ok with dropped glyphs). Other apps may.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyToClipboardPasteFailureDecision {
    FailClosed,
    TrySendInput,
}

fn copy_to_clipboard_paste_failure_decision(
    prefers_clipboard: bool,
) -> CopyToClipboardPasteFailureDecision {
    if prefers_clipboard {
        CopyToClipboardPasteFailureDecision::FailClosed
    } else {
        CopyToClipboardPasteFailureDecision::TrySendInput
    }
}

fn notepad_like_copy_to_clipboard_paste_failed(clipboard_error: &str) -> String {
    format!(
        "Clipboard paste into Notepad/WordPad failed ({clipboard_error}). {NOTEPAD_LIKE_COPY_TO_CLIPBOARD_PASTE_FAILED}"
    )
}

#[cfg(windows)]
impl NotepadLikeClipboardDecision {
    fn reject_message(&self) -> Option<&'static str> {
        match self {
            Self::PasteAndRestore(_) => None,
            Self::RejectUnpreservable => Some(NOTEPAD_LIKE_UNPRESERVABLE),
            Self::RejectCaptureFailed(_) => Some(NOTEPAD_LIKE_CAPTURE_FAILED),
        }
    }
}

/// Empty captured formats means "clipboard was empty" only when no GDI
/// format was skipped. An incomplete snapshot must not EmptyClipboard.
fn should_clear_clipboard_on_restore(formats_empty: bool, skipped_unpreservable: bool) -> bool {
    formats_empty && !skipped_unpreservable
}

/// Restore-path paste must not overwrite the clipboard when the snapshot
/// skipped GDI formats. Callers reject before `write_clipboard_unicode`.
fn may_replace_clipboard_for_restore(skipped_unpreservable: bool) -> bool {
    !skipped_unpreservable
}

#[cfg(windows)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ClipboardSnapshot {
    formats: Vec<(u32, Vec<u8>)>,
    /// Set when EnumClipboardFormats listed a GDI format we skipped.
    /// Restore cannot recover those; an empty `formats` vec is then
    /// incomplete rather than "clipboard was empty".
    skipped_unpreservable: bool,
}

#[cfg(windows)]
impl ClipboardSnapshot {
    fn is_preservable(&self) -> bool {
        !self.skipped_unpreservable
    }
}

#[cfg(windows)]
#[derive(Clone)]
enum PendingRestore {
    Idle,
    Snapshot(ClipboardSnapshot),
    Failed,
}

#[cfg(windows)]
struct ClipboardRestoreState {
    generation: u64,
    pending: PendingRestore,
}

#[cfg(windows)]
fn clipboard_restore_state() -> &'static std::sync::Mutex<ClipboardRestoreState> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<ClipboardRestoreState>> =
        std::sync::OnceLock::new();
    STATE.get_or_init(|| {
        std::sync::Mutex::new(ClipboardRestoreState {
            generation: 0,
            pending: PendingRestore::Idle,
        })
    })
}

#[cfg(windows)]
fn inject_via_clipboard(text: &str, restore: bool) -> Result<(), String> {
    inject_via_clipboard_inner(text, restore, None)
}

/// Paste via clipboard and restore using a snapshot captured by the caller
/// so restore does not recapture (and does not copy every format twice).
#[cfg(windows)]
fn inject_via_clipboard_with_snapshot(
    text: &str,
    snapshot: ClipboardSnapshot,
) -> Result<(), String> {
    inject_via_clipboard_inner(text, true, Some(snapshot))
}

#[cfg(windows)]
fn inject_via_clipboard_inner(
    text: &str,
    restore: bool,
    snapshot: Option<ClipboardSnapshot>,
) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, VK_CONTROL, VK_V};

    let generation = {
        let mut state = clipboard_restore_state()
            .lock()
            .map_err(|_| "clipboard restore lock poisoned")?;
        if restore {
            if matches!(state.pending, PendingRestore::Idle) {
                let resolved = match snapshot {
                    Some(snapshot) => Ok(snapshot),
                    None => capture_clipboard_snapshot(),
                };
                match resolved {
                    Ok(snapshot) => {
                        // Fail closed before write: an unpreservable snapshot
                        // cannot be restored, and writing would destroy GDI.
                        if !may_replace_clipboard_for_restore(snapshot.skipped_unpreservable) {
                            return Err(CLIPBOARD_RESTORE_UNPRESERVABLE.into());
                        }
                        state.pending = PendingRestore::Snapshot(snapshot);
                    }
                    Err(_) => {
                        state.pending = PendingRestore::Failed;
                    }
                }
            }
        } else {
            state.pending = PendingRestore::Idle;
        }
        state.generation = state.generation.wrapping_add(1);
        state.generation
    };

    write_clipboard_unicode(text)?;

    let inputs = [
        key_down(VK_CONTROL),
        key_down(VK_V),
        key_up(VK_V),
        key_up(VK_CONTROL),
    ];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        if restore {
            restore_pending_clipboard(generation, None);
        }
        return Err("Ctrl+V SendInput failed".into());
    }
    // Give the target app time to read the clipboard before it is restored.
    std::thread::sleep(std::time::Duration::from_millis(PASTE_SETTLE_MS));
    if restore {
        restore_pending_clipboard(generation, Some(text));
    }
    Ok(())
}

#[cfg(windows)]
fn restore_pending_clipboard(generation: u64, expected_text: Option<&str>) {
    let pending = {
        let Ok(mut state) = clipboard_restore_state().lock() else {
            return;
        };
        if state.generation != generation {
            return;
        }
        std::mem::replace(&mut state.pending, PendingRestore::Idle)
    };
    // VocaLinux: if the user (or a clipboard manager) replaced our
    // transcription during the delay, leave that newer value alone.
    if let Some(text) = expected_text {
        if !clipboard_unicode_equals(text) {
            return;
        }
    }
    match pending {
        PendingRestore::Snapshot(snapshot) => {
            let _ = restore_clipboard_snapshot(&snapshot);
        }
        PendingRestore::Failed | PendingRestore::Idle => {}
    }
}

#[cfg(windows)]
fn key_down(
    vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT};
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: Default::default(),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
fn key_up(
    vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
fn open_clipboard_with_retry() -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::DataExchange::OpenClipboard;
    for _ in 0..10 {
        if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err("OpenClipboard: busy".into())
}

#[cfg(windows)]
fn read_clipboard_unicode() -> Result<String, String> {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
            return Err("no unicode clipboard".into());
        }
        open_clipboard_with_retry()?;
        let handle = match GetClipboardData(CF_UNICODETEXT) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = CloseClipboard();
                return Err(format!("GetClipboardData: {error}"));
            }
        };
        let ptr = GlobalLock(windows::Win32::Foundation::HGLOBAL(handle.0)) as *const u16;
        if ptr.is_null() {
            let _ = CloseClipboard();
            return Err("clipboard lock failed".into());
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        let text = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(windows::Win32::Foundation::HGLOBAL(handle.0));
        let _ = CloseClipboard();
        Ok(text)
    }
}

#[cfg(windows)]
fn clipboard_unicode_equals(text: &str) -> bool {
    read_clipboard_unicode().is_ok_and(|current| current == text)
}

#[cfg(windows)]
fn write_clipboard_unicode(text: &str) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    let encoded: Vec<u16> = HSTRING::from(text)
        .as_wide()
        .iter()
        .copied()
        .chain([0])
        .collect();
    let bytes = encoded.len() * 2;
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes)
            .map_err(|error| format!("clipboard alloc failed: {error}"))?;
        let ptr = GlobalLock(mem) as *mut u16;
        if ptr.is_null() {
            global_free(mem);
            return Err("clipboard lock failed".into());
        }
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), ptr, encoded.len());
        let _ = GlobalUnlock(mem);
        if let Err(error) = open_clipboard_with_retry() {
            global_free(mem);
            return Err(error);
        }
        let result = (|| {
            EmptyClipboard().map_err(|error| format!("EmptyClipboard: {error}"))?;
            SetClipboardData(CF_UNICODETEXT, HANDLE(mem.0))
                .map_err(|error| format!("SetClipboardData: {error}"))?;
            Ok::<(), String>(())
        })();
        let _ = CloseClipboard();
        if result.is_err() {
            global_free(mem);
        }
        result
    }
}

/// windows 0.58 exports GlobalFree from Foundation, not System::Memory.
/// A successful free returns a null handle, which the crate reports as Err.
#[cfg(windows)]
unsafe fn global_free(mem: windows::Win32::Foundation::HGLOBAL) {
    let _ = windows::Win32::Foundation::GlobalFree(mem);
}

#[cfg(windows)]
fn clear_clipboard() -> Result<(), String> {
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard};
    open_clipboard_with_retry()?;
    let result = unsafe { EmptyClipboard() }.map_err(|error| format!("EmptyClipboard: {error}"));
    let _ = unsafe { CloseClipboard() };
    result.map(|_| ())
}

#[cfg(windows)]
fn capture_clipboard_snapshot() -> Result<ClipboardSnapshot, String> {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EnumClipboardFormats, GetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    open_clipboard_with_retry()?;
    let result = (|| unsafe {
        let mut snapshot = ClipboardSnapshot::default();
        let mut format = 0u32;
        loop {
            format = EnumClipboardFormats(format);
            if format == 0 {
                break;
            }
            if is_gdi_clipboard_format(format) {
                snapshot.skipped_unpreservable = true;
                continue;
            }
            let Ok(handle) = GetClipboardData(format) else {
                continue;
            };
            let mem = windows::Win32::Foundation::HGLOBAL(handle.0);
            let size = GlobalSize(mem);
            if size == 0 {
                continue;
            }
            let ptr = GlobalLock(mem) as *const u8;
            if ptr.is_null() {
                continue;
            }
            let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
            let _ = GlobalUnlock(mem);
            snapshot.formats.push((format, bytes));
        }
        Ok(snapshot)
    })();
    let _ = unsafe { CloseClipboard() };
    result
}

#[cfg(windows)]
fn restore_clipboard_snapshot(snapshot: &ClipboardSnapshot) -> Result<(), String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    if should_clear_clipboard_on_restore(
        snapshot.formats.is_empty(),
        snapshot.skipped_unpreservable,
    ) {
        return clear_clipboard();
    }
    if snapshot.formats.is_empty() {
        // Incomplete snapshot (GDI formats were skipped): do not
        // EmptyClipboard. Incomplete != empty.
        return Ok(());
    }
    open_clipboard_with_retry()?;
    let result = (|| unsafe {
        EmptyClipboard().map_err(|error| format!("EmptyClipboard: {error}"))?;
        for (format, bytes) in &snapshot.formats {
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
                .map_err(|error| format!("clipboard alloc failed: {error}"))?;
            let ptr = GlobalLock(mem) as *mut u8;
            if ptr.is_null() {
                global_free(mem);
                return Err("clipboard lock failed".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = GlobalUnlock(mem);
            if SetClipboardData(*format, HANDLE(mem.0)).is_err() {
                global_free(mem);
            }
        }
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    result
}

/// Copy Debug log text (and other UI strings) to the system clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        write_clipboard_unicode(text)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err("Clipboard copy is available in Windows builds only.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capitalizes_sentences_like_mac() {
        assert_eq!(
            capitalize_sentences("hello world. next one! ok? yes"),
            "Hello world. Next one! Ok? Yes"
        );
    }

    #[test]
    fn trailing_space_skips_blank_and_existing() {
        assert_eq!(append_trailing_space(""), "");
        assert_eq!(append_trailing_space("hi "), "hi ");
        assert_eq!(append_trailing_space("hi"), "hi ");
    }

    #[test]
    fn polish_applies_in_order() {
        assert_eq!(
            apply_output_polish("hello. world", true, true),
            "Hello. World "
        );
    }

    #[test]
    fn clipboard_is_not_taken_over_by_default() {
        let options = InjectOptions::default();
        assert!(!options.copy_to_clipboard);
        assert!(options.restore_clipboard());
    }

    #[test]
    fn copy_to_clipboard_skips_restore() {
        let options = InjectOptions {
            copy_to_clipboard: true,
            ..InjectOptions::default()
        };
        assert!(!options.restore_clipboard());
    }

    #[test]
    fn paste_is_chosen_per_app_or_everywhere() {
        let options = InjectOptions {
            paste_apps: vec!["Slack".into(), "mstsc.exe".into()],
            ..InjectOptions::default()
        };
        assert!(options.pastes_into(Some("slack.exe")));
        assert!(options.pastes_into(Some("MSTSC.EXE")));
        assert!(!options.pastes_into(Some("chrome.exe")));
        assert!(!options.pastes_into(None));
        let everywhere = InjectOptions {
            paste_everywhere: true,
            ..InjectOptions::default()
        };
        assert!(everywhere.pastes_into(Some("chrome.exe")));
        assert!(everywhere.pastes_into(None));
    }

    #[test]
    fn typing_keeps_emoji_whole_and_turns_line_breaks_into_enter() {
        let groups = typing_groups("a🎉\r\nb\nc\r");
        assert_eq!(
            groups,
            vec![
                TypedKey::Units(vec!['a' as u16]),
                TypedKey::Units(vec![0xD83C, 0xDF89]),
                TypedKey::Enter,
                TypedKey::Units(vec!['b' as u16]),
                TypedKey::Enter,
                TypedKey::Units(vec!['c' as u16]),
                TypedKey::Enter,
            ]
        );
        // Chunks split between characters, never inside a surrogate pair.
        let long = "🎉".repeat(TYPING_CHUNK_CHARS * 2 + 1);
        for chunk in typing_groups(&long).chunks(TYPING_CHUNK_CHARS) {
            assert!(chunk.iter().all(|key| matches!(key, TypedKey::Units(units) if units.len() == 2)));
        }
    }

    #[test]
    fn notepad_like_targets_prefer_clipboard_inject() {
        assert!(prefers_clipboard_inject("notepad.exe"));
        assert!(prefers_clipboard_inject("NOTEPAD.EXE"));
        assert!(prefers_clipboard_inject("Notepad"));
        assert!(prefers_clipboard_inject("wordpad.exe"));
        assert!(!prefers_clipboard_inject("chrome.exe"));
        assert!(!prefers_clipboard_inject("Code.exe"));
        assert!(!prefers_clipboard_inject("explorer.exe"));
        assert!(!prefers_clipboard_inject(""));
    }

    #[test]
    fn clipboard_without_gdi_formats_is_preservable() {
        const CF_TEXT: u32 = 1;
        const CF_DIB: u32 = 8;
        const CF_UNICODETEXT: u32 = 13;
        const CF_HDROP: u32 = 15;

        assert!(clipboard_formats_are_preservable([] as [u32; 0]));
        assert!(clipboard_formats_are_preservable([CF_UNICODETEXT]));
        assert!(clipboard_formats_are_preservable([
            CF_TEXT,
            CF_UNICODETEXT,
            CF_DIB,
            CF_HDROP
        ]));
        assert!(!is_gdi_clipboard_format(CF_UNICODETEXT));
        assert!(!is_gdi_clipboard_format(CF_DIB));
    }

    #[test]
    fn clipboard_with_gdi_formats_is_not_preservable() {
        const CF_TEXT: u32 = 1;
        const CF_BITMAP: u32 = 2;
        const CF_METAFILEPICT: u32 = 3;
        const CF_PALETTE: u32 = 9;
        const CF_UNICODETEXT: u32 = 13;
        const CF_ENHMETAFILE: u32 = 14;
        const CF_OWNERDISPLAY: u32 = 0x0080;
        const CF_DSPBITMAP: u32 = 0x0082;
        const CF_DSPMETAFILEPICT: u32 = 0x0083;
        const CF_DSPENHMETAFILE: u32 = 0x008E;

        for format in [
            CF_BITMAP,
            CF_METAFILEPICT,
            CF_PALETTE,
            CF_ENHMETAFILE,
            CF_OWNERDISPLAY,
            CF_DSPBITMAP,
            CF_DSPMETAFILEPICT,
            CF_DSPENHMETAFILE,
        ] {
            assert!(is_gdi_clipboard_format(format));
            assert!(!clipboard_formats_are_preservable([format]));
        }
        // Mixed: restore would drop the GDI object after EmptyClipboard.
        assert!(!clipboard_formats_are_preservable([
            CF_TEXT,
            CF_UNICODETEXT,
            CF_ENHMETAFILE
        ]));
        // GDI-only: formats vec is empty after skip; restore must not
        // treat that as "clipboard was empty" and EmptyClipboard.
        assert!(!clipboard_formats_are_preservable([
            CF_BITMAP,
            CF_ENHMETAFILE
        ]));
    }

    #[cfg(windows)]
    #[test]
    fn notepad_like_fails_closed_when_clipboard_cannot_be_restored() {
        let preservable = ClipboardSnapshot::default();
        assert_eq!(
            notepad_like_clipboard_decision(Ok(preservable.clone())),
            NotepadLikeClipboardDecision::PasteAndRestore(preservable)
        );
        let unpreservable_snapshot = ClipboardSnapshot {
            skipped_unpreservable: true,
            ..ClipboardSnapshot::default()
        };
        assert_eq!(
            notepad_like_clipboard_decision(Ok(unpreservable_snapshot)),
            NotepadLikeClipboardDecision::RejectUnpreservable
        );
        assert_eq!(
            notepad_like_clipboard_decision(Err("some detail".into())),
            NotepadLikeClipboardDecision::RejectCaptureFailed("some detail".into())
        );
        assert!(
            notepad_like_clipboard_decision(Ok(ClipboardSnapshot::default()))
                .reject_message()
                .is_none()
        );

        let unpreservable = notepad_like_clipboard_decision(Ok(ClipboardSnapshot {
            skipped_unpreservable: true,
            ..ClipboardSnapshot::default()
        }))
        .reject_message()
        .expect("unpreservable must reject");
        assert!(
            unpreservable.contains("Clear the clipboard")
                && unpreservable.contains("copy text")
                && unpreservable.contains("try again")
        );

        let capture_failed = notepad_like_clipboard_decision(Err("some detail".into()))
            .reject_message()
            .expect("capture failure must reject");
        assert!(
            capture_failed.contains("Clear the clipboard")
                && capture_failed.contains("copy text")
                && capture_failed.contains("try again")
        );
    }

    /// Real Windows input: type and paste into a classic Edit control and a
    /// RichEdit control (the two families most desktop apps build on), the
    /// way a dictation reaches another app. Runs on the Windows CI runner.
    /// Skips, with a note, when the session cannot give its window the
    /// foreground (no interactive desktop): SendInput needs one.
    #[cfg(windows)]
    #[test]
    fn typing_and_pasting_reach_real_windows_text_controls() {
        use windows::core::{w, PCWSTR};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::LibraryLoader::LoadLibraryW;
        use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
            GetWindowTextLengthW, GetWindowTextW, PeekMessageW, SetForegroundWindow,
            SetWindowTextW, ShowWindow, TranslateMessage, ES_AUTOVSCROLL, ES_MULTILINE, MSG,
            PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        };

        fn pump_for(duration: std::time::Duration) {
            let deadline = std::time::Instant::now() + duration;
            let mut msg = MSG::default();
            while std::time::Instant::now() < deadline {
                unsafe {
                    while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        /// Run `work` on another thread while this one (the window's
        /// thread) pumps the input it produces.
        fn while_pumping<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
            let handle = std::thread::spawn(work);
            while !handle.is_finished() {
                pump_for(std::time::Duration::from_millis(20));
            }
            pump_for(std::time::Duration::from_millis(400));
            handle.join().expect("injection thread panicked")
        }

        fn text_of(hwnd: HWND) -> String {
            unsafe {
                let length = GetWindowTextLengthW(hwnd).max(0) as usize;
                let mut buffer = vec![0u16; length + 1];
                let copied = GetWindowTextW(hwnd, &mut buffer).max(0) as usize;
                String::from_utf16_lossy(&buffer[..copied])
            }
        }

        fn focus(hwnd: HWND) -> bool {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
                pump_for(std::time::Duration::from_millis(150));
                let _ = SetFocus(hwnd);
                GetForegroundWindow() == hwnd
            }
        }

        let _ = unsafe { LoadLibraryW(w!("Msftedit.dll")) };
        let classes: [(&str, PCWSTR); 2] = [("Edit", w!("EDIT")), ("RichEdit", w!("RICHEDIT50W"))];
        for (name, class) in classes {
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    w!(""),
                    WS_OVERLAPPEDWINDOW
                        | WS_VISIBLE
                        | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL) as u32),
                    100,
                    100,
                    600,
                    300,
                    HWND::default(),
                    None,
                    None,
                    None,
                )
            }
            .unwrap_or_else(|error| panic!("could not create a {name} window: {error}"));
            if !focus(hwnd) {
                eprintln!("skipping {name}: this session cannot take the foreground");
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                continue;
            }

            // Typing: accents, an emoji (a surrogate pair), a line break,
            // and more than one chunk of characters.
            let typed = format!("Héllo wörld 🎉\nline two {}", "x".repeat(60));
            let sent = typed.clone();
            while_pumping(move || inject_send_input(&sent).map_err(|f| f.message)).unwrap();
            // Edit reports a line break as "\r\n", RichEdit as "\r".
            let got = text_of(hwnd).replace("\r\n", "\n").replace('\r', "\n");
            assert_eq!(got, typed, "{name}: typed text");

            // Pasting restores what was on the clipboard before.
            unsafe {
                let _ = SetWindowTextW(hwnd, w!(""));
            }
            assert!(focus(hwnd), "{name}: focus lost");
            write_clipboard_unicode("keep me").unwrap();
            while_pumping(|| inject_via_clipboard("pasted text", true)).unwrap();
            assert_eq!(text_of(hwnd), "pasted text", "{name}: pasted text");
            assert_eq!(read_clipboard_unicode().unwrap(), "keep me", "{name}: clipboard restored");

            // The public path with the default options types.
            unsafe {
                let _ = SetWindowTextW(hwnd, w!(""));
            }
            assert!(focus(hwnd), "{name}: focus lost");
            while_pumping(|| inject("Default path ", &InjectOptions::default())).unwrap();
            assert_eq!(text_of(hwnd), "Default path ", "{name}: default inject");
            assert_eq!(read_clipboard_unicode().unwrap(), "keep me", "{name}: clipboard untouched");

            // Paste chosen for every app goes through the clipboard.
            unsafe {
                let _ = SetWindowTextW(hwnd, w!(""));
            }
            assert!(focus(hwnd), "{name}: focus lost");
            let paste = InjectOptions {
                paste_everywhere: true,
                ..InjectOptions::default()
            };
            while_pumping(move || inject("Pasted path", &paste)).unwrap();
            assert_eq!(text_of(hwnd), "Pasted path", "{name}: paste everywhere");

            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }

    #[test]
    fn incomplete_clipboard_snapshot_must_not_clear_on_restore() {
        // Empty + fully captured: clipboard really was empty.
        assert!(should_clear_clipboard_on_restore(true, false));
        // Empty formats but GDI was skipped: incomplete != empty.
        assert!(!should_clear_clipboard_on_restore(true, true));
        // Non-empty HGLOBAL formats: restore those, do not clear.
        assert!(!should_clear_clipboard_on_restore(false, false));
        assert!(!should_clear_clipboard_on_restore(false, true));
    }

    #[test]
    fn unpreservable_snapshot_must_not_replace_clipboard_for_restore() {
        assert!(may_replace_clipboard_for_restore(false));
        assert!(!may_replace_clipboard_for_restore(true));
        assert!(
            CLIPBOARD_RESTORE_UNPRESERVABLE.contains("refusing clipboard paste restore")
                && !CLIPBOARD_RESTORE_UNPRESERVABLE.contains("Notepad")
        );
    }

    #[test]
    fn copy_to_clipboard_paste_failure_fails_closed_for_notepad_like() {
        assert_eq!(
            copy_to_clipboard_paste_failure_decision(true),
            CopyToClipboardPasteFailureDecision::FailClosed
        );
        assert_eq!(
            copy_to_clipboard_paste_failure_decision(false),
            CopyToClipboardPasteFailureDecision::TrySendInput
        );
        assert_eq!(
            copy_to_clipboard_paste_failure_decision(prefers_clipboard_inject("notepad.exe")),
            CopyToClipboardPasteFailureDecision::FailClosed
        );
        assert_eq!(
            copy_to_clipboard_paste_failure_decision(prefers_clipboard_inject("wordpad.exe")),
            CopyToClipboardPasteFailureDecision::FailClosed
        );
        assert_eq!(
            copy_to_clipboard_paste_failure_decision(prefers_clipboard_inject("chrome.exe")),
            CopyToClipboardPasteFailureDecision::TrySendInput
        );

        let message = notepad_like_copy_to_clipboard_paste_failed("Ctrl+V SendInput failed");
        assert!(
            message.contains("Notepad/WordPad")
                && message.contains("Ctrl+V SendInput failed")
                && message.contains("UNICODE SendInput")
                && message.contains("not injected")
                && message.contains("clipboard")
        );
    }
}
