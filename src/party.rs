//! Internet party: long-poll the fleet server's party relay and mirror the LAN
//! remote protocol across it. Members in different places join a named room with
//! a passkey; a sender's command (the same wire strings the LAN remote uses) is
//! relayed to every receiver, which resolves the names against its OWN sequence
//! files — so "PLAY raid1" plays each member's local raid1.
//!
//! Modeled on report.rs: one worker thread, idempotent start(), and the config is
//! re-read from `config::cached_config` every cycle so the Remote dialog's
//! Connect/role changes apply without a restart. The long poll itself paces the
//! loop — the server holds the request up to 25s and answers instantly when a
//! command arrives, so delivery latency is network round-trip, not poll interval.

use crate::{config, network, report, update};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

/// Must out-wait the server's 25s hold with margin.
const POLL_RECV_TIMEOUT_MS: i32 = 40_000;
const SEND_RECV_TIMEOUT_MS: i32 = 15_000;
/// Consecutive-failure backoff steps, seconds.
const BACKOFF: [u64; 4] = [2, 5, 15, 30];

static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static CONNECTED: AtomicBool = AtomicBool::new(false);
/// Bumped whenever MEMBERS changes, so the dialog's timer only rebuilds its
/// listbox when there is something new (no half-second flicker).
static GEN: AtomicU64 = AtomicU64::new(0);
static STATUS: Mutex<String> = Mutex::new(String::new());
static LAST_SEND: Mutex<String> = Mutex::new(String::new());
static MEMBERS: Mutex<Vec<MemberSnap>> = Mutex::new(Vec::new());

#[derive(Clone, PartialEq)]
pub struct MemberSnap {
    pub name: String,
    pub send: bool,
    pub recv: bool,
    pub online: bool,
    /// (ok, detail) of the member's last executed command, e.g. (false, "ERR not_found").
    pub last_result: Option<(bool, String)>,
}

#[derive(Serialize, Clone)]
struct Ack {
    seq: u64,
    ok: bool,
    detail: String,
}

#[derive(Serialize)]
struct PollBody<'a> {
    room: &'a str,
    passkey: &'a str,
    agent_id: &'a str,
    member: &'a str,
    send: bool,
    recv: bool,
    last_seq: u64,
    last_result: Option<Ack>,
}

#[derive(Deserialize)]
struct WireResult {
    ok: bool,
    #[serde(default)]
    detail: String,
}

#[derive(Deserialize)]
struct WireMember {
    name: String,
    #[serde(default)]
    send: bool,
    #[serde(default)]
    recv: bool,
    #[serde(default)]
    online: bool,
    #[serde(default)]
    last_result: Option<WireResult>,
}

#[derive(Deserialize)]
struct PollResp {
    seq: u64,
    cmd: Option<String>,
    #[serde(default)]
    members: Vec<WireMember>,
}

/// Spawn the party thread (idempotent). It idles cheaply until the config says
/// party_enabled with a room and passkey, then joins and long-polls.
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

pub fn is_connected() -> bool {
    CONNECTED.load(Ordering::Acquire)
}

/// Whether outgoing commands (remote hotkeys, the Remote dialog's Send buttons)
/// should also go to the party. Config-only on purpose: a send can succeed even
/// while the poll thread is mid-backoff, so it isn't gated on is_connected().
pub fn sender_active() -> bool {
    let cfg = config::cached_config();
    cfg.party_enabled
        && cfg.party_send
        && !cfg.party_room.trim().is_empty()
        && !cfg.party_passkey.trim().is_empty()
        && !cfg.party_url.trim().is_empty()
}

/// Fire-and-forget broadcast of one wire command to the room.
pub fn send(cmd: &str) {
    if !sender_active() {
        return;
    }
    let cfg = config::cached_config();
    let cmd = cmd.trim().to_string();
    thread::spawn(move || {
        let agent_id = report::machine_name();
        let body = serde_json::json!({
            "room": cfg.party_room.trim(),
            "passkey": cfg.party_passkey.trim(),
            "agent_id": agent_id,
            "member": display_name(&cfg.report_label, &agent_id),
            "cmd": cmd,
        });
        let url = api_url(&cfg.party_url, "/api/party/send");
        let json = serde_json::to_vec(&body).unwrap_or_default();
        let note = match update::https_post_json_timeout(&url, &json, SEND_RECV_TIMEOUT_MS) {
            Ok((200, resp)) => {
                let n = serde_json::from_slice::<serde_json::Value>(&resp)
                    .ok()
                    .and_then(|v| v["receivers"].as_u64())
                    .unwrap_or(0);
                format!("sent to {} receiver(s)", n)
            }
            Ok((403, _)) => "send failed: wrong passkey".to_string(),
            Ok((status, _)) => format!("send failed (HTTP {})", status),
            Err(e) => format!("send failed ({})", e),
        };
        *LAST_SEND.lock().unwrap() = note;
    });
}

/// One line for the Remote dialog's status static.
pub fn status_line() -> String {
    let conn = STATUS.lock().unwrap().clone();
    let sent = LAST_SEND.lock().unwrap().clone();
    match (conn.is_empty(), sent.is_empty()) {
        (true, true) => String::new(),
        (false, true) => conn,
        (true, false) => sent,
        (false, false) => format!("{} · {}", conn, sent),
    }
}

/// Member rows for the dialog, plus a generation stamp: repaint only when it moves.
pub fn members_snapshot() -> (u64, Vec<MemberSnap>) {
    let rows = MEMBERS.lock().unwrap().clone();
    (GEN.load(Ordering::Acquire), rows)
}

/// Listbox row: name, roles, and how the member's last command went.
pub fn format_member(m: &MemberSnap) -> String {
    let roles = match (m.send, m.recv) {
        (true, true) => "[S+R]",
        (true, false) => "[S]",
        (false, true) => "[R]",
        (false, false) => "[-]",
    };
    let tail = if !m.online {
        " — offline".to_string()
    } else {
        match &m.last_result {
            Some((_, detail)) => format!(" — {}", detail),
            None => String::new(),
        }
    };
    format!("{} {}{}", m.name, roles, tail)
}

/// Adopt the server's seq unconditionally (a lower seq means the server restarted
/// and the room was rebuilt) and only execute a command that moved it.
fn advance(last_seq: u64, resp_seq: u64, cmd: Option<String>) -> (u64, Option<String>) {
    let run = if resp_seq == last_seq { None } else { cmd };
    (resp_seq, run)
}

fn api_url(origin: &str, path: &str) -> String {
    format!("{}{}", origin.trim().trim_end_matches('/'), path)
}

fn display_name(label: &str, agent_id: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        agent_id.to_string()
    } else {
        label.to_string()
    }
}

fn set_status(s: &str) {
    *STATUS.lock().unwrap() = s.to_string();
}

fn set_members(rows: Vec<MemberSnap>) {
    let mut cur = MEMBERS.lock().unwrap();
    if *cur != rows {
        *cur = rows;
        GEN.fetch_add(1, Ordering::AcqRel);
    }
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
    let mut was_active = false;
    loop {
        if CANCEL.load(Ordering::Acquire) {
            break;
        }
        let cfg = config::cached_config();
        let active = cfg.party_enabled
            && !cfg.party_room.trim().is_empty()
            && !cfg.party_passkey.trim().is_empty()
            && !cfg.party_url.trim().is_empty();
        if !active {
            if was_active {
                was_active = false;
                CONNECTED.store(false, Ordering::Release);
                set_members(Vec::new());
                set_status("");
                LAST_SEND.lock().unwrap().clear();
                last_seq = 0;
                pending_ack = None;
                failures = 0;
            }
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        was_active = true;
        let member = display_name(&cfg.report_label, &agent_id);
        let body = PollBody {
            room: cfg.party_room.trim(),
            passkey: cfg.party_passkey.trim(),
            agent_id: &agent_id,
            member: &member,
            send: cfg.party_send,
            recv: cfg.party_receive,
            last_seq,
            // Clone, not take: if this poll fails, the ack rides the next one.
            last_result: pending_ack.clone(),
        };
        let json = match serde_json::to_vec(&body) {
            Ok(j) => j,
            Err(_) => {
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let url = api_url(&cfg.party_url, "/api/party/poll");
        match update::https_post_json_timeout(&url, &json, POLL_RECV_TIMEOUT_MS) {
            Ok((200, resp)) => match serde_json::from_slice::<PollResp>(&resp) {
                Ok(pr) => {
                    failures = 0;
                    pending_ack = None;
                    CONNECTED.store(true, Ordering::Release);
                    let online = pr.members.iter().filter(|m| m.online).count();
                    set_members(
                        pr.members
                            .into_iter()
                            .map(|m| MemberSnap {
                                name: m.name,
                                send: m.send,
                                recv: m.recv,
                                online: m.online,
                                last_result: m.last_result.map(|r| (r.ok, r.detail)),
                            })
                            .collect(),
                    );
                    set_status(&format!("Connected — {} member(s) online", online));
                    let (next, cmd) = advance(last_seq, pr.seq, pr.cmd);
                    last_seq = next;
                    if let Some(cmd) = cmd {
                        // Same path as a LAN remote command: recording gate,
                        // stop-and-override takeover, local name resolution.
                        let result = network::execute_command(&cmd).trim().to_string();
                        println!("[Cadence] Party command: {} -> {}", cmd, result);
                        pending_ack = Some(Ack {
                            seq: next,
                            ok: result.starts_with("OK"),
                            detail: result,
                        });
                    }
                    // No sleep: the server's 25s hold paces this loop.
                }
                Err(_) => {
                    CONNECTED.store(false, Ordering::Release);
                    set_status("Bad server response");
                    let b = BACKOFF[failures.min(BACKOFF.len() - 1)];
                    failures += 1;
                    sleep_cancellable(b);
                }
            },
            Ok((403, _)) => {
                CONNECTED.store(false, Ordering::Release);
                set_members(Vec::new());
                set_status("Wrong passkey");
                // Slow retry so a corrected passkey heals without reconnect ceremony.
                sleep_cancellable(15);
            }
            Ok((status, _)) => {
                CONNECTED.store(false, Ordering::Release);
                set_status(&format!("Server error (HTTP {})", status));
                let b = BACKOFF[failures.min(BACKOFF.len() - 1)];
                failures += 1;
                sleep_cancellable(b);
            }
            Err(_) => {
                CONNECTED.store(false, Ordering::Release);
                set_status("Offline — retrying");
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
    fn advance_adopts_and_gates() {
        // Normal delivery: seq moved forward with a command.
        assert_eq!(
            advance(4, 5, Some("PLAY raid1".into())),
            (5, Some("PLAY raid1".into()))
        );
        // Idle poll: adopt seq, nothing to run.
        assert_eq!(advance(4, 4, None), (4, None));
        // Server restart: seq went backwards — adopt it, never replay.
        assert_eq!(advance(9, 0, None), (0, None));
        assert_eq!(advance(9, 1, Some("STOP".into())), (1, Some("STOP".into())));
        // Same seq with a command attached must not re-execute.
        assert_eq!(advance(5, 5, Some("STOP".into())), (5, None));
    }

    #[test]
    fn api_url_joins_cleanly() {
        assert_eq!(api_url("https://x.io", "/api/party/poll"), "https://x.io/api/party/poll");
        assert_eq!(api_url("https://x.io/", "/api/party/poll"), "https://x.io/api/party/poll");
        assert_eq!(api_url(" https://x.io/ ", "/p"), "https://x.io/p");
    }

    #[test]
    fn display_name_prefers_label() {
        assert_eq!(display_name(" archer ", "PC1"), "archer");
        assert_eq!(display_name("  ", "PC1"), "PC1");
    }

    #[test]
    fn poll_wire_shapes() {
        let body = PollBody {
            room: "raid",
            passkey: "k",
            agent_id: "PC",
            member: "Bea",
            send: true,
            recv: true,
            last_seq: 4,
            last_result: Some(Ack { seq: 4, ok: true, detail: "OK".into() }),
        };
        let v: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(v["room"], "raid");
        assert_eq!(v["recv"], true);
        assert_eq!(v["last_seq"], 4);
        assert_eq!(v["last_result"]["ok"], true);

        let idle: PollResp = serde_json::from_str(
            r#"{"seq":5,"cmd":null,"members":[{"agent_id":"PC","name":"Al","send":false,"recv":true,"online":true,"last_result":null}]}"#,
        )
        .unwrap();
        assert_eq!(idle.seq, 5);
        assert!(idle.cmd.is_none());
        assert_eq!(idle.members.len(), 1);
        assert!(idle.members[0].recv && idle.members[0].online);

        let hit: PollResp =
            serde_json::from_str(r#"{"seq":6,"cmd":"PLAY raid1","members":[]}"#).unwrap();
        assert_eq!(hit.cmd.as_deref(), Some("PLAY raid1"));
    }

    #[test]
    fn member_rows_render() {
        let m = MemberSnap {
            name: "Bea".into(),
            send: true,
            recv: true,
            online: true,
            last_result: Some((true, "OK".into())),
        };
        assert_eq!(format_member(&m), "Bea [S+R] — OK");
        let m2 = MemberSnap {
            name: "Al".into(),
            send: false,
            recv: true,
            online: true,
            last_result: Some((false, "ERR not_found".into())),
        };
        assert_eq!(format_member(&m2), "Al [R] — ERR not_found");
        let m3 = MemberSnap { name: "Cy".into(), send: false, recv: true, online: false, last_result: None };
        assert_eq!(format_member(&m3), "Cy [R] — offline");
        let m4 = MemberSnap { name: "Dee".into(), send: false, recv: true, online: true, last_result: None };
        assert_eq!(format_member(&m4), "Dee [R]");
    }
}
