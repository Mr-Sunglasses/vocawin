//! Local dictation history, stored only on this PC.
//!
//! Matches VocaMac's `DictationHistoryStore`: the audio of a take is written
//! before transcription starts, so a crash or a failed decode never loses
//! what was said. Entries can be searched, copied, replayed, retried with the
//! current model, and deleted, and they expire after the retention the user
//! picked (30 days by default, like VocaMac).
//!
//! `history.json` keeps the text; `history-audio/<id>.wav` keeps 16 kHz mono
//! audio for the most recent takes only, so disk use stays bounded.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Entries kept in `history.json`, newest first.
const MAX_ENTRIES: usize = 500;
/// Entries that keep their audio. About 32 KB per second of speech.
const MAX_AUDIO: usize = 50;

pub const STATUS_OK: &str = "ok";
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_CANCELLED: &str = "cancelled";

fn default_status() -> String {
    STATUS_OK.into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: u128,
    pub text: String,
    pub model_id: String,
    pub created_at_ms: u128,
    /// File name inside `history-audio`, while the audio is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_file: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
    /// `ok`, `pending` (transcribing), `failed`, or `cancelled`.
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct HistoryStore {
    path: PathBuf,
    audio_dir: PathBuf,
    lock: Mutex<()>,
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl HistoryStore {
    pub fn new(path: PathBuf, audio_dir: PathBuf) -> Self {
        Self {
            path,
            audio_dir,
            lock: Mutex::new(()),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn read(&self) -> Vec<HistoryEntry> {
        self.read_checked().unwrap_or_default()
    }

    /// `Err` when `history.json` exists but cannot be read or parsed. Callers
    /// that delete audio must not treat that as "no entries".
    fn read_checked(&self) -> Result<Vec<HistoryEntry>, String> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|error| format!("history.json is unreadable: {error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(format!("Could not read history: {error}")),
        }
    }

    fn write(&self, entries: &[HistoryEntry]) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or("History path has no parent directory")?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create history directory: {error}"))?;
        let serialized = serde_json::to_vec_pretty(entries)
            .map_err(|error| format!("Could not save history: {error}"))?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, serialized)
            .map_err(|error| format!("Could not save history: {error}"))?;
        // `fs::rename` replaces the old file in one step on Windows too
        // (MoveFileEx with REPLACE_EXISTING), so there is never a moment
        // with no history.json on disk.
        fs::rename(&temporary, &self.path)
            .map_err(|error| format!("Could not save history: {error}"))
    }

    pub fn load(&self) -> Vec<HistoryEntry> {
        let _guard = self.guard();
        self.read()
    }

    pub fn entry(&self, id: u128) -> Option<HistoryEntry> {
        self.load().into_iter().find(|entry| entry.id == id)
    }

    /// The newest take that typed something.
    pub fn latest_text(&self) -> Option<String> {
        self.load()
            .into_iter()
            .find(|entry| entry.status == STATUS_OK && !entry.text.trim().is_empty())
            .map(|entry| entry.text)
    }

    pub fn audio_path(&self, id: u128) -> Option<PathBuf> {
        let entry = self.entry(id)?;
        let path = self.audio_dir.join(entry.audio_file?);
        path.is_file().then_some(path)
    }

    fn next_id(entries: &[HistoryEntry]) -> u128 {
        let now = now_ms();
        entries
            .iter()
            .map(|entry| entry.id + 1)
            .max()
            .map_or(now, |next| next.max(now))
    }

    /// Save the audio and a pending entry before transcription starts.
    pub fn begin(&self, pcm16k: &[f32], model_id: &str, keep_audio: bool) -> Result<u128, String> {
        let _guard = self.guard();
        let mut entries = self.read();
        let id = Self::next_id(&entries);
        let audio_file = if keep_audio {
            let name = format!("{id}.wav");
            fs::create_dir_all(&self.audio_dir)
                .map_err(|error| format!("Could not create history audio folder: {error}"))?;
            write_wav(&self.audio_dir.join(&name), pcm16k)?;
            Some(name)
        } else {
            None
        };
        entries.insert(
            0,
            HistoryEntry {
                id,
                text: String::new(),
                model_id: model_id.to_string(),
                created_at_ms: now_ms(),
                audio_file,
                duration_ms: (pcm16k.len() as u64 * 1000) / 16_000,
                status: STATUS_PENDING.into(),
                error: None,
            },
        );
        self.bound(&mut entries);
        self.write(&entries)?;
        Ok(id)
    }

    /// Record the outcome of a take. An `ok` take with no text had nothing
    /// in it, so the entry and its audio go.
    pub fn finish(
        &self,
        id: u128,
        text: &str,
        model_id: &str,
        status: &str,
        error: Option<String>,
    ) -> Result<(), String> {
        let _guard = self.guard();
        let mut entries = self.read();
        let Some(position) = entries.iter().position(|entry| entry.id == id) else {
            return Ok(());
        };
        if status == STATUS_OK && text.trim().is_empty() {
            let removed = entries.remove(position);
            self.remove_audio(&removed);
        } else {
            let entry = &mut entries[position];
            entry.text = text.trim().to_string();
            entry.model_id = model_id.to_string();
            entry.status = status.to_string();
            entry.error = error;
        }
        self.write(&entries)
    }

    /// Changes the status and drops the error the old status carried: a
    /// cancelled take is no longer a failure.
    pub fn set_status(&self, id: u128, status: &str) -> Result<(), String> {
        let _guard = self.guard();
        let mut entries = self.read();
        let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
            return Ok(());
        };
        entry.status = status.to_string();
        entry.error = None;
        self.write(&entries)
    }

    pub fn delete(&self, id: u128) -> Result<(), String> {
        let _guard = self.guard();
        let mut entries = self.read();
        let Some(position) = entries.iter().position(|entry| entry.id == id) else {
            return Ok(());
        };
        let removed = entries.remove(position);
        self.remove_audio(&removed);
        self.write(&entries)
    }

    pub fn clear(&self) -> Result<(), String> {
        let _guard = self.guard();
        if self.path.exists() {
            fs::remove_file(&self.path)
                .map_err(|error| format!("Could not clear history: {error}"))?;
        }
        if self.audio_dir.exists() {
            fs::remove_dir_all(&self.audio_dir)
                .map_err(|error| format!("Could not clear history audio: {error}"))?;
        }
        Ok(())
    }

    /// Drop entries older than `retention_days` (0 keeps them) and bound
    /// the list and the audio kept.
    pub fn prune(&self, retention_days: u32) -> Result<(), String> {
        let _guard = self.guard();
        // An unreadable index must not look like an empty one: that would
        // delete every saved recording as an orphan.
        let mut entries = self.read_checked()?;
        let before = entries.clone();
        if retention_days > 0 {
            let cutoff = now_ms().saturating_sub(retention_days as u128 * 86_400_000);
            let (keep, expired): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .partition(|entry| entry.created_at_ms >= cutoff);
            for entry in &expired {
                self.remove_audio(entry);
            }
            entries = keep;
        }
        self.bound(&mut entries);
        self.remove_orphan_audio(&entries);
        if entries != before {
            self.write(&entries)?;
        }
        Ok(())
    }

    /// Takes still pending at launch never finished: VocaWin closed or
    /// crashed mid-transcription. Their audio is kept for a retry.
    pub fn recover_pending(&self) -> Result<(), String> {
        let _guard = self.guard();
        let mut entries = self.read();
        let mut changed = false;
        for entry in entries.iter_mut().filter(|e| e.status == STATUS_PENDING) {
            entry.status = STATUS_FAILED.into();
            entry.error = Some("VocaWin closed before this take finished. Retry it.".into());
            changed = true;
        }
        if changed {
            self.write(&entries)?;
        }
        Ok(())
    }

    fn bound(&self, entries: &mut Vec<HistoryEntry>) {
        for entry in entries.iter().skip(MAX_ENTRIES) {
            self.remove_audio(entry);
        }
        entries.truncate(MAX_ENTRIES);
        for entry in entries.iter_mut().skip(MAX_AUDIO) {
            if entry.audio_file.is_some() {
                self.remove_audio(entry);
                entry.audio_file = None;
            }
        }
    }

    fn remove_audio(&self, entry: &HistoryEntry) {
        if let Some(name) = &entry.audio_file {
            let _ = fs::remove_file(self.audio_dir.join(name));
        }
    }

    /// Audio files no entry points at (an entry deleted mid-write).
    fn remove_orphan_audio(&self, entries: &[HistoryEntry]) {
        let Ok(files) = fs::read_dir(&self.audio_dir) else {
            return;
        };
        let known: std::collections::HashSet<&str> = entries
            .iter()
            .filter_map(|entry| entry.audio_file.as_deref())
            .collect();
        for file in files.flatten() {
            let name = file.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".wav") && !known.contains(name.as_ref()) {
                let _ = fs::remove_file(file.path());
            }
        }
    }
}

pub fn write_wav(path: &Path, pcm16k: &[f32]) -> Result<(), String> {
    let specification = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, specification)
        .map_err(|error| format!("Could not save dictation audio: {error}"))?;
    for sample in pcm16k {
        writer
            .write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .map_err(|error| format!("Could not save dictation audio: {error}"))?;
    }
    writer
        .finalize()
        .map_err(|error| format!("Could not save dictation audio: {error}"))
}

/// 16 kHz mono audio written by `write_wav`.
pub fn read_wav(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|error| format!("Could not open saved audio: {error}"))?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.sample_rate != 16_000 || spec.bits_per_sample != 16 {
        return Err("Saved audio is not 16 kHz mono.".into());
    }
    reader
        .samples::<i16>()
        .map(|sample| {
            sample
                .map(|value| value as f32 / i16::MAX as f32)
                .map_err(|error| format!("Could not read saved audio: {error}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, HistoryStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(
            directory.path().join("history.json"),
            directory.path().join("history-audio"),
        );
        (directory, store)
    }

    #[test]
    fn audio_is_saved_before_the_text_arrives() {
        let (_directory, store) = store();
        let id = store
            .begin(&vec![0.1; 16_000], "whisper-tiny", true)
            .unwrap();
        let pending = store.entry(id).unwrap();
        assert_eq!(pending.status, STATUS_PENDING);
        assert_eq!(pending.duration_ms, 1000);
        let audio = store.audio_path(id).unwrap();
        assert_eq!(read_wav(&audio).unwrap().len(), 16_000);

        store
            .finish(id, " Hello there ", "whisper-tiny", STATUS_OK, None)
            .unwrap();
        let done = store.entry(id).unwrap();
        assert_eq!(done.text, "Hello there");
        assert_eq!(done.status, STATUS_OK);
        assert_eq!(store.latest_text().as_deref(), Some("Hello there"));
    }

    #[test]
    fn an_empty_take_leaves_nothing_behind() {
        let (_directory, store) = store();
        let id = store.begin(&[0.0; 8_000], "whisper-tiny", true).unwrap();
        let audio = store.audio_path(id).unwrap();
        store
            .finish(id, "", "whisper-tiny", STATUS_OK, None)
            .unwrap();
        assert!(store.entry(id).is_none());
        assert!(!audio.exists());
    }

    #[test]
    fn a_cancelled_failure_loses_its_error() {
        let (_directory, store) = store();
        let id = store.begin(&[0.0; 8_000], "whisper-tiny", true).unwrap();
        store
            .finish(id, "", "whisper-tiny", STATUS_FAILED, Some("No speech".into()))
            .unwrap();
        store.set_status(id, STATUS_CANCELLED).unwrap();
        let entry = store.entry(id).unwrap();
        assert_eq!(entry.status, STATUS_CANCELLED);
        assert_eq!(entry.error, None);
    }

    #[test]
    fn a_crash_mid_take_becomes_a_retryable_failure() {
        let (_directory, store) = store();
        let id = store.begin(&[0.0; 8_000], "whisper-tiny", true).unwrap();
        store.recover_pending().unwrap();
        let entry = store.entry(id).unwrap();
        assert_eq!(entry.status, STATUS_FAILED);
        assert!(entry.error.unwrap().contains("Retry"));
        assert!(store.audio_path(id).is_some());
    }

    #[test]
    fn retention_expires_old_entries_and_their_audio() {
        let (_directory, store) = store();
        let id = store.begin(&[0.0; 8_000], "whisper-tiny", true).unwrap();
        store
            .finish(id, "old", "whisper-tiny", STATUS_OK, None)
            .unwrap();
        let mut entries = store.load();
        entries[0].created_at_ms = now_ms() - 3 * 86_400_000;
        store.write(&entries).unwrap();
        let audio = store.audio_path(id).unwrap();

        store.prune(0).unwrap();
        assert!(store.entry(id).is_some(), "forever keeps it");
        store.prune(7).unwrap();
        assert!(store.entry(id).is_some(), "younger than a week");
        store.prune(1).unwrap();
        assert!(store.entry(id).is_none());
        assert!(!audio.exists());
    }

    #[test]
    fn old_entries_written_before_audio_still_load() {
        let (_directory, store) = store();
        fs::write(
            &store.path,
            r#"[{"id":1,"text":"hi","modelId":"whisper-tiny","createdAtMs":1}]"#,
        )
        .unwrap();
        let entry = &store.load()[0];
        assert_eq!(entry.status, STATUS_OK);
        assert!(entry.audio_file.is_none());
    }

    #[test]
    fn an_unreadable_index_keeps_its_audio() {
        let (_directory, store) = store();
        let id = store.begin(&[0.0; 8_000], "m", true).unwrap();
        let audio = store.audio_path(id).unwrap();
        fs::write(&store.path, "{ not json").unwrap();
        assert!(store.prune(30).is_err());
        assert!(audio.exists(), "audio must survive a corrupt index");
    }

    #[test]
    fn saving_replaces_the_index_in_place() {
        let (_directory, store) = store();
        let first = store.begin(&[0.0; 8_000], "m", false).unwrap();
        let second = store.begin(&[0.0; 8_000], "m", false).unwrap();
        assert_eq!(store.load().len(), 2);
        assert!(store.entry(first).is_some() && store.entry(second).is_some());
        assert!(!store.path.with_extension("json.tmp").exists());
    }

    #[test]
    fn delete_and_clear_remove_audio() {
        let (_directory, store) = store();
        let first = store.begin(&[0.0; 8_000], "m", true).unwrap();
        let second = store.begin(&[0.0; 8_000], "m", true).unwrap();
        assert_ne!(first, second);
        let first_audio = store.audio_path(first).unwrap();
        store.delete(first).unwrap();
        assert!(!first_audio.exists());
        store.clear().unwrap();
        assert!(store.load().is_empty());
        assert!(!store.audio_dir.exists());
    }
}
