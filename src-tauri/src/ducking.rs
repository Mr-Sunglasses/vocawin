//! "Mute other audio while dictating" (VocaMac `AudioDucker`, off by default).
//!
//! macOS can only mute the whole output device. Windows can mute each app's
//! audio session, so VocaWin mutes only sessions that are playing right now,
//! and only ones that were not already muted, then unmutes exactly those. A
//! video paused before dictation, or an app the user muted on purpose, is
//! left alone.
//!
//! COM objects are thread-bound, so one worker thread owns them and takes
//! mute/restore requests over a channel.

use std::sync::mpsc;

enum Command {
    Mute,
    Restore,
}

pub struct Ducker {
    commands: Option<mpsc::Sender<Command>>,
}

impl Ducker {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let started = std::thread::Builder::new()
            .name("vocawin-ducking".into())
            .spawn(move || worker(receiver))
            .is_ok();
        Self {
            commands: started.then_some(sender),
        }
    }

    /// Mute every other app that is playing sound.
    pub fn mute_others(&self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Mute);
        }
    }

    /// Unmute what `mute_others` muted. Safe to call when nothing was.
    pub fn restore(&self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Restore);
        }
    }
}

#[cfg(windows)]
fn worker(receiver: mpsc::Receiver<Command>) {
    use windows::Win32::Media::Audio::ISimpleAudioVolume;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut muted: Vec<ISimpleAudioVolume> = Vec::new();
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Mute => {
                if !muted.is_empty() {
                    continue;
                }
                match mute_playing_sessions() {
                    Ok(sessions) => {
                        if !sessions.is_empty() {
                            crate::logbuf::debug(format!(
                                "Muted {} app(s) while dictating.",
                                sessions.len()
                            ));
                        }
                        muted = sessions;
                    }
                    Err(error) => {
                        crate::logbuf::warn(format!("Could not mute other audio: {error}"))
                    }
                }
            }
            Command::Restore => {
                for volume in muted.drain(..) {
                    unsafe {
                        let _ = volume.SetMute(false, std::ptr::null());
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn mute_playing_sessions(
) -> windows::core::Result<Vec<windows::Win32::Media::Audio::ISimpleAudioVolume>> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
        IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    let own_process = std::process::id();
    let mut muted = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;
        for index in 0..sessions.GetCount()? {
            let Ok(control) = sessions.GetSession(index) else {
                continue;
            };
            if control.GetState().ok() != Some(AudioSessionStateActive) {
                continue;
            }
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            // System sounds carry VocaWin's own cues; leave them and ourselves.
            if control2.IsSystemSoundsSession().0 == 0 {
                continue;
            }
            if control2.GetProcessId().ok() == Some(own_process) {
                continue;
            }
            let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            if volume.GetMute().map(|m| m.as_bool()).unwrap_or(true) {
                continue;
            }
            if volume.SetMute(true, std::ptr::null()).is_ok() {
                muted.push(volume);
            }
        }
    }
    Ok(muted)
}

#[cfg(not(windows))]
fn worker(receiver: mpsc::Receiver<Command>) {
    while receiver.recv().is_ok() {}
}
