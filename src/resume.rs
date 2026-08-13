//! Resume-after-update marker. Just before the auto-updater swaps the exe and restarts,
//! the old instance drops a small JSON file naming the sequence/queue it interrupted; the
//! new instance consumes the file on launch and starts the same playback, so a fleet-wide
//! rollout never leaves a VM idle. The marker lives next to config.json and is deleted the
//! moment it is read — a marker that was never consumed (crash, VM rebooted later) expires
//! by timestamp instead of surprising the user with playback days after the fact.

use crate::player::{self, PlaybackSource};
use crate::win32_helpers::lock_or_recover;
use crate::{gui, storage};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A marker older than this is dead: the relaunch it was written for evidently never
/// consumed it, and auto-starting playback on some unrelated later launch is worse than
/// making the user press Play once.
const MAX_AGE_SECS: u64 = 600;

#[derive(Serialize, Deserialize)]
pub struct ResumeMarker {
    pub saved_at_unix: u64,
    /// Version that wrote the marker, for the log line only.
    pub from_version: String,
    pub kind: String,       // "sequence" | "queue"
    pub name: String,       // sequence name ("" for a queue)
    pub queue: Vec<String>, // queue names ([] for a sequence)
}

impl ResumeMarker {
    pub fn to_source(&self) -> PlaybackSource {
        match self.kind.as_str() {
            "sequence" => PlaybackSource::Sequence(self.name.clone()),
            "queue" => PlaybackSource::Queue(self.queue.clone()),
            _ => PlaybackSource::Adhoc,
        }
    }
}

fn marker_path() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("ranify2").join("resume.json")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record what the update is about to interrupt. Adhoc playback (unnamed events,
/// proximity one-shots) has no on-disk identity to reload, so it writes nothing.
pub fn write_marker(source: &PlaybackSource) {
    let (kind, name, queue) = match source {
        PlaybackSource::Sequence(name) => ("sequence", name.clone(), Vec::new()),
        PlaybackSource::Queue(names) => ("queue", String::new(), names.clone()),
        PlaybackSource::Adhoc => return,
    };
    let marker = ResumeMarker {
        saved_at_unix: now_unix(),
        from_version: crate::update::current_version().to_string(),
        kind: kind.to_string(),
        name,
        queue,
    };
    match serde_json::to_string_pretty(&marker) {
        Ok(json) => {
            if let Err(e) = std::fs::write(marker_path(), json) {
                eprintln!("[Cadence] Couldn't write resume marker: {}", e);
            }
        }
        Err(e) => eprintln!("[Cadence] Couldn't serialize resume marker: {}", e),
    }
}

/// Read and delete the marker. None if absent, unreadable, or stale. The file is removed
/// even when the contents are rejected, so a bad marker can't linger.
pub fn take_marker() -> Option<ResumeMarker> {
    let path = marker_path();
    let json = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    parse_marker(&json, now_unix())
}

/// Best-effort delete, for the failed-update path (the restart it was staged for isn't
/// happening, and the local recovery restart takes over).
pub fn clear_marker() {
    let _ = std::fs::remove_file(marker_path());
}

/// Validation split out from `take_marker` so it is testable without touching %APPDATA%.
fn parse_marker(json: &str, now: u64) -> Option<ResumeMarker> {
    let marker: ResumeMarker = serde_json::from_str(json).ok()?;
    let age = now.saturating_sub(marker.saved_at_unix);
    if age > MAX_AGE_SECS {
        println!("[Cadence] Ignoring stale resume marker ({} s old).", age);
        return None;
    }
    Some(marker)
}

/// Start playback from a source. Shared by the post-update startup resume and the
/// failed-update recovery in the toolbar, so the load-names → set_source → play logic
/// exists once. Safe to call from any thread (network.rs already plays from its
/// listener thread).
pub fn start_from(source: &PlaybackSource) {
    match source {
        PlaybackSource::Sequence(name) => match storage::load_sequence(name) {
            Ok(seq) => {
                player::set_source(PlaybackSource::Sequence(name.clone()));
                player::play_sequence(seq.events);
            }
            Err(e) => eprintln!("[Cadence] Resume: can't load sequence '{}': {}", name, e),
        },
        PlaybackSource::Queue(names) => {
            let mut event_lists = Vec::new();
            for name in names {
                if let Ok(seq) = storage::load_sequence(name) {
                    event_lists.push(seq.events);
                }
            }
            if event_lists.is_empty() {
                eprintln!("[Cadence] Resume: queue has no loadable sequences.");
                return;
            }
            // Restore the in-memory queue so the toolbar's Play button plays the same
            // thing the user sees, exactly as before the restart.
            *lock_or_recover(&gui::SEQUENCE_QUEUE) = names.clone();
            player::set_source(PlaybackSource::Queue(names.clone()));
            player::play_queue(event_lists);
        }
        PlaybackSource::Adhoc => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_json(saved_at: u64) -> String {
        serde_json::to_string(&ResumeMarker {
            saved_at_unix: saved_at,
            from_version: "3.4.0".to_string(),
            kind: "sequence".to_string(),
            name: "farm loop".to_string(),
            queue: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn fresh_marker_round_trips() {
        let m = parse_marker(&marker_json(1_000_000), 1_000_100).expect("fresh marker accepted");
        assert_eq!(m.kind, "sequence");
        assert_eq!(m.name, "farm loop");
        assert!(matches!(m.to_source(), PlaybackSource::Sequence(n) if n == "farm loop"));
    }

    #[test]
    fn stale_marker_is_rejected() {
        assert!(parse_marker(&marker_json(1_000_000), 1_000_000 + MAX_AGE_SECS + 1).is_none());
        // Exactly at the limit still counts — the boundary belongs to the fresh side.
        assert!(parse_marker(&marker_json(1_000_000), 1_000_000 + MAX_AGE_SECS).is_some());
    }

    #[test]
    fn garbage_and_future_kinds_are_harmless() {
        assert!(parse_marker("not json", 0).is_none());
        let m = parse_marker(
            &serde_json::json!({
                "saved_at_unix": 50, "from_version": "9.9.9",
                "kind": "hologram", "name": "", "queue": []
            })
            .to_string(),
            60,
        )
        .expect("unknown kind still parses");
        assert!(matches!(m.to_source(), PlaybackSource::Adhoc));
    }

    #[test]
    fn queue_marker_maps_to_queue_source() {
        let json = serde_json::to_string(&ResumeMarker {
            saved_at_unix: 10,
            from_version: "3.4.0".into(),
            kind: "queue".into(),
            name: String::new(),
            queue: vec!["a".into(), "b".into()],
        })
        .unwrap();
        let m = parse_marker(&json, 10).unwrap();
        assert!(matches!(m.to_source(), PlaybackSource::Queue(q) if q == vec!["a", "b"]));
    }
}
