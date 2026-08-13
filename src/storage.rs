use crate::sequence::Sequence;
use crate::win32_helpers::lock_or_recover;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

fn sequences_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("ranify2").join("sequences");
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[Cadence] Failed to create sequences dir: {}", e);
    }
    dir
}

pub fn save_sequence(seq: &Sequence) -> std::io::Result<PathBuf> {
    let dir = sequences_dir();
    let filename = sanitize_filename(&seq.name);
    let path = dir.join(format!("{}.json", filename));
    let json = serde_json::to_string_pretty(seq)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn load_sequence(name: &str) -> std::io::Result<Sequence> {
    let dir = sequences_dir();
    let filename = sanitize_filename(name);
    let path = dir.join(format!("{}.json", filename));
    let json = fs::read_to_string(path)?;
    serde_json::from_str(&json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

pub fn list_sequences() -> std::io::Result<Vec<String>> {
    let dir = sequences_dir();
    let mut names = Vec::new();
    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Some(stem) = path.file_stem() {
                    names.push(stem.to_string_lossy().to_string());
                }
            }
        }
    }
    names.sort();
    Ok(names)
}

pub fn list_sequences_with_groups() -> std::io::Result<Vec<(String, Option<String>)>> {
    let names = list_sequences()?;
    let mut result = Vec::new();
    for name in names {
        let group = load_sequence(&name).map(|seq| seq.group).unwrap_or(None);
        result.push((name, group));
    }
    Ok(result)
}

/// Name of the most recently saved/modified sequence (by file mtime) — the one the
/// user just recorded and has probably already forgotten the name of.
pub fn newest_sequence() -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in fs::read_dir(sequences_dir()).ok()?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "json") {
            continue;
        }
        if let (Some(stem), Ok(meta)) = (path.file_stem(), entry.metadata()) {
            if let Ok(modified) = meta.modified() {
                if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                    best = Some((modified, stem.to_string_lossy().to_string()));
                }
            }
        }
    }
    best.map(|(_, name)| name)
}

// --- Last played ------------------------------------------------------------------------------

/// The last named playback (sequence or queue). Persisted in its own small file — deliberately
/// NOT in config.json, which several dialogs rewrite wholesale from long-lived copies and would
/// silently clobber a value that changes on every Play.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct LastPlayed {
    /// Sequence name; empty when a queue was played.
    pub name: String,
    /// Queue names, in order; empty when a single sequence was played.
    pub queue: Vec<String>,
}

impl LastPlayed {
    /// Short human description for the status line, or None if nothing was ever played.
    pub fn describe(&self) -> Option<String> {
        if !self.name.is_empty() {
            Some(self.name.clone())
        } else if !self.queue.is_empty() {
            Some(format!("queue ({}): {}", self.queue.len(), self.queue.join(", ")))
        } else {
            None
        }
    }
}

fn last_played_path() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("ranify2").join("last_played.json")
}

static LAST_PLAYED: Mutex<Option<LastPlayed>> = Mutex::new(None);

/// Cached view of the last named playback; loads from disk once per process.
pub fn last_played() -> LastPlayed {
    lock_or_recover(&LAST_PLAYED)
        .get_or_insert_with(|| {
            fs::read_to_string(last_played_path())
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default()
        })
        .clone()
}

/// Remember a named playback (no-op when unchanged).
pub fn set_last_played(name: String, queue: Vec<String>) {
    let lp = LastPlayed { name, queue };
    if last_played() == lp {
        return;
    }
    match serde_json::to_string_pretty(&lp) {
        Ok(json) => {
            if let Err(e) = fs::write(last_played_path(), json) {
                eprintln!("[Cadence] Couldn't save last-played: {}", e);
            }
        }
        Err(e) => eprintln!("[Cadence] Couldn't serialize last-played: {}", e),
    }
    *lock_or_recover(&LAST_PLAYED) = Some(lp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_played_describes_sequence_queue_and_nothing() {
        let none = LastPlayed::default();
        assert_eq!(none.describe(), None);
        let seq = LastPlayed { name: "farm loop".into(), queue: Vec::new() };
        assert_eq!(seq.describe().as_deref(), Some("farm loop"));
        let q = LastPlayed { name: String::new(), queue: vec!["a".into(), "b".into()] };
        assert_eq!(q.describe().as_deref(), Some("queue (2): a, b"));
    }
}

pub fn delete_sequence(name: &str) -> std::io::Result<()> {
    let dir = sequences_dir();
    let filename = sanitize_filename(name);
    let path = dir.join(format!("{}.json", filename));
    fs::remove_file(path)
}

pub fn rename_sequence(old_filename: &str, new_name: &str) -> std::io::Result<()> {
    let mut seq = load_sequence(old_filename)?;
    let new_sanitized = sanitize_filename(new_name);
    let old_sanitized = sanitize_filename(old_filename);

    if new_sanitized != old_sanitized {
        let dir = sequences_dir();
        let new_path = dir.join(format!("{}.json", new_sanitized));
        if new_path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("A sequence named \"{}\" already exists", new_name),
            ));
        }
    }

    seq.name = new_name.to_string();
    save_sequence(&seq)?;

    if new_sanitized != old_sanitized {
        let dir = sequences_dir();
        let old_path = dir.join(format!("{}.json", old_sanitized));
        if old_path.exists() {
            fs::remove_file(old_path)?;
        }
    }

    Ok(())
}

pub(crate) fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
