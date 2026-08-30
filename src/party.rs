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

use crate::{config, network, player, report, update};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

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
/// Set when THIS machine started the room's auto-loop, so it can re-assert the
/// loop after a server restart: (cmd, gap_secs, when it was set — a fresh start
/// gets a grace window before "auto missing" is read as "someone stopped it").
/// In-memory only on purpose: it dies with the process, and a restarted client
/// can no longer prove it still owns the loop (no zombie re-asserts).
static AUTO_WANTED: Mutex<Option<(String, u32, Instant)>> = Mutex::new(None);
/// The room's auto-loop as of the last poll, for the UI:
/// (cmd, round, next-fire deadline).
static AUTO_SNAP: Mutex<Option<(String, u64, Instant)>> = Mutex::new(None);
/// Seconds a freshly-set AUTO_WANTED is trusted even when the poll shows no
/// auto — our own start POST may still be in flight.
const AUTO_GRACE_SECS: u64 = 10;

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
    /// One-pass duration of the run this ack started; the server schedules the
    /// next auto-loop round off the longest member's report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_micros: Option<i64>,
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
struct WireAuto {
    cmd: String,
    #[serde(default)]
    round: u64,
    #[serde(default)]
    next_in_secs: i64,
    #[serde(default)]
    started_by: String,
}

#[derive(Deserialize)]
struct PollResp {
    seq: u64,
    cmd: Option<String>,
    #[serde(default)]
    members: Vec<WireMember>,
    #[serde(default)]
    auto: Option<WireAuto>,
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

/// Whether the room currently has an auto-loop (as of the last poll). Drives
/// the Remote dialog's Start/Stop-auto button text.
pub fn auto_active() -> bool {
    AUTO_SNAP.lock().unwrap().is_some()
}

/// Start (or replace) the room's server-hosted auto-loop: the server re-fires
/// `PLAY <name>` every round at the longest member's reported duration plus a
/// small margin plus `gap_secs` of rest. The loop lives on the room, so every
/// machine (this one included) can go AFK and rounds keep firing.
pub fn start_auto(name: &str, gap_secs: u32) {
    post_auto(Some((format!("PLAY {}", name.trim()), gap_secs)));
}

/// Stop the auto-loop: future rounds only, running playback is untouched (the
/// Send-Stop button remains the panic path — and does NOT stop the loop).
pub fn stop_auto() {
    post_auto(None);
}

/// Fire-and-forget POST for auto start (`Some((cmd, gap))`) or stop (`None`);
/// updates AUTO_WANTED ownership and the LAST_SEND note from the outcome.
fn post_auto(start: Option<(String, u32)>) {
    if !sender_active() {
        return;
    }
    let cfg = config::cached_config();
    thread::spawn(move || {
        let agent_id = report::machine_name();
        let mut body = serde_json::json!({
            "room": cfg.party_room.trim(),
            "passkey": cfg.party_passkey.trim(),
            "agent_id": agent_id,
            "member": display_name(&cfg.report_label, &agent_id),
        });
        match &start {
            Some((cmd, gap)) => {
                body["cmd"] = serde_json::json!(cmd);
                body["auto_start"] = serde_json::json!(true);
                body["auto_gap_secs"] = serde_json::json!(gap);
            }
            None => body["auto_stop"] = serde_json::json!(true),
        }
        let url = api_url(&cfg.party_url, "/api/party/send");
        let json = serde_json::to_vec(&body).unwrap_or_default();
        let note = match update::https_post_json_timeout(&url, &json, SEND_RECV_TIMEOUT_MS) {
            Ok((200, _)) => match start {
                Some((cmd, gap)) => {
                    *AUTO_WANTED.lock().unwrap() = Some((cmd, gap, Instant::now()));
                    "auto started".to_string()
                }
                None => {
                    *AUTO_WANTED.lock().unwrap() = None;
                    *AUTO_SNAP.lock().unwrap() = None;
                    "auto stopped".to_string()
                }
            },
            Ok((403, _)) => "auto failed: wrong passkey".to_string(),
            Ok((status, _)) => format!("auto failed (HTTP {})", status),
            Err(e) => format!("auto failed ({})", e),
        };
        *LAST_SEND.lock().unwrap() = note;
    });
}

/// One line for the Remote dialog's status static.
pub fn status_line() -> String {
    let conn = STATUS.lock().unwrap().clone();
    let sent = LAST_SEND.lock().unwrap().clone();
    let auto = AUTO_SNAP.lock().unwrap().as_ref().map(|(cmd, round, deadline)| {
        format_auto(cmd, *round, deadline.saturating_duration_since(Instant::now()).as_secs())
    });
    let parts: Vec<String> = [Some(conn), auto, Some(sent)]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    parts.join(" · ")
}

/// Status-line fragment for an active auto-loop; the dialog's timer repaints
/// every 500ms, so the countdown runs live between polls.
fn format_auto(cmd: &str, round: u64, secs_left: u64) -> String {
    format!("Auto: {} r{}, next {}s", cmd, round, secs_left)
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

#[derive(PartialEq, Debug)]
enum AutoAction {
    Keep,
    ClearWanted,
    Reassert,
}

/// What to do with this machine's auto-loop ownership after a poll.
/// `started_by_me`: whether the room's auto (if any) names this machine.
/// `backwards`: the response seq was lower than ours before adoption — the
/// server-restart signature, meaning the room was rebuilt and a loop we own
/// must be re-asserted. Auto missing WITHOUT that signature means another
/// sender stopped or replaced it: drop ownership so a stopped loop never comes
/// back as a zombie. The grace window covers a just-sent start whose POST the
/// poll may have raced.
fn auto_reconcile(
    wanted: bool,
    wanted_age_secs: u64,
    started_by_me: Option<bool>,
    backwards: bool,
) -> AutoAction {
    match (wanted, started_by_me) {
        (false, _) => AutoAction::Keep,
        (true, Some(true)) => AutoAction::Keep,
        (true, Some(false)) => AutoAction::ClearWanted,
        (true, None) if backwards => AutoAction::Reassert,
        (true, None) if wanted_age_secs < AUTO_GRACE_SECS => AutoAction::Keep,
        (true, None) => AutoAction::ClearWanted,
    }
}

fn set_auto_snap(auto: Option<&WireAuto>) {
    *AUTO_SNAP.lock().unwrap() = auto.map(|a| {
        let deadline = Instant::now() + Duration::from_secs(a.next_in_secs.max(0) as u64);
        (a.cmd.clone(), a.round, deadline)
    });
}

/// Apply auto_reconcile to the statics after a successful poll; a Reassert
/// refreshes the grace stamp first so the next poll doesn't race the re-POST.
fn reconcile_auto(auto: Option<&WireAuto>, agent_id: &str, backwards: bool) {
    let resend = {
        let mut wanted = AUTO_WANTED.lock().unwrap();
        let Some((cmd, gap, at)) = wanted.clone() else { return };
        let started_by_me = auto.map(|a| a.started_by == agent_id);
        match auto_reconcile(true, at.elapsed().as_secs(), started_by_me, backwards) {
            AutoAction::Keep => None,
            AutoAction::ClearWanted => {
                *wanted = None;
                None
            }
            AutoAction::Reassert => {
                *wanted = Some((cmd.clone(), gap, Instant::now()));
                Some((cmd, gap))
            }
        }
    };
    if let Some((cmd, gap)) = resend {
        println!("[Cadence] Party auto-loop re-asserted after server restart: {}", cmd);
        post_auto(Some((cmd, gap)));
    }
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
                // Disconnecting is a user action, not an outage: drop the room's
                // auto view and any ownership claim.
                *AUTO_SNAP.lock().unwrap() = None;
                *AUTO_WANTED.lock().unwrap() = None;
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
                    let backwards = pr.seq < last_seq;
                    set_auto_snap(pr.auto.as_ref());
                    reconcile_auto(pr.auto.as_ref(), &agent_id, backwards);
                    let (next, cmd) = advance(last_seq, pr.seq, pr.cmd);
                    last_seq = next;
                    if let Some(cmd) = cmd {
                        // Same path as a LAN remote command: recording gate,
                        // stop-and-override takeover, local name resolution.
                        let result = network::execute_command(&cmd).trim().to_string();
                        println!("[Cadence] Party command: {} -> {}", cmd, result);
                        let ok = result.starts_with("OK");
                        pending_ack = Some(Ack {
                            seq: next,
                            ok,
                            detail: result,
                            // execute_command returns right after playback
                            // starts, so the player's pass duration is live.
                            pass_micros: if ok {
                                player::progress().map(|(_, p)| p).filter(|p| *p > 0)
                            } else {
                                None
                            },
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
                *AUTO_SNAP.lock().unwrap() = None;
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
    *AUTO_SNAP.lock().unwrap() = None;
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
            last_result: Some(Ack {
                seq: 4,
                ok: true,
                detail: "OK".into(),
                pass_micros: Some(140_000_000),
            }),
        };
        let v: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(v["room"], "raid");
        assert_eq!(v["recv"], true);
        assert_eq!(v["last_seq"], 4);
        assert_eq!(v["last_result"]["ok"], true);
        assert_eq!(v["last_result"]["pass_micros"], 140_000_000);
        // Without a duration the key is omitted entirely (old-server friendly).
        let bare = serde_json::to_value(Ack { seq: 1, ok: false, detail: "ERR x".into(), pass_micros: None }).unwrap();
        assert!(bare.get("pass_micros").is_none());

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
        assert!(hit.auto.is_none()); // absent and null both mean no auto

        let with_auto: PollResp = serde_json::from_str(
            r#"{"seq":7,"cmd":null,"members":[],"auto":{"cmd":"PLAY 3f3","round":12,"gap_secs":3,"next_in_secs":41,"started_by":"PC9"}}"#,
        )
        .unwrap();
        let a = with_auto.auto.unwrap();
        assert_eq!((a.cmd.as_str(), a.round, a.next_in_secs, a.started_by.as_str()), ("PLAY 3f3", 12, 41, "PC9"));
        let null_auto: PollResp =
            serde_json::from_str(r#"{"seq":7,"cmd":null,"members":[],"auto":null}"#).unwrap();
        assert!(null_auto.auto.is_none());
    }

    #[test]
    fn auto_reconcile_matrix() {
        use AutoAction::*;
        // Not an owner: nothing to do regardless of what the room shows.
        assert_eq!(auto_reconcile(false, 999, None, true), Keep);
        assert_eq!(auto_reconcile(false, 999, Some(false), false), Keep);
        // Our loop is running: keep ownership.
        assert_eq!(auto_reconcile(true, 999, Some(true), false), Keep);
        // Another sender replaced it: they own re-assert now.
        assert_eq!(auto_reconcile(true, 999, Some(false), false), ClearWanted);
        // Server restarted (seq went backwards) and the loop is gone: re-assert.
        assert_eq!(auto_reconcile(true, 999, None, true), Reassert);
        // Loop gone without a restart signature: someone stopped it — no zombie.
        assert_eq!(auto_reconcile(true, AUTO_GRACE_SECS, None, false), ClearWanted);
        // ...unless our own start was just sent and the poll raced it.
        assert_eq!(auto_reconcile(true, AUTO_GRACE_SECS - 1, None, false), Keep);
    }

    #[test]
    fn auto_status_renders() {
        assert_eq!(format_auto("PLAY 3f3", 12, 41), "Auto: PLAY 3f3 r12, next 41s");
        assert_eq!(format_auto("PLAY x", 1, 0), "Auto: PLAY x r1, next 0s");
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
