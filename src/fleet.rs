//! Fleet control channel: long-poll the dashboard server for the ▶/■ a dashboard
//! user presses on this machine's card, authenticated with the same report token
//! as the heartbeat. Independent of the party relay, so a VM in no room is still
//! controllable — and only by the owner of the token it reports with.
//!
//! Modeled on party.rs: one worker thread, idempotent start(), config re-read
//! every cycle; the server holds each poll up to 25s and answers instantly when a
//! command is published, so latency is a round-trip, not a poll interval.

use crate::player::PlaybackSource;
use crate::win32_helpers::lock_or_recover;
use crate::{config, gui, network, player, report, storage, update};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// Must out-wait the server's 25s hold with margin.
const POLL_RECV_TIMEOUT_MS: i32 = 40_000;
const BACKOFF: [u64; 4] = [2, 5, 15, 30];

static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static CONNECTED: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Clone)]
struct Ack {
    seq: u64,
    ok: bool,
    detail: String,
}

#[derive(Serialize)]
struct PollBody<'a> {
    agent_id: &'a str,
    last_seq: u64,
    last_result: Option<Ack>,
    /// What this machine last played, as a wire command — the dashboard's ▶
    /// resends exactly this, so "resume" means resume.
    resume_cmd: Option<String>,
}

#[derive(Deserialize)]
struct PollResp {
    seq: u64,
    cmd: Option<String>,
}

/// Spawn the fleet-control thread (idempotent). It idles while reporting is
/// disabled or the token is blank, and otherwise long-polls the report server.
pub fn start() {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    CANCEL.store(false, Ordering::Release);
    thread::spawn(run);
}

pub fn stop() {
    CANCEL.store(true, Ordering::Release);
}

#[allow(dead_code)]
pub fn is_connected() -> bool {
    CONNECTED.load(Ordering::Acquire)
}

/// The poll endpoint lives next to the report endpoint: strip `/api/report` from
/// the configured report URL (or take it as an origin) and append `/api/agent/poll`.
pub fn poll_url(report_url: &str) -> String {
    let u = report_url.trim().trim_end_matches('/');
    let origin = u.strip_suffix("/api/report").unwrap_or(u);
    format!("{}/api/agent/poll", origin)
}

/// Adopt the server's seq unconditionally (a lower seq means the server restarted)
/// and only execute a command that moved it. Same rule as party::advance.
fn advance(last_seq: u64, resp_seq: u64, cmd: Option<String>) -> (u64, Option<String>) {
    let run = if resp_seq == last_seq { None } else { cmd };
    (resp_seq, run)
}

/// A label usable as the single-token name slot of PLAY_SAVED / PLAY_LIST.
fn token_label(label: Option<&str>) -> Option<&str> {
    label
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.chars().any(char::is_whitespace))
}

fn list_cmd(label: Option<&str>, names: &[String]) -> String {
    match token_label(label) {
        // The host's own saved queue of that name wins; the list is the fallback.
        Some(l) => format!("PLAY_SAVED {} {}", l, names.join(" ")),
        None => format!("PLAY_LIST resume {}", names.join(" ")),
    }
}

/// The wire command that reproduces what this machine is playing / last played:
/// the live source first (with the queue's saved-queue label when it has one),
/// else the on-disk last-played record, else nothing (never played a named thing).
pub fn resume_cmd_from(source: &PlaybackSource, label: Option<&str>, last: &storage::LastPlayed) -> Option<String> {
    match source {
        PlaybackSource::Sequence(n) if !n.trim().is_empty() => return Some(format!("PLAY {}", n.trim())),
        PlaybackSource::Queue(names) if !names.is_empty() => return Some(list_cmd(label, names)),
        _ => {}
    }
    if !last.queue.is_empty() {
        return Some(list_cmd(Some(&last.name), &last.queue));
    }
    if !last.name.trim().is_empty() {
        return Some(format!("PLAY {}", last.name.trim()));
    }
    None
}

fn sleep_cancellable(secs: u64) {
    for _ in 0..secs {
        if CANCEL.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn run() {
    let agent_id = report::machine_name();
    let mut last_seq: u64 = 0;
    let mut pending_ack: Option<Ack> = None;
    let mut failures: usize = 0;
    loop {
        if CANCEL.load(Ordering::Acquire) {
            break;
        }
        let cfg = config::cached_config();
        let token = report::normalize_token(&cfg.report_token);
        if !cfg.report_enabled || cfg.report_url.trim().is_empty() || token.is_empty() {
            CONNECTED.store(false, Ordering::Release);
            last_seq = 0;
            pending_ack = None;
            failures = 0;
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        let label = lock_or_recover(&gui::QUEUE_LABEL).clone();
        let resume = resume_cmd_from(&player::current_source(), label.as_deref(), &storage::last_played());
        let body = PollBody {
            agent_id: &agent_id,
            last_seq,
            // Clone, not take: if this poll fails, the ack rides the next one.
            last_result: pending_ack.clone(),
            resume_cmd: resume,
        };
        let json = match serde_json::to_vec(&body) {
            Ok(j) => j,
            Err(_) => {
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let url = poll_url(&cfg.report_url);
        match update::https_post_json_token_timeout(&url, &token, &json, POLL_RECV_TIMEOUT_MS) {
            Ok((200, resp)) => match serde_json::from_slice::<PollResp>(&resp) {
                Ok(pr) => {
                    failures = 0;
                    pending_ack = None;
                    CONNECTED.store(true, Ordering::Release);
                    let (next, cmd) = advance(last_seq, pr.seq, pr.cmd);
                    last_seq = next;
                    if let Some(cmd) = cmd {
                        // Same path as a LAN remote / party command: recording gate,
                        // stop-and-override takeover, local name resolution.
                        let result = network::execute_command(&cmd).trim().to_string();
                        println!("[Cadence] Dashboard command: {} -> {}", cmd, result);
                        pending_ack = Some(Ack { seq: next, ok: result.starts_with("OK"), detail: result });
                    }
                    // No sleep: the server's hold paces this loop.
                }
                Err(_) => {
                    CONNECTED.store(false, Ordering::Release);
                    let b = BACKOFF[failures.min(BACKOFF.len() - 1)];
                    failures += 1;
                    sleep_cancellable(b);
                }
            },
            // Old server (no such route) or a bad token: slow retry, no log spam.
            Ok((401, _)) | Ok((403, _)) | Ok((404, _)) => {
                CONNECTED.store(false, Ordering::Release);
                sleep_cancellable(30);
            }
            Ok((_, _)) | Err(_) => {
                CONNECTED.store(false, Ordering::Release);
                let b = BACKOFF[failures.min(BACKOFF.len() - 1)];
                failures += 1;
                sleep_cancellable(b);
            }
        }
    }
    CONNECTED.store(false, Ordering::Release);
    RUNNING.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_url_from_report_url() {
        assert_eq!(poll_url("https://x.io/api/report"), "https://x.io/api/agent/poll");
        assert_eq!(poll_url(" https://x.io/api/report/ "), "https://x.io/api/agent/poll");
        assert_eq!(poll_url("https://x.io"), "https://x.io/api/agent/poll");
        assert_eq!(poll_url("https://x.io/"), "https://x.io/api/agent/poll");
    }

    #[test]
    fn advance_adopts_and_gates() {
        assert_eq!(advance(4, 5, Some("STOP".into())), (5, Some("STOP".into())));
        assert_eq!(advance(4, 4, None), (4, None));
        assert_eq!(advance(9, 0, None), (0, None)); // server restart: adopt, never replay
        assert_eq!(advance(5, 5, Some("STOP".into())), (5, None)); // same seq: no re-run
    }

    #[test]
    fn resume_cmd_prefers_live_source_then_disk() {
        let none = storage::LastPlayed::default();
        let seq = PlaybackSource::Sequence("raid1".into());
        assert_eq!(resume_cmd_from(&seq, None, &none).as_deref(), Some("PLAY raid1"));
        let q = PlaybackSource::Queue(vec!["a".into(), "b".into()]);
        assert_eq!(resume_cmd_from(&q, Some("farm"), &none).as_deref(), Some("PLAY_SAVED farm a b"));
        assert_eq!(resume_cmd_from(&q, Some("two words"), &none).as_deref(), Some("PLAY_LIST resume a b"));
        assert_eq!(resume_cmd_from(&q, None, &none).as_deref(), Some("PLAY_LIST resume a b"));
        // Adhoc / idle: fall back to the on-disk record.
        let disk_seq = storage::LastPlayed { name: "boss".into(), queue: vec![] };
        assert_eq!(resume_cmd_from(&PlaybackSource::Adhoc, None, &disk_seq).as_deref(), Some("PLAY boss"));
        let disk_q = storage::LastPlayed { name: "farm".into(), queue: vec!["a".into()] };
        assert_eq!(resume_cmd_from(&PlaybackSource::Adhoc, None, &disk_q).as_deref(), Some("PLAY_SAVED farm a"));
        assert_eq!(resume_cmd_from(&PlaybackSource::Adhoc, None, &none), None);
        let empty_q = PlaybackSource::Queue(vec![]);
        assert_eq!(resume_cmd_from(&empty_q, None, &none), None);
    }
}
