use crate::sequence::{SavedQueue, Sequence};
use crate::win32_helpers::lock_or_recover;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// Cadence's data folder (`%APPDATA%\ranify2`, name kept from the old product name).
pub fn data_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("ranify2")
}

fn ensured_subdir(name: &str) -> PathBuf {
    let dir = data_dir().join(name);
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[Cadence] Failed to create {} dir: {}", name, e);
    }
    dir
}

fn sequences_dir() -> PathBuf {
    ensured_subdir("sequences")
}

fn queues_dir() -> PathBuf {
    ensured_subdir("queues")
}

/// Sorted file stems of all `*.json` files in `dir` (empty when the dir is missing).
fn list_json_stems(dir: &std::path::Path) -> std::io::Result<Vec<String>> {
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

// ---- Saved queues -------------------------------------------------------------------------

fn saved_queue_path(name: &str) -> PathBuf {
    queues_dir().join(format!("{}.json", sanitize_filename(name)))
}

pub fn list_saved_queues() -> Vec<String> {
    list_json_stems(&queues_dir()).unwrap_or_default()
}

pub fn saved_queue_exists(name: &str) -> bool {
    saved_queue_path(name).exists()
}

pub fn load_saved_queue(name: &str) -> std::io::Result<SavedQueue> {
    let json = fs::read_to_string(saved_queue_path(name))?;
    serde_json::from_str(&json).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write (or overwrite) a saved queue. The stored name is the sanitized stem so that what the
/// list shows is what `load_saved_queue` accepts.
pub fn save_saved_queue(name: &str, items: Vec<String>) -> std::io::Result<SavedQueue> {
    let stem = sanitize_filename(name);
    let q = SavedQueue::new(stem.clone(), items);
    let json = serde_json::to_string_pretty(&q)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(saved_queue_path(&stem), json)?;
    Ok(q)
}

pub fn delete_saved_queue(name: &str) -> std::io::Result<()> {
    fs::remove_file(saved_queue_path(name))
}

/// Replace `old` with `new` in a saved queue's items; true if anything changed.
fn rename_items(items: &mut [String], old: &str, new: &str) -> bool {
    let mut changed = false;
    for item in items.iter_mut() {
        if item == old {
            *item = new.to_string();
            changed = true;
        }
    }
    changed
}

/// A sequence was renamed: keep every saved queue that referenced it pointing at the new name.
pub fn rename_in_saved_queues(old: &str, new: &str) {
    for qname in list_saved_queues() {
        if let Ok(mut q) = load_saved_queue(&qname) {
            if rename_items(&mut q.items, old, new) {
                if let Err(e) = save_saved_queue(&q.name, q.items) {
                    eprintln!("[Cadence] Couldn't update saved queue {}: {}", qname, e);
                }
            }
        }
    }
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
    list_json_stems(&sequences_dir())
}

/// The bits of a sequence the list needs without holding every event list in memory.
#[derive(Clone, Debug, PartialEq)]
pub struct SeqMeta {
    pub name: String,
    pub group: Option<String>,
    pub group_order: u32,
    pub duration_micros: i64,
    pub leading_delay_micros: i64,
}

pub fn list_sequence_meta() -> std::io::Result<Vec<SeqMeta>> {
    let names = list_sequences()?;
    let mut result = Vec::new();
    for name in names {
        let meta = match load_sequence(&name) {
            Ok(seq) => SeqMeta {
                name,
                duration_micros: seq.duration_micros(),
                leading_delay_micros: seq.leading_delay_micros(),
                group: seq.group,
                group_order: seq.group_order,
            },
            Err(_) => SeqMeta { name, group: None, group_order: 0, duration_micros: 0, leading_delay_micros: 0 },
        };
        result.push(meta);
    }
    Ok(result)
}

/// Display/play order inside one group: explicit order first, name breaks ties (so legacy
/// files, all order 0, stay alphabetical).
pub fn sort_group_members(members: &mut [SeqMeta]) {
    members.sort_by(|a, b| a.group_order.cmp(&b.group_order).then_with(|| a.name.cmp(&b.name)));
}

/// Ordered names of one group bucket (`None` = ungrouped).
pub fn group_members(group: Option<&str>) -> Vec<String> {
    let mut members: Vec<SeqMeta> = list_sequence_meta()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.group.as_deref() == group)
        .collect();
    sort_group_members(&mut members);
    members.into_iter().map(|m| m.name).collect()
}

/// Order value that puts a sequence after every current member of `group`.
pub fn next_group_order(group: Option<&str>) -> u32 {
    list_sequence_meta()
        .unwrap_or_default()
        .iter()
        .filter(|m| m.group.as_deref() == group)
        .map(|m| m.group_order + 1)
        .max()
        .unwrap_or(0)
}

/// Persist `names` as the exact order of their group: position becomes `group_order`.
pub fn set_group_order(names_in_order: &[String]) -> std::io::Result<()> {
    for (i, name) in names_in_order.iter().enumerate() {
        let mut seq = load_sequence(name)?;
        if seq.group_order != i as u32 {
            seq.group_order = i as u32;
            save_sequence(&seq)?;
        }
    }
    Ok(())
}

/// First of `base_copy`, `base_copy2`, `base_copy3`, … that `exists` rejects. Only `[A-Za-z0-9_]`
/// is appended so the display name and the sanitized filename stay identical.
pub(crate) fn copy_name(base: &str, exists: impl Fn(&str) -> bool) -> String {
    let first = format!("{}_copy", base);
    if !exists(&first) {
        return first;
    }
    (2u32..)
        .map(|n| format!("{}_copy{}", base, n))
        .find(|candidate| !exists(candidate))
        .expect("unbounded counter")
}

pub fn unique_copy_name(base: &str) -> String {
    let dir = sequences_dir();
    copy_name(base, |candidate| {
        dir.join(format!("{}.json", sanitize_filename(candidate))).exists()
    })
}

/// Copy `src` under `new_name` into `group`, placed after that group's current members.
/// Fails with `AlreadyExists` if the target name is taken (including `src` itself).
pub fn duplicate_sequence(src: &str, new_name: &str, group: Option<String>) -> std::io::Result<()> {
    let target = sequences_dir().join(format!("{}.json", sanitize_filename(new_name)));
    if target.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("A sequence named \"{}\" already exists", new_name),
        ));
    }
    let seq = load_sequence(src)?;
    let order = next_group_order(group.as_deref());
    save_sequence(&seq.copy_as(new_name.to_string(), group, order))?;
    Ok(())
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
    data_dir().join("last_played.json")
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

    #[test]
    fn rename_items_replaces_every_occurrence() {
        let mut items: Vec<String> = ["a", "b", "a", "c"].iter().map(|s| s.to_string()).collect();
        assert!(rename_items(&mut items, "a", "z"));
        assert_eq!(items, ["z", "b", "z", "c"]);
        assert!(!rename_items(&mut items, "missing", "q"));
    }

    #[test]
    fn copy_name_skips_taken_names() {
        assert_eq!(copy_name("farm", |_| false), "farm_copy");
        let taken = ["farm_copy", "farm_copy2"];
        assert_eq!(copy_name("farm", |n| taken.contains(&n)), "farm_copy3");
    }

    #[test]
    fn group_members_sort_by_order_then_name() {
        let m = |name: &str, order: u32| SeqMeta {
            name: name.into(), group: None, group_order: order, duration_micros: 0, leading_delay_micros: 0,
        };
        let mut members = vec![m("zeta", 0), m("alpha", 2), m("beta", 0), m("gamma", 1)];
        sort_group_members(&mut members);
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["beta", "zeta", "gamma", "alpha"]);
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
