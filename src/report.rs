//! Fleet heartbeat reporting. A background thread polls the app's own state
//! every few seconds — playback, recording, the bar monitor's observations —
//! and POSTs it to the cadence dashboard server, plus discrete events on state
//! flips. Pure poller: nothing else in the app knows this module exists.
//!
//! The client reports raw truth only (bar states + how stale they are); all
//! judgment calls (offline, HP-critical-sustained, death-suspected) are made
//! server-side, so thresholds can be tuned without redeploying the fleet.
//! The client-side judgments are `pet` = "hungry" and `skill` = "idle": Pet
//! Guard / Idle Guard already had to wait their configured windows before those
//! words mean anything, and the edge event is what makes the POST immediate. The
//! server still decides when a sustained "hungry"/"idle" becomes an alert.
//! Timestamps are never sent — only relative ages — so VM clock skew is moot.
//!
//! The payload deliberately contains no window titles or exe paths.

use crate::config::AppConfig;
use crate::{monitor, player, recorder, update};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_secs(5);
const HEARTBEAT_SECS: u64 = 30;
const QUEUE_CAP: usize = 200;
/// Consecutive-failure backoff steps, seconds.
const BACKOFF: [u64; 3] = [5, 15, 60];

static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Serialize)]
struct WireEvent {
    kind: &'static str,
    detail: String,
    age_secs: u64,
}

#[derive(Serialize)]
struct WireBars {
    hp: &'static str,
    mp: &'static str,
    sp: &'static str,
}

#[derive(Serialize)]
struct ReportBody<'a> {
    agent_id: &'a str,
    label: &'a str,
    version: &'a str,
    playing: bool,
    recording: bool,
    window_found: bool,
    bars: WireBars,
    bars_age_secs: i64,
    /// Pet Guard: "off" (disabled, or no MP/SP monitor to ride on), "ok", "hungry".
    pet: &'static str,
    /// Idle Guard: "off" (skill pixel not armed), "ok", "idle" (no skill cooldown seen
    /// for the configured window while a sequence plays).
    skill: &'static str,
    events: Vec<WireEvent>,
}

struct Pending {
    kind: &'static str,
    detail: String,
    at: Instant,
}

pub fn start(cfg: &AppConfig) {
    if !cfg.report_enabled || cfg.report_url.is_empty() {
        return;
    }
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    CANCEL.store(false, Ordering::Release);
    let url = cfg.report_url.clone();
    let token = cfg.report_token.clone();
    let label = cfg.report_label.clone();
    thread::spawn(move || run(url, token, label));
    println!("[Cadence] Fleet reporting started.");
}

/// Signal the reporting thread to send a final `shutdown` event and stop.
/// Best-effort: the process may exit before the post completes; the server's
/// offline detection covers the miss.
pub fn stop() {
    CANCEL.store(true, Ordering::Release);
}

fn machine_name() -> String {
    unsafe {
        let mut buf = [0u16; 64];
        let mut len = buf.len() as u32;
        if winapi::um::winbase::GetComputerNameW(buf.as_mut_ptr(), &mut len) != 0 {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            "UNKNOWN".to_string()
        }
    }
}

fn bar_wire_state(b: &monitor::BarInfo) -> &'static str {
    if !b.enabled {
        "off"
    } else if b.low {
        "low"
    } else {
        "ok"
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Age in seconds of the freshest sample across enabled bars; -1 if nothing
/// has ever been sampled (no bars enabled, or window never in foreground).
fn bars_age_secs(snap: &monitor::BarsSnapshot) -> i64 {
    let now = now_unix_ms();
    snap.bars
        .iter()
        .filter(|b| b.enabled && b.last_sample_ms > 0)
        .map(|b| (now.saturating_sub(b.last_sample_ms) / 1000) as i64)
        .min()
        .unwrap_or(-1)
}

/// Pet Guard state on the wire. "off" unless the guard is on AND at least one of
/// the MP/SP monitors it rides on is enabled; "hungry" once MP/SP has stayed low
/// past the configured window despite the pet call.
fn pet_wire_state(snap: &monitor::BarsSnapshot) -> &'static str {
    let rides_on = snap.bars[1].enabled || snap.bars[2].enabled;
    if !snap.pet_guard || !rides_on {
        "off"
    } else if snap.pet_hungry {
        "hungry"
    } else {
        "ok"
    }
}

/// Idle Guard state on the wire. "off" unless the skill pixel slot is armed; "idle" once
/// no cooldown colour has been seen for the configured window while playing.
fn skill_wire_state(snap: &monitor::BarsSnapshot) -> &'static str {
    if !snap.bars[monitor::Bar::Skill as usize].enabled {
        "off"
    } else if snap.skill_idle {
        "idle"
    } else {
        "ok"
    }
}

#[derive(PartialEq, Clone, Copy)]
struct Edges {
    playing: bool,
    window_found: bool,
    hp_low: bool,
    pet_hungry: bool,
    skill_idle: bool,
}

fn push(queue: &mut VecDeque<Pending>, kind: &'static str, detail: String) {
    if queue.len() >= QUEUE_CAP {
        queue.pop_front();
    }
    queue.push_back(Pending {
        kind,
        detail,
        at: Instant::now(),
    });
}

fn run(url: String, token: String, label: String) {
    let agent_id = machine_name();
    let version = update::current_version();
    let mut queue: VecDeque<Pending> = VecDeque::new();
    let mut prev = Edges {
        playing: false,
        window_found: true,
        hp_low: false,
        pet_hungry: false,
        skill_idle: false,
    };
    let mut first = true;
    // Start "due" so the first heartbeat goes out on the first tick.
    let mut last_ok = Instant::now() - Duration::from_secs(HEARTBEAT_SECS);
    let mut failures: u32 = 0;
    let mut next_attempt = Instant::now();

    loop {
        let stopping = CANCEL.load(Ordering::Acquire);

        let snap = monitor::snapshot();
        let any_bar = snap.bars.iter().any(|b| b.enabled);
        let pet = pet_wire_state(&snap);
        let skill = skill_wire_state(&snap);
        let cur = Edges {
            playing: player::is_playing(),
            // Window presence is only knowable while the bar monitor runs.
            window_found: if any_bar { snap.window_found } else { true },
            hp_low: snap.bars[0].enabled && snap.bars[0].low,
            pet_hungry: pet == "hungry",
            skill_idle: skill == "idle",
        };

        if first {
            push(&mut queue, "startup", String::new());
            first = false;
        } else {
            if cur.playing != prev.playing {
                push(
                    &mut queue,
                    if cur.playing { "playback_started" } else { "playback_stopped" },
                    String::new(),
                );
            }
            if cur.window_found != prev.window_found {
                push(
                    &mut queue,
                    if cur.window_found { "window_found" } else { "window_lost" },
                    String::new(),
                );
            }
            if cur.hp_low != prev.hp_low {
                push(
                    &mut queue,
                    if cur.hp_low { "hp_low" } else { "hp_recovered" },
                    String::new(),
                );
            }
            if cur.pet_hungry != prev.pet_hungry {
                let detail = if cur.pet_hungry {
                    let low_secs = if snap.pet_low_since_ms > 0 {
                        now_unix_ms().saturating_sub(snap.pet_low_since_ms) / 1000
                    } else {
                        0
                    };
                    format!("mp/sp low {}s, pet called {}x", low_secs, snap.pet_calls)
                } else {
                    String::new()
                };
                push(
                    &mut queue,
                    if cur.pet_hungry { "pet_hungry" } else { "pet_recovered" },
                    detail,
                );
            }
            if cur.skill_idle != prev.skill_idle {
                let detail = if cur.skill_idle {
                    let secs = if snap.skill_nomatch_since_ms > 0 {
                        now_unix_ms().saturating_sub(snap.skill_nomatch_since_ms) / 1000
                    } else {
                        0
                    };
                    format!(
                        "no skill cooldown for {}s while playing, hp {}",
                        secs,
                        if cur.hp_low { "low" } else { "ok" }
                    )
                } else {
                    String::new()
                };
                push(
                    &mut queue,
                    if cur.skill_idle { "idle" } else { "idle_recovered" },
                    detail,
                );
            }
        }
        prev = cur;

        if stopping {
            push(&mut queue, "shutdown", String::new());
        }

        let due = stopping
            || last_ok.elapsed() >= Duration::from_secs(HEARTBEAT_SECS)
            || !queue.is_empty();
        if due && Instant::now() >= next_attempt {
            let events: Vec<WireEvent> = queue
                .iter()
                .map(|p| WireEvent {
                    kind: p.kind,
                    detail: p.detail.clone(),
                    age_secs: p.at.elapsed().as_secs(),
                })
                .collect();
            let body = ReportBody {
                agent_id: &agent_id,
                label: &label,
                version,
                playing: cur.playing,
                recording: recorder::is_recording(),
                window_found: cur.window_found,
                bars: WireBars {
                    hp: bar_wire_state(&snap.bars[0]),
                    mp: bar_wire_state(&snap.bars[1]),
                    sp: bar_wire_state(&snap.bars[2]),
                },
                bars_age_secs: bars_age_secs(&snap),
                pet,
                skill,
                events,
            };
            match serde_json::to_vec(&body) {
                Ok(json) => match update::https_post_json(&url, &token, &json) {
                    Ok((200, _)) => {
                        queue.clear();
                        last_ok = Instant::now();
                        failures = 0;
                        next_attempt = Instant::now();
                    }
                    Ok((status, _)) => {
                        failures += 1;
                        let delay = BACKOFF[(failures as usize - 1).min(BACKOFF.len() - 1)];
                        next_attempt = Instant::now() + Duration::from_secs(delay);
                        eprintln!("[Cadence] Report rejected (HTTP {}), retry in {}s.", status, delay);
                    }
                    Err(e) => {
                        failures += 1;
                        let delay = BACKOFF[(failures as usize - 1).min(BACKOFF.len() - 1)];
                        next_attempt = Instant::now() + Duration::from_secs(delay);
                        eprintln!("[Cadence] Report failed ({}), retry in {}s.", e, delay);
                    }
                },
                Err(e) => eprintln!("[Cadence] Report serialization failed: {}", e),
            }
        }

        if stopping {
            break;
        }
        thread::sleep(TICK);
    }

    println!("[Cadence] Fleet reporting stopped.");
    RUNNING.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape the server's POST /api/report handler deserializes.
    #[test]
    fn report_body_serializes_to_the_wire_shape() {
        let body = ReportBody {
            agent_id: "VM-RAN-07",
            label: "Archer main",
            version: "3.4.0",
            playing: true,
            recording: false,
            window_found: true,
            bars: WireBars { hp: "ok", mp: "low", sp: "off" },
            bars_age_secs: 2,
            pet: "hungry",
            skill: "idle",
            events: vec![WireEvent { kind: "playback_started", detail: String::new(), age_secs: 3 }],
        };
        let v: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(v["agent_id"], "VM-RAN-07");
        assert_eq!(v["bars"]["hp"], "ok");
        assert_eq!(v["bars"]["mp"], "low");
        assert_eq!(v["bars"]["sp"], "off");
        assert_eq!(v["bars_age_secs"], 2);
        assert_eq!(v["pet"], "hungry");
        assert_eq!(v["skill"], "idle");
        assert_eq!(v["events"][0]["kind"], "playback_started");
        assert_eq!(v["events"][0]["age_secs"], 3);
    }

    fn snap(pet_guard: bool, mp_on: bool, sp_on: bool, pet_hungry: bool) -> monitor::BarsSnapshot {
        let mut s = monitor::snapshot();
        s.pet_guard = pet_guard;
        s.bars[1].enabled = mp_on;
        s.bars[2].enabled = sp_on;
        s.pet_hungry = pet_hungry;
        s
    }

    #[test]
    fn pet_state_is_off_unless_guard_rides_on_an_enabled_bar() {
        assert_eq!(pet_wire_state(&snap(false, true, true, true)), "off");
        assert_eq!(pet_wire_state(&snap(true, false, false, true)), "off");
        assert_eq!(pet_wire_state(&snap(true, true, false, false)), "ok");
        assert_eq!(pet_wire_state(&snap(true, false, true, true)), "hungry");
    }

    #[test]
    fn skill_state_is_off_unless_the_skill_pixel_is_armed() {
        let mut s = monitor::snapshot();
        s.bars[monitor::Bar::Skill as usize].enabled = false;
        s.skill_idle = true;
        assert_eq!(skill_wire_state(&s), "off");
        s.bars[monitor::Bar::Skill as usize].enabled = true;
        s.skill_idle = false;
        assert_eq!(skill_wire_state(&s), "ok");
        s.skill_idle = true;
        assert_eq!(skill_wire_state(&s), "idle");
    }

    #[test]
    fn queue_caps_and_drops_oldest() {
        let mut q = VecDeque::new();
        for _ in 0..(QUEUE_CAP + 10) {
            push(&mut q, "hp_low", String::new());
        }
        assert_eq!(q.len(), QUEUE_CAP);
    }
}
