//! Player-proximity alert: sniffs the RAN Online game-world stream and presses a key when
//! another player enters your character's view.
//!
//! It captures server->client TCP on the game port via Npcap (wpcap.dll, loaded dynamically so
//! no build-time SDK is required), reassembles the stream, decodes the NET_COMPRESS envelope
//! (with LZO1X decompression), walks the inner messages, and fires on the DROP_PC opcode.
//! See D:\Dev\Ran_services\ran_sniff\PROTOCOL.md for the reverse-engineered wire format.

use crate::{player, storage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::os::raw::{c_char, c_int, c_long, c_void};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use winapi::um::consoleapi::{AllocConsole, WriteConsoleW};
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryExW, LoadLibraryW};
use winapi::um::minwinbase::SYSTEMTIME;
use winapi::um::processenv::GetStdHandle;
use winapi::um::sysinfoapi::{GetLocalTime, GetSystemDirectoryW};
use winapi::um::wincon::SetConsoleTitleW;
use winapi::um::winbase::STD_OUTPUT_HANDLE;
use winapi::um::winuser::{MapVirtualKeyW, PostMessageW, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
// Scan mode: when set, the capture logs every detected player to the lists but NEVER reacts
// (no key press, no disarm) — a passive "who's around" scan that won't trip the trigger.
static SCAN_ONLY: AtomicBool = AtomicBool::new(false);

/// One detected player: name, last-known level, when last seen, and how many times.
/// Serialized to disk for the persistent (all-time) list.
#[derive(Clone, Serialize, Deserialize)]
pub struct DetectedPlayer {
    pub name: String,
    #[serde(default)]
    pub level: Option<u16>,
    /// emClass bitflag (single bit). Mapped to a class name in the UI.
    #[serde(default)]
    pub class: Option<u16>,
    /// School index: 0=Sacred Gate, 1=Mystic Peak, 2=Phoenix.
    #[serde(default)]
    pub school: Option<u16>,
    /// Guild (club) name; empty if none.
    #[serde(default)]
    pub guild: String,
    #[serde(default)]
    pub hp_now: Option<u16>,
    #[serde(default)]
    pub hp_max: Option<u16>,
    /// Map main id (SNATIVEID.wMainID).
    #[serde(default)]
    pub map: Option<u16>,
    #[serde(default)]
    pub pos_x: Option<i32>,
    #[serde(default)]
    pub pos_z: Option<i32>,
    /// Unix seconds of the most recent sighting (used for latest-first sorting + persistence).
    #[serde(default)]
    pub last_unix: i64,
    /// Local "MM-DD HH:MM" of the most recent sighting, formatted at detection time.
    #[serde(default)]
    pub seen_str: String,
    #[serde(default = "one")]
    pub count: u32,
}

fn one() -> u32 { 1 }

/// Everything parsed from one DROP_PC (SDROP_CHAR) payload. Fields are Option/empty when the
/// value failed a sanity check (short/garbled message), so a bad read never poisons the record.
pub struct PcInfo {
    pub name: String,
    pub level: Option<u16>,
    pub class: Option<u16>,
    pub school: Option<u16>,
    pub guild: String,
    pub hp_now: Option<u16>,
    pub hp_max: Option<u16>,
    pub map: Option<u16>,
    pub pos_x: Option<i32>,
    pub pos_z: Option<i32>,
}

// Two detection lists, both keyed by name (deduped):
//   SESSION   — players seen since Cadence started; cleared on demand ("Clear" in Session view).
//   PERMANENT — an all-time log persisted to detected_players.json; cleared separately.
// Plus the ignore list (names that must NOT trigger the key press). Shared with the Players UI.
static SESSION: Mutex<Vec<DetectedPlayer>> = Mutex::new(Vec::new());
static PERMANENT: Mutex<Vec<DetectedPlayer>> = Mutex::new(Vec::new());
static PERM_DIRTY: AtomicBool = AtomicBool::new(false);
static IGNORE: Mutex<Vec<String>> = Mutex::new(Vec::new());

const SESSION_CAP: usize = 500;
const PERMANENT_CAP: usize = 3000;
// Reaction sequence name to play on detection; empty = press the proximity key instead.
static REACTION_SEQ: Mutex<String> = Mutex::new(String::new());

/// Set the reaction sequence (empty = press key). Called on startup and when settings change.
pub fn set_reaction(sequence: String) {
    *REACTION_SEQ.lock().unwrap() = sequence;
}

// Window + message to post when a player is detected, so the toolbar can uncheck "Det" (one-shot).
static NOTIFY_HWND: AtomicIsize = AtomicIsize::new(0);
static NOTIFY_MSG: AtomicU32 = AtomicU32::new(0);

/// Register the toolbar window + message to notify when detection fires (disarm/uncheck).
pub fn set_notify(hwnd: isize, msg: u32) {
    NOTIFY_HWND.store(hwnd, Ordering::Release);
    NOTIFY_MSG.store(msg, Ordering::Release);
}

fn notify_hit() {
    let hwnd = NOTIFY_HWND.load(Ordering::Acquire);
    let msg = NOTIFY_MSG.load(Ordering::Acquire);
    if hwnd != 0 && msg != 0 {
        unsafe {
            PostMessageW(hwnd as _, msg, 0, 0);
        }
    }
}

// --- RAN protocol constants (calibrated against the C:\RAN Portal server, base 977) ----------
const NET_MSG_COMPRESS: u32 = 170; // fixed envelope opcode, not region-shifted
// Region/build opcode base. RAN Portal (51.79.253.42) uses a custom base of 988 -> DROP_PC=3011,
// confirmed by DROP_CROW(3012, frequent) and DROP_OUT(3015) lining up. (The public 143.14.88.19
// server used 977 -> DROP_PC=3000.) If you point Cadence at a different server, this may change.
const NET_MSG_BASE: u32 = 988;
pub(crate) const NET_MSG_GCTRL: u32 = NET_MSG_BASE + 1900;
const DROP_PC: u32 = NET_MSG_GCTRL + 123; // 3011 — a player entered view
const ENVELOPE_HEADER: usize = 12; // [dwSize:u32][nType=170:u32][bCompress:u32]
#[allow(dead_code)]
const GAME_PORT: u16 = 7112;
const MAX_ENVELOPE: u32 = 0x20000;
const MAX_FLOW_BUF: usize = 4 * 1024 * 1024;

// --- Debug console + log file ---------------------------------------------------------------
// Release builds have no console, so detection would be invisible. When proximity starts we
// attach a console and also tee every line to %TEMP%\cadence_proximity.log as a backup.
static CONSOLE_OUT: AtomicIsize = AtomicIsize::new(0);
static DEBUG_READY: AtomicBool = AtomicBool::new(false);
static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

fn ensure_debug() {
    if DEBUG_READY.swap(true, Ordering::AcqRel) {
        return; // already attached (survives stop/start toggles)
    }
    unsafe {
        AllocConsole();
        SetConsoleTitleW(wide("Cadence Proximity Debug").as_ptr());
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        CONSOLE_OUT.store(h as isize, Ordering::Release);
    }
    if let Ok(f) = std::fs::File::create(std::env::temp_dir().join("cadence_proximity.log")) {
        *LOG_FILE.lock().unwrap() = Some(f);
    }
}

fn dlog(msg: &str) {
    let line = format!("{}\r\n", msg);
    let h = CONSOLE_OUT.load(Ordering::Acquire);
    if h != 0 {
        let w: Vec<u16> = line.encode_utf16().collect();
        let mut written = 0u32;
        unsafe {
            WriteConsoleW(
                h as _,
                w.as_ptr() as _,
                w.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            );
        }
    }
    if let Ok(mut g) = LOG_FILE.lock() {
        if let Some(f) = g.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

/// Parse a dotted-quad IPv4 into a big-endian u32 matching the IP header byte order.
pub(crate) fn parse_ipv4(s: &str) -> u32 {
    let mut o = [0u32; 4];
    for (i, part) in s.split('.').enumerate().take(4) {
        o[i] = part.trim().parse().unwrap_or(0);
    }
    (o[0] << 24) | (o[1] << 16) | (o[2] << 8) | o[3]
}

#[allow(dead_code)] // public status accessor, parity with pet_cycle/hp_monitor
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Current local wall-clock as "MM-DD HH:MM" (formatted here so persisted rows need no tz math).
fn now_local_str() -> String {
    unsafe {
        let mut st: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);
        format!("{:02}-{:02} {:02}:{:02}", st.wMonth, st.wDay, st.wHour, st.wMinute)
    }
}

/// Merge freshly-parsed fields into a record; only overwrite a field when the new read is valid,
/// so a later short/partial packet can't wipe details we already learned.
fn merge_fields(p: &mut DetectedPlayer, info: &PcInfo) {
    if info.level.is_some() { p.level = info.level; }
    if info.class.is_some() { p.class = info.class; }
    if info.school.is_some() { p.school = info.school; }
    if !info.guild.is_empty() { p.guild = info.guild.clone(); }
    if info.hp_max.is_some() { p.hp_now = info.hp_now; p.hp_max = info.hp_max; }
    if info.map.is_some() { p.map = info.map; }
    if info.pos_x.is_some() { p.pos_x = info.pos_x; p.pos_z = info.pos_z; }
}

/// Insert or update a player in one list (dedup by name; refresh fields/time/count).
fn upsert(list: &Mutex<Vec<DetectedPlayer>>, info: &PcInfo, now: i64, stamp: &str, cap: usize) {
    let mut d = list.lock().unwrap();
    if let Some(p) = d.iter_mut().find(|p| p.name == info.name) {
        p.last_unix = now;
        p.seen_str = stamp.to_string();
        p.count = p.count.saturating_add(1);
        merge_fields(p, info);
    } else {
        let mut p = DetectedPlayer {
            name: info.name.clone(),
            level: None, class: None, school: None, guild: String::new(),
            hp_now: None, hp_max: None, map: None, pos_x: None, pos_z: None,
            last_unix: now, seen_str: stamp.to_string(), count: 1,
        };
        merge_fields(&mut p, info);
        d.push(p);
        if d.len() > cap {
            // Drop the oldest sightings first.
            d.sort_by(|a, b| b.last_unix.cmp(&a.last_unix));
            d.truncate(cap);
        }
    }
}

fn record_detection(info: &PcInfo) {
    if info.name.is_empty() || info.name == "<player>" {
        return;
    }
    let now = now_unix();
    let stamp = now_local_str();
    upsert(&SESSION, info, now, &stamp, SESSION_CAP);
    upsert(&PERMANENT, info, now, &stamp, PERMANENT_CAP);
    PERM_DIRTY.store(true, Ordering::Release);
}

fn is_ignored(name: &str) -> bool {
    IGNORE.lock().unwrap().iter().any(|n| n == name)
}

/// Snapshot of players detected this session (for the Players UI).
pub fn session_players() -> Vec<DetectedPlayer> {
    SESSION.lock().unwrap().clone()
}

/// Snapshot of the persistent all-time detection log (for the Players UI).
pub fn permanent_players() -> Vec<DetectedPlayer> {
    PERMANENT.lock().unwrap().clone()
}

/// Names currently on the ignore list (won't trigger the key).
pub fn ignored_players() -> Vec<String> {
    IGNORE.lock().unwrap().clone()
}

/// Replace the ignore list (called on startup from config, and by the Players UI).
pub fn set_ignored(names: Vec<String>) {
    *IGNORE.lock().unwrap() = names;
}

/// Flip a name's ignore state; returns the new state (true = now ignored).
pub fn toggle_ignore(name: &str) -> bool {
    let mut ig = IGNORE.lock().unwrap();
    if let Some(pos) = ig.iter().position(|n| n == name) {
        ig.remove(pos);
        false
    } else {
        ig.push(name.to_string());
        true
    }
}

/// Clear the session detection list (ignore + permanent lists are kept).
pub fn clear_session() {
    SESSION.lock().unwrap().clear();
}

/// Clear the persistent all-time detection list and erase it on disk.
pub fn clear_permanent() {
    PERMANENT.lock().unwrap().clear();
    save_permanent();
}

/// Remove one player by name from a list. `permanent` selects which list; when true the change
/// is written to disk immediately.
pub fn delete_player(permanent: bool, name: &str) {
    let list = if permanent { &PERMANENT } else { &SESSION };
    list.lock().unwrap().retain(|p| p.name != name);
    if permanent {
        save_permanent();
    }
}

/// Turn passive scan mode on/off. In scan mode the capture logs players without reacting.
pub fn set_scan_only(on: bool) {
    SCAN_ONLY.store(on, Ordering::Release);
}

/// Whether passive scan mode is currently engaged.
pub fn is_scan_only() -> bool {
    SCAN_ONLY.load(Ordering::Acquire)
}

fn permanent_path() -> std::path::PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = base.join("ranify2");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("detected_players.json")
}

/// Load the persistent all-time detection list from disk (called once at startup).
pub fn load_permanent() {
    if let Ok(txt) = std::fs::read_to_string(permanent_path()) {
        if let Ok(v) = serde_json::from_str::<Vec<DetectedPlayer>>(&txt) {
            *PERMANENT.lock().unwrap() = v;
        }
    }
}

/// Write the persistent all-time detection list to disk.
pub fn save_permanent() {
    let v = PERMANENT.lock().unwrap().clone();
    if let Ok(txt) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(permanent_path(), txt);
    }
    PERM_DIRTY.store(false, Ordering::Release);
}

/// Enumerate available Npcap capture devices as (name, description) for the Settings UI.
/// Returns an empty list if Npcap is not installed.
pub fn list_devices() -> Vec<(String, String)> {
    match Pcap::load() {
        Ok(pc) => pc.list_devices(),
        Err(_) => Vec::new(),
    }
}

/// Start the proximity watcher.
/// * `vk`        - virtual-key code to press on detection (e.g. 0x45 = 'E')
/// * `iface`     - Npcap device name (empty = auto-pick the first device)
/// * `server_ip` - game server IP for the capture filter (e.g. "143.14.88.19")
/// * `cooldown_ms` - minimum gap between key presses
pub fn start(vk: u16, iface: String, server_ip: String, cooldown_ms: u64) {
    // Make restart reliable: signal any previous capture to stop and WAIT for it to finish,
    // so a stop()+start() (e.g. clicking OK in Settings while running) doesn't race into
    // "nothing running". stop() only sets the cancel flag; the thread clears ACTIVE on exit.
    CANCEL.store(true, Ordering::Release);
    for _ in 0..75 {
        if !ACTIVE.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(20)); // up to ~1.5s for the old capture to stop
    }
    if ACTIVE.load(Ordering::Acquire) {
        eprintln!("[Cadence] Proximity: previous capture still stopping; start skipped.");
        return;
    }
    CANCEL.store(false, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);

    thread::spawn(move || {
        println!(
            "[Cadence] Proximity alert started (key=0x{:02X}, iface={:?}, server={})",
            vk,
            if iface.is_empty() { "auto" } else { &iface },
            server_ip
        );

        if let Err(e) = capture_loop(vk, &iface, &server_ip, cooldown_ms) {
            eprintln!("[Cadence] Proximity alert error: {}", e);
        }

        println!("[Cadence] Proximity alert stopped.");
        ACTIVE.store(false, Ordering::Release);
    });
}

pub fn stop() {
    if ACTIVE.load(Ordering::Acquire) {
        CANCEL.store(true, Ordering::Release);
    }
}

fn press_key(vk: u16) {
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    player::send_key_input(vk, scan, KEYEVENTF_SCANCODE);
    player::send_key_input(vk, scan, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP);
}

/// Stop any currently-playing sequence/queue and wait for it to actually stop. Returns whether
/// something was playing.
fn stop_current_playback() -> bool {
    if !player::is_playing() {
        return false;
    }
    player::cancel_playback();
    for _ in 0..75 {
        if !player::is_playing() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Do the reaction: play the configured sequence, or press the key. Returns a log description.
/// Called only AFTER detection has been disarmed, so it can't re-trigger.
fn react_action(vk: u16) -> String {
    let seq = REACTION_SEQ.lock().unwrap().clone();
    if seq.is_empty() {
        press_key(vk);
        "pressed key".to_string()
    } else {
        match storage::load_sequence(&seq) {
            Ok(s) => {
                player::play_sequence_once(s.events); // one-shot: never loop, even if Loop is on
                format!("played sequence '{}' once", seq)
            }
            Err(e) => {
                press_key(vk); // fall back to the key if the sequence can't load
                format!("sequence '{}' failed ({}) — pressed key", seq, e)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Capture loop
// ---------------------------------------------------------------------------------------------

fn capture_loop(vk: u16, iface: &str, server_ip: &str, cooldown_ms: u64) -> Result<(), String> {
    ensure_debug();
    dlog(&format!(
        "=== Cadence Proximity starting — key=0x{:02X}, server={}, cooldown={}ms ===",
        vk, server_ip, cooldown_ms
    ));

    let pc = match Pcap::load() {
        Ok(p) => p,
        Err(e) => {
            dlog(&format!("ERROR: {}", e));
            return Err(e);
        }
    };
    dlog("wpcap.dll loaded OK.");

    let dev = if iface.is_empty() {
        match pc.first_device() {
            Ok(d) => {
                dlog(&format!("auto-selected interface: {}", d));
                dlog("  (if you see pkts=0 below, this is the wrong adapter — pick your Wi-Fi/Ethernet in Settings)");
                d
            }
            Err(e) => {
                dlog(&format!("ERROR: {}", e));
                return Err(e);
            }
        }
    } else {
        dlog(&format!("using interface: {}", iface));
        iface.to_string()
    };

    let handle = match pc.open_live(&dev, 200) {
        Ok(h) => h,
        Err(e) => {
            dlog(&format!("ERROR opening interface: {}", e));
            dlog("Tip: run Cadence as Administrator, or choose a different Interface in Settings.");
            return Err(e);
        }
    };
    dlog("interface opened for capture.");

    // Broad filter so we can DISCOVER which server IP + adapter actually carry the game.
    let filter = "tcp".to_string();
    if let Err(e) = pc.set_filter(handle, &filter) {
        dlog(&format!("WARNING: set filter failed ({}).", e));
    }
    let configured = if server_ip.trim().is_empty() { 0u32 } else { parse_ipv4(server_ip.trim()) };
    let intro = if configured != 0 {
        format!("Detecting players from server {} on RAN ports 7101/7112/7113. Move near another player...", fmt_ip(configured))
    } else {
        "No Server IP set — auto-discovering the RAN server. Log into the game and move around...".to_string()
    };
    dlog(&intro);

    let mut flows: HashMap<u64, Vec<u8>> = HashMap::new();
    // One-shot: on the first non-ignored detection we react and disarm (the toolbar unchecks
    // "Det"; the user re-arms manually). cooldown_ms is unused in one-shot mode.
    let _ = cooldown_ms;
    let mut disarm = false;
    let mut hit_name: Option<String> = None;
    let mut fired: Option<(String, bool)> = None; // (player name, was a sequence stopped)

    let start = Instant::now();
    let mut last_stats = Instant::now();
    let mut total_pkts: u64 = 0;
    let mut srv_pkts: u64 = 0;
    let mut conn_seen: HashMap<(u32, u16), u64> = HashMap::new();
    let mut port_counts: HashMap<u16, u64> = HashMap::new();
    let mut envelopes: u64 = 0;
    let mut inner: u64 = 0;
    let mut droppc: u64 = 0;
    let mut op_hist: HashMap<u32, u64> = HashMap::new();
    let mut name_ops: HashMap<u32, String> = HashMap::new();

    loop {
        if CANCEL.load(Ordering::Acquire) {
            break;
        }

        // Periodic diagnostics — prints even when no packets arrive, so a dead
        // interface / wrong server IP is obvious.
        if last_stats.elapsed() >= Duration::from_secs(2) {
            last_stats = Instant::now();
            // Flush new detections to disk at most once per tick (keeps the all-time list
            // durable during long scans without a disk write on every packet).
            if PERM_DIRTY.load(Ordering::Acquire) {
                save_permanent();
            }
            // Discovered game server = busiest non-web service endpoint.
            let disc = conn_seen
                .iter()
                .filter(|((_, p), _)| !matches!(*p, 443 | 80 | 53))
                .max_by_key(|(_, c)| **c)
                .map(|((ip, p), c)| (*ip, *p, *c));
            let disc_str = disc
                .map(|(ip, p, c)| format!("{}:{}({})", fmt_ip(ip), p, c))
                .unwrap_or_else(|| "none".into());
            let mut ports: Vec<String> = port_counts.iter().map(|(p, c)| format!("{}:{}", p, c)).collect();
            ports.sort();
            dlog(&format!(
                "[t+{:>3}s] pkts={} gameServer={} fromActive={} ports=[{}] envelopes={} inner={} DROP_PC={}",
                start.elapsed().as_secs(), total_pkts, disc_str, srv_pkts, ports.join(" "), envelopes, inner, droppc
            ));
            if total_pkts == 0 {
                dlog("   -> This adapter sees NO traffic. Wrong interface — pick another in Settings.");
            } else if disc.is_none() {
                dlog("   -> Only web/system traffic here. Game not running here, or wrong adapter.");
            } else if configured != 0 && Some(configured) != disc.map(|(ip, _, _)| ip) {
                dlog(&format!(
                    "   -> Your Server IP {} doesn't match the busy game server {}. Clear the Server IP field (blank) to auto-lock it, or set it to that IP.",
                    fmt_ip(configured), fmt_ip(disc.unwrap().0)
                ));
            } else if srv_pkts == 0 {
                dlog("   -> Not parsing the game server yet — clear the Server IP field (blank) to auto-lock the server above.");
            } else if envelopes == 0 {
                dlog("   -> Server traffic seen but no NET_COMPRESS envelopes yet.");
            } else if droppc == 0 {
                let mut v: Vec<(u32, u64)> = op_hist.iter().map(|(k, c)| (*k, *c)).collect();
                v.sort_by(|a, b| b.1.cmp(&a.1));
                let top: Vec<String> = v.iter().take(10).map(|(op, c)| format!("{}:{}", op, c)).collect();
                dlog(&format!("   -> Game data flowing but no DROP_PC={} yet (base may differ on this server). Opcodes(top)=[{}].", DROP_PC, top.join(" ")));
                let names: Vec<String> = name_ops.iter().take(12).map(|(op, nm)| format!("{}='{}'", op, nm)).collect();
                if !names.is_empty() {
                    dlog(&format!("   -> Name-carrying opcodes (DROP_PC candidates): [{}]. Paste me this line to recalibrate.", names.join(" ")));
                }
            }
        }

        let pkt = match pc.next_packet(handle) {
            Ok(Some(p)) => p,
            Ok(None) => continue, // read timeout
            Err(_) => {
                dlog("capture read error; stopping.");
                break;
            }
        };
        total_pkts += 1;
        let (sip, dip, sp, dp, payload) = match parse_tcp(pkt) {
            Some(t) => t,
            None => continue,
        };
        // Record service endpoints (lower port = the server side). The busiest non-web endpoint
        // is the game server — works even when RAN Portal remaps the standard ports.
        let (svc_ip, svc_port) = if sp <= dp { (sip, sp) } else { (dip, dp) };
        *conn_seen.entry((svc_ip, svc_port)).or_insert(0) += 1;

        // Detect against the configured server, or the busiest discovered game server.
        let active = if configured != 0 {
            configured
        } else {
            conn_seen
                .iter()
                .filter(|((_, port), _)| !matches!(*port, 443 | 80 | 53))
                .max_by_key(|(_, c)| **c)
                .map(|((ip, _), _)| *ip)
                .unwrap_or(0)
        };

        if active != 0 && sip == active && !payload.is_empty() {
            srv_pkts += 1;
            *port_counts.entry(sp).or_insert(0) += 1;
            let key = ((sp as u64) << 16) | dp as u64;
            let buf = flows.entry(key).or_default();
            buf.extend_from_slice(payload);
            if buf.len() > MAX_FLOW_BUF {
                buf.clear();
                continue;
            }
            let env = parse_flow(buf, &mut |op, msg| {
                inner += 1;
                *op_hist.entry(op).or_insert(0) += 1;
                // Record an example name for any opcode whose payload carries one — this is how we
                // identify DROP_PC on a server whose opcode base we don't know yet.
                if !name_ops.contains_key(&op) {
                    if let Some(nm) = scan_name(msg) {
                        name_ops.insert(op, nm);
                    }
                }
                if op == DROP_PC {
                    droppc += 1;
                    let info = parse_pc(&msg[8..]);
                    let name = info.name.clone();
                    record_detection(&info);
                    let scan = SCAN_ONLY.load(Ordering::Acquire);
                    let lvl_s = info.level.map(|l| format!(" Lv{}", l)).unwrap_or_default();
                    if scan {
                        // Passive scan: log only, never react or disarm.
                        dlog(&format!("[t+{:.1}s] scan: {}{}", start.elapsed().as_secs_f32(), name, lvl_s));
                    } else if disarm {
                        // Already flagged a detection this cycle — ignore the rest.
                    } else if is_ignored(&name) {
                        dlog(&format!("[t+{:.1}s] player in view: {}{} (ignored)", start.elapsed().as_secs_f32(), name, lvl_s));
                    } else {
                        // First real player: flag it; the reaction runs after parsing this batch.
                        hit_name = Some(name);
                        disarm = true;
                    }
                }
            });
            envelopes += env as u64;

            if disarm {
                // Order: stop current sequence -> disarm (uncheck "Det" + stop capturing) -> react.
                // The reaction runs after the loop breaks, so moving into a crowd can't re-trigger.
                let name = hit_name.take().unwrap_or_default();
                let stopped = stop_current_playback();
                notify_hit(); // uncheck "Det"
                fired = Some((name, stopped));
                break; // stop capturing BEFORE the reaction runs
            }
        }
    }

    // Capture has stopped. NOW run the reaction (key or sequence) — detection is already disarmed,
    // so a reaction that moves you into a crowd cannot re-trigger it.
    if let Some((name, stopped)) = fired {
        let action = react_action(vk);
        dlog(&format!(
            "[t+{:.1}s] *** PLAYER DETECTED: {} *** \u{2014} disarmed; {}{} (re-check 'Det' to re-arm)",
            start.elapsed().as_secs_f32(),
            name,
            if stopped { "stopped running sequence; " } else { "" },
            action
        ));
    }

    if PERM_DIRTY.load(Ordering::Acquire) {
        save_permanent(); // persist anything logged since the last tick
    }
    dlog("=== Cadence Proximity stopped. ===");
    pc.close(handle);
    Ok(())
}

/// Parse Ethernet/IPv4/TCP. Returns (src_ip, dst_ip, src_port, dst_port, payload).
pub(crate) fn parse_tcp(pkt: &[u8]) -> Option<(u32, u32, u16, u16, &[u8])> {
    if pkt.len() < 14 + 20 {
        return None;
    }
    let ethertype = u16::from_be_bytes([pkt[12], pkt[13]]);
    if ethertype != 0x0800 {
        return None; // not IPv4
    }
    let ip = &pkt[14..];
    if (ip[0] >> 4) != 4 {
        return None;
    }
    let ihl = ((ip[0] & 0x0f) as usize) * 4;
    if ip.len() < ihl + 20 || ip[9] != 6 {
        return None; // not TCP
    }
    let src_ip = u32::from_be_bytes([ip[12], ip[13], ip[14], ip[15]]);
    let dst_ip = u32::from_be_bytes([ip[16], ip[17], ip[18], ip[19]]);
    let tcp = &ip[ihl..];
    let src_port = u16::from_be_bytes([tcp[0], tcp[1]]);
    let dst_port = u16::from_be_bytes([tcp[2], tcp[3]]);
    let data_off = ((tcp[12] >> 4) as usize) * 4;
    if tcp.len() <= data_off {
        return None;
    }
    Some((src_ip, dst_ip, src_port, dst_port, &tcp[data_off..]))
}

/// Server->client payload for a specific server IP: (flow_key, server_port, payload).
#[cfg(test)]
fn extract_server_payload(pkt: &[u8], server_ip: u32) -> Option<(u64, u16, &[u8])> {
    let (src_ip, _dst_ip, src_port, dst_port, payload) = parse_tcp(pkt)?;
    if src_ip != server_ip || payload.is_empty() {
        return None;
    }
    Some((((src_port as u64) << 16) | dst_port as u64, src_port, payload))
}

pub(crate) fn fmt_ip(ip: u32) -> String {
    format!("{}.{}.{}.{}", (ip >> 24) & 0xff, (ip >> 16) & 0xff, (ip >> 8) & 0xff, ip & 0xff)
}

/// Find a printable, null-terminated name-like string within the first bytes of an inner
/// message payload. Used to identify player/vendor-spawn opcodes on an uncalibrated server.
fn scan_name(msg: &[u8]) -> Option<String> {
    let hi = msg.len().min(48);
    let mut i = 8; // skip [dwSize][nType]
    while i < hi {
        let mut j = i;
        while j < msg.len() && j - i < 24 && (0x20..0x7f).contains(&msg[j]) {
            j += 1;
        }
        if j < msg.len() && msg[j] == 0 && j - i >= 3 {
            let s = &msg[i..j];
            if s.iter().any(|c| c.is_ascii_alphabetic()) {
                return Some(String::from_utf8_lossy(s).into_owned());
            }
        }
        i += 1;
    }
    None
}

/// Consume complete NET_COMPRESS envelopes from a flow buffer; retain the trailing remainder.
/// Calls `on_msg(opcode, message)` for every inner message. Returns envelopes consumed.
pub(crate) fn parse_flow(buf: &mut Vec<u8>, on_msg: &mut dyn FnMut(u32, &[u8])) -> usize {
    let mut start = 0usize;
    let mut envelopes = 0usize;
    while buf.len() - start >= ENVELOPE_HEADER {
        let dw = u32::from_le_bytes(buf[start..start + 4].try_into().unwrap());
        let ntype = u32::from_le_bytes(buf[start + 4..start + 8].try_into().unwrap());
        let bcompress = u32::from_le_bytes(buf[start + 8..start + 12].try_into().unwrap());
        if ntype != NET_MSG_COMPRESS || dw < ENVELOPE_HEADER as u32 || dw > MAX_ENVELOPE {
            start += 1; // resync
            continue;
        }
        let size = dw as usize;
        if buf.len() - start < size {
            break; // envelope spans more segments; wait
        }
        let body = &buf[start + ENVELOPE_HEADER..start + size];
        let decoded = if bcompress != 0 {
            match lzo1x_decompress(body) {
                Some(d) => d,
                None => {
                    start += size;
                    continue;
                }
            }
        } else {
            body.to_vec()
        };
        let mut b = 0usize;
        while b + 8 <= decoded.len() {
            let isz = u32::from_le_bytes(decoded[b..b + 4].try_into().unwrap()) as usize;
            let itype = u32::from_le_bytes(decoded[b + 4..b + 8].try_into().unwrap());
            if isz < 8 || b + isz > decoded.len() {
                break;
            }
            on_msg(itype, &decoded[b..b + isz]);
            b += isz;
        }
        envelopes += 1;
        start += size;
    }
    if start > 0 {
        buf.drain(0..start);
    }
    envelopes
}

/// Player name: null-terminated ASCII at payload offset 4 (a leading dword precedes szName in
/// this build). Falls back to "<player>" if it can't be read cleanly.
fn read_name(payload: &[u8]) -> String {
    let off = 4;
    if payload.len() <= off {
        return "<player>".to_string();
    }
    let mut end = off;
    while end < payload.len() && end - off < 33 && payload[end] != 0 {
        end += 1;
    }
    let raw = &payload[off..end];
    if !raw.is_empty() && raw.iter().all(|&c| (0x20..0x7f).contains(&c)) {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        "<player>".to_string()
    }
}

/// Player level from a DROP_PC (SDROP_CHAR) payload. `wLevel` (WORD) sits 64 bytes past the start
/// of the szName field, which begins at payload offset 4 in this build — so offset 68. Layout
/// (from GLCharData.h): szName[33] + pad(3) + emTribe/emClass (int×2) + wSchool/wHair/wHairColor/
/// wFace/wSex (WORD×5) + pad(2) + nBright(int) + dwCharID(DWORD) + wLevel. Returns None if the
/// value is out of a sane range (guards against reading past a truncated message).
fn read_level(payload: &[u8]) -> Option<u16> {
    const OFF: usize = 68;
    let b = payload.get(OFF..OFF + 2)?;
    let lvl = u16::from_le_bytes([b[0], b[1]]);
    if (1..=999).contains(&lvl) {
        Some(lvl)
    } else {
        None
    }
}

fn ru16(p: &[u8], o: usize) -> Option<u16> {
    p.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}
fn rf32(p: &[u8], o: usize) -> Option<f32> {
    p.get(o..o + 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
/// Null-terminated printable ASCII of at most `max` bytes at offset `o`; empty if the run isn't
/// clean text (guards against reading struct padding as a name).
fn rstr(p: &[u8], o: usize, max: usize) -> String {
    let end = (o + max).min(p.len());
    let mut s = String::new();
    for i in o..end {
        let c = p[i];
        if c == 0 {
            break;
        }
        if (0x20..0x7f).contains(&c) {
            s.push(c as char);
        } else {
            return String::new();
        }
    }
    s
}

/// Parse the fields we surface from a DROP_PC (SDROP_CHAR) payload. Offsets verified against a real
/// capture (name@4, level@68, class@44, school@48, guild@80, HP@168/170, map@176, pos@184/192).
/// See GLCharData.h `SDROP_CHAR`; every field is validated so a garbled read yields None/empty.
fn parse_pc(payload: &[u8]) -> PcInfo {
    // Class is a single-bit emClass flag; reject 0/multi-bit reads.
    let class = ru16(payload, 44).filter(|c| c.count_ones() == 1);
    let school = ru16(payload, 48).filter(|s| *s <= 2);
    let (hp_now, hp_max) = match (ru16(payload, 168), ru16(payload, 170)) {
        (Some(n), Some(m)) if m > 0 && n <= m => (Some(n), Some(m)),
        _ => (None, None),
    };
    let map = ru16(payload, 176).filter(|m| *m != 0xFFFF);
    let (pos_x, pos_z) = match (rf32(payload, 184), rf32(payload, 192)) {
        (Some(x), Some(z)) if x.is_finite() && z.is_finite() && x.abs() < 1.0e6 && z.abs() < 1.0e6 => {
            (Some(x.round() as i32), Some(z.round() as i32))
        }
        _ => (None, None),
    };
    PcInfo {
        name: read_name(payload),
        level: read_level(payload),
        class,
        school,
        guild: rstr(payload, 80, 32),
        hp_now,
        hp_max,
        map,
        pos_x,
        pos_z,
    }
}

// ---------------------------------------------------------------------------------------------
// LZO1X decompressor (pure Rust port of the validated ran_sniff/lzo1x.py)
// ---------------------------------------------------------------------------------------------

fn lzo1x_decompress(src: &[u8]) -> Option<Vec<u8>> {
    #[derive(Clone, Copy)]
    enum L {
        OuterTop,
        FirstLiteralRun,
        Match,
        MatchDone,
        MatchNext,
    }
    let mut out: Vec<u8> = Vec::with_capacity(src.len() * 3);
    let mut ip = 0usize;
    let mut t: usize;

    let first = *src.get(ip)?;
    let mut label = if first > 17 {
        t = first as usize - 17;
        ip += 1;
        if t < 4 {
            L::MatchNext
        } else {
            let end = ip.checked_add(t)?;
            out.extend_from_slice(src.get(ip..end)?);
            ip = end;
            L::FirstLiteralRun
        }
    } else {
        t = 0;
        L::OuterTop
    };

    let copy_match = |out: &mut Vec<u8>, m_pos: isize, count: usize| -> Option<()> {
        if m_pos < 0 {
            return None;
        }
        let mut mp = m_pos as usize;
        for _ in 0..count {
            let v = *out.get(mp)?;
            out.push(v);
            mp += 1;
        }
        Some(())
    };

    loop {
        match label {
            L::OuterTop => {
                t = *src.get(ip)? as usize;
                ip += 1;
                if t >= 16 {
                    label = L::Match;
                    continue;
                }
                if t == 0 {
                    while *src.get(ip)? == 0 {
                        t += 255;
                        ip += 1;
                    }
                    t += 15 + *src.get(ip)? as usize;
                    ip += 1;
                }
                let n = t + 3;
                let end = ip.checked_add(n)?;
                out.extend_from_slice(src.get(ip..end)?);
                ip = end;
                label = L::FirstLiteralRun;
            }
            L::FirstLiteralRun => {
                t = *src.get(ip)? as usize;
                ip += 1;
                if t >= 16 {
                    label = L::Match;
                    continue;
                }
                let mut m_pos = out.len() as isize - (1 + 0x0800);
                m_pos -= (t >> 2) as isize;
                m_pos -= (*src.get(ip)? as isize) << 2;
                ip += 1;
                if m_pos < 0 {
                    return None;
                }
                let mp = m_pos as usize;
                let (a, b, c) = (*out.get(mp)?, *out.get(mp + 1)?, *out.get(mp + 2)?);
                out.push(a);
                out.push(b);
                out.push(c);
                label = L::MatchDone;
            }
            L::Match => {
                if t >= 64 {
                    let mut m_pos = out.len() as isize - 1;
                    m_pos -= ((t >> 2) & 7) as isize;
                    m_pos -= (*src.get(ip)? as isize) << 3;
                    ip += 1;
                    t = (t >> 5) - 1;
                    copy_match(&mut out, m_pos, t + 2)?;
                    label = L::MatchDone;
                } else if t >= 32 {
                    t &= 31;
                    if t == 0 {
                        while *src.get(ip)? == 0 {
                            t += 255;
                            ip += 1;
                        }
                        t += 31 + *src.get(ip)? as usize;
                        ip += 1;
                    }
                    let mut m_pos = out.len() as isize - 1;
                    let d = *src.get(ip)? as isize | ((*src.get(ip + 1)? as isize) << 8);
                    ip += 2;
                    m_pos -= d >> 2;
                    copy_match(&mut out, m_pos, t + 2)?;
                    label = L::MatchDone;
                } else if t >= 16 {
                    let mut m_pos = out.len() as isize;
                    m_pos -= ((t & 8) << 11) as isize;
                    t &= 7;
                    if t == 0 {
                        while *src.get(ip)? == 0 {
                            t += 255;
                            ip += 1;
                        }
                        t += 7 + *src.get(ip)? as usize;
                        ip += 1;
                    }
                    let d = *src.get(ip)? as isize | ((*src.get(ip + 1)? as isize) << 8);
                    ip += 2;
                    m_pos -= d >> 2;
                    if m_pos == out.len() as isize {
                        break; // end-of-stream
                    }
                    m_pos -= 0x4000;
                    copy_match(&mut out, m_pos, t + 2)?;
                    label = L::MatchDone;
                } else {
                    let mut m_pos = out.len() as isize - 1;
                    m_pos -= (t >> 2) as isize;
                    m_pos -= (*src.get(ip)? as isize) << 2;
                    ip += 1;
                    if m_pos < 0 {
                        return None;
                    }
                    let mp = m_pos as usize;
                    let (a, b) = (*out.get(mp)?, *out.get(mp + 1)?);
                    out.push(a);
                    out.push(b);
                    label = L::MatchDone;
                }
            }
            L::MatchDone => {
                if ip < 2 {
                    return None;
                }
                t = (src[ip - 2] & 3) as usize;
                label = if t == 0 { L::OuterTop } else { L::MatchNext };
            }
            L::MatchNext => {
                out.push(*src.get(ip)?);
                ip += 1;
                for _ in 0..t.saturating_sub(1) {
                    out.push(*src.get(ip)?);
                    ip += 1;
                }
                t = *src.get(ip)? as usize;
                ip += 1;
                label = L::Match;
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Minimal dynamic binding to wpcap.dll (Npcap) — avoids a build-time SDK dependency.
// ---------------------------------------------------------------------------------------------

const PCAP_ERRBUF_SIZE: usize = 256;
const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x00000008;

#[repr(C)]
struct PcapIf {
    next: *mut PcapIf,
    name: *mut c_char,
    description: *mut c_char,
    addresses: *mut c_void,
    flags: u32,
}

#[repr(C)]
struct PcapPkthdr {
    ts_sec: c_long,
    ts_usec: c_long,
    caplen: u32,
    len: u32,
}

#[repr(C)]
struct BpfProgram {
    bf_len: u32,
    bf_insns: *mut c_void,
}

pub(crate) type PcapT = *mut c_void;
type FnFindAllDevs = unsafe extern "C" fn(*mut *mut PcapIf, *mut c_char) -> c_int;
type FnFreeAllDevs = unsafe extern "C" fn(*mut PcapIf);
type FnOpenLive =
    unsafe extern "C" fn(*const c_char, c_int, c_int, c_int, *mut c_char) -> PcapT;
type FnCompile =
    unsafe extern "C" fn(PcapT, *mut BpfProgram, *const c_char, c_int, u32) -> c_int;
type FnSetFilter = unsafe extern "C" fn(PcapT, *mut BpfProgram) -> c_int;
type FnFreeCode = unsafe extern "C" fn(*mut BpfProgram);
type FnNextEx = unsafe extern "C" fn(PcapT, *mut *mut PcapPkthdr, *mut *const u8) -> c_int;
type FnClose = unsafe extern "C" fn(PcapT);

pub(crate) struct Pcap {
    _lib: *mut c_void,
    findalldevs: FnFindAllDevs,
    freealldevs: FnFreeAllDevs,
    open_live: FnOpenLive,
    compile: FnCompile,
    setfilter: FnSetFilter,
    freecode: FnFreeCode,
    next_ex: FnNextEx,
    close: FnClose,
    // Scratch packet returned by next_packet; kept alive between calls.
    scratch: Mutex<Vec<u8>>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Pcap {
    pub(crate) fn load() -> Result<Pcap, String> {
        // Prefer the Npcap folder inside System32, then a WinPcap-compat install, then
        // hardcoded fallbacks and the default search path. Report each attempt so a missing
        // Npcap (e.g. inside a VM) is unambiguous.
        let mut candidates: Vec<String> = Vec::new();
        let mut sysdir = [0u16; 260];
        let n = unsafe { GetSystemDirectoryW(sysdir.as_mut_ptr(), sysdir.len() as u32) } as usize;
        if n > 0 && n <= sysdir.len() {
            let sys = String::from_utf16_lossy(&sysdir[..n]);
            candidates.push(format!("{}\\Npcap\\wpcap.dll", sys));
            candidates.push(format!("{}\\wpcap.dll", sys));
        }
        candidates.push(r"C:\Windows\System32\Npcap\wpcap.dll".to_string());
        candidates.push("wpcap.dll".to_string()); // default DLL search path

        let mut lib: *mut c_void = std::ptr::null_mut();
        let mut report = String::new();
        for cand in &candidates {
            let w = wide(cand);
            let mut h = unsafe {
                LoadLibraryExW(w.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH)
            };
            if h.is_null() {
                h = unsafe { LoadLibraryW(w.as_ptr()) };
            }
            if !h.is_null() {
                lib = h as *mut c_void;
                break;
            }
            let err = unsafe { GetLastError() };
            report.push_str(&format!("\n  tried {} -> WinError {}", cand, err));
        }
        if lib.is_null() {
            return Err(format!(
                "Npcap wpcap.dll could not be loaded. Install Npcap from npcap.com IN THE ENVIRONMENT WHERE CADENCE RUNS \
                 (if the game/Cadence run inside a VM, install Npcap inside that VM), then reboot.{}",
                report
            ));
        }

        unsafe fn sym(lib: *mut c_void, name: &str) -> Result<*const c_void, String> {
            let cname = std::ffi::CString::new(name).unwrap();
            let p = GetProcAddress(lib as _, cname.as_ptr());
            if p.is_null() {
                Err(format!("wpcap.dll missing export {}", name))
            } else {
                Ok(p as *const c_void)
            }
        }

        unsafe {
            Ok(Pcap {
                _lib: lib,
                findalldevs: std::mem::transmute::<_, FnFindAllDevs>(sym(lib, "pcap_findalldevs")?),
                freealldevs: std::mem::transmute::<_, FnFreeAllDevs>(sym(lib, "pcap_freealldevs")?),
                open_live: std::mem::transmute::<_, FnOpenLive>(sym(lib, "pcap_open_live")?),
                compile: std::mem::transmute::<_, FnCompile>(sym(lib, "pcap_compile")?),
                setfilter: std::mem::transmute::<_, FnSetFilter>(sym(lib, "pcap_setfilter")?),
                freecode: std::mem::transmute::<_, FnFreeCode>(sym(lib, "pcap_freecode")?),
                next_ex: std::mem::transmute::<_, FnNextEx>(sym(lib, "pcap_next_ex")?),
                close: std::mem::transmute::<_, FnClose>(sym(lib, "pcap_close")?),
                scratch: Mutex::new(Vec::new()),
            })
        }
    }

    /// Enumerate capture devices as (name, description).
    pub fn list_devices(&self) -> Vec<(String, String)> {
        let mut devs: Vec<(String, String)> = Vec::new();
        let mut alldevs: *mut PcapIf = std::ptr::null_mut();
        let mut errbuf = [0i8; PCAP_ERRBUF_SIZE];
        unsafe {
            if (self.findalldevs)(&mut alldevs, errbuf.as_mut_ptr()) != 0 {
                return devs;
            }
            let mut cur = alldevs;
            while !cur.is_null() {
                let name = cstr((*cur).name);
                let desc = cstr((*cur).description);
                if !name.is_empty() {
                    devs.push((name, desc));
                }
                cur = (*cur).next;
            }
            (self.freealldevs)(alldevs);
        }
        devs
    }

    pub(crate) fn first_device(&self) -> Result<String, String> {
        let devs = self.list_devices();
        // Skip pseudo/virtual adapters that never carry the game traffic.
        let bad = [
            "loopback", "wan miniport", "virtual", "bluetooth", "vmware",
            "vethernet", "tailscale", "tap-windows", "hyper-v",
        ];
        for (name, desc) in &devs {
            let d = desc.to_lowercase();
            if !bad.iter().any(|b| d.contains(b)) {
                return Ok(name.clone());
            }
        }
        devs.into_iter()
            .map(|(n, _)| n)
            .next()
            .ok_or_else(|| "no capture devices found".to_string())
    }

    pub(crate) fn open_live(&self, dev: &str, to_ms: i32) -> Result<PcapT, String> {
        let cdev = std::ffi::CString::new(dev).map_err(|_| "bad device name".to_string())?;
        let mut errbuf = [0i8; PCAP_ERRBUF_SIZE];
        // promisc=0: we only need this host's own traffic, and promiscuous mode is unreliable
        // (often fails) on Wi-Fi adapters.
        let h = unsafe { (self.open_live)(cdev.as_ptr(), 65535, 0, to_ms, errbuf.as_mut_ptr()) };
        if h.is_null() {
            return Err(format!("pcap_open_live failed: {}", cstr(errbuf.as_ptr())));
        }
        Ok(h)
    }

    pub(crate) fn set_filter(&self, handle: PcapT, filter: &str) -> Result<(), String> {
        let cfilter = std::ffi::CString::new(filter).map_err(|_| "bad filter".to_string())?;
        let mut prog: BpfProgram = unsafe { std::mem::zeroed() };
        unsafe {
            if (self.compile)(handle, &mut prog, cfilter.as_ptr(), 1, 0xffffffff) != 0 {
                return Err("pcap_compile failed".to_string());
            }
            let rc = (self.setfilter)(handle, &mut prog);
            (self.freecode)(&mut prog);
            if rc != 0 {
                return Err("pcap_setfilter failed".to_string());
            }
        }
        Ok(())
    }

    /// Returns Ok(Some(pkt)) with the captured bytes, Ok(None) on read timeout, Err on error.
    pub(crate) fn next_packet(&self, handle: PcapT) -> Result<Option<&[u8]>, ()> {
        let mut hdr: *mut PcapPkthdr = std::ptr::null_mut();
        let mut data: *const u8 = std::ptr::null();
        let rc = unsafe { (self.next_ex)(handle, &mut hdr, &mut data) };
        match rc {
            1 => unsafe {
                let caplen = (*hdr).caplen as usize;
                let mut scratch = self.scratch.lock().unwrap();
                scratch.clear();
                scratch.extend_from_slice(std::slice::from_raw_parts(data, caplen));
                // Safety: scratch lives in self; returned slice is used before the next call.
                Ok(Some(std::slice::from_raw_parts(scratch.as_ptr(), scratch.len())))
            },
            0 => Ok(None),
            _ => Err(()),
        }
    }

    pub(crate) fn close(&self, handle: PcapT) {
        unsafe { (self.close)(handle) };
    }
}

unsafe impl Send for Pcap {}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // A real compressed DROP_PC envelope body (LZO1X) captured from real_market.txt, and the
    // player name it must decode to. Guards the LZO port + framing against regressions.
    #[test]
    fn decodes_drop_pc_from_real_capture() {
        let body = hex(TEST_ENVELOPE_BODY_HEX);
        let decoded = lzo1x_decompress(&body).expect("lzo decode");
        // Test vector is from the 977-base server (DROP_PC opcode 3000), independent of the
        // configured NET_MSG_BASE.
        let mut got = None;
        let mut b = 0usize;
        while b + 8 <= decoded.len() {
            let isz = u32::from_le_bytes(decoded[b..b + 4].try_into().unwrap()) as usize;
            let itype = u32::from_le_bytes(decoded[b + 4..b + 8].try_into().unwrap());
            if isz < 8 || b + isz > decoded.len() {
                break;
            }
            if itype == 3000 {
                got = Some(read_name(&decoded[b + 8..]));
                // The same capture carries the full character block; assert every field offset.
                let info = parse_pc(&decoded[b + 8..]);
                assert_eq!(info.level, Some(16));
                assert_eq!(info.class, Some(2)); // GLCC_SWORDSMAN_M
                assert_eq!(info.school, Some(2)); // Phoenix
                assert_eq!((info.hp_now, info.hp_max), (Some(199), Some(199)));
                assert_eq!(info.map, Some(22));
                assert_eq!((info.pos_x, info.pos_z), (Some(-53), Some(-48)));
            }
            b += isz;
        }
        assert_eq!(got.as_deref(), Some(TEST_EXPECTED_NAME));
    }

    #[test]
    fn parse_flow_decodes_full_envelope() {
        // Wrap the compressed body in a NET_COMPRESS envelope: [dwSize][170][bCompress=1][body].
        let body = hex(TEST_ENVELOPE_BODY_HEX);
        let dw = (body.len() + ENVELOPE_HEADER) as u32;
        let mut env = Vec::new();
        env.extend_from_slice(&dw.to_le_bytes());
        env.extend_from_slice(&NET_MSG_COMPRESS.to_le_bytes());
        env.extend_from_slice(&1u32.to_le_bytes());
        env.extend_from_slice(&body);
        // Prepend a garbage byte to exercise the resync path, and split across a partial feed.
        let mut buf = vec![0xEEu8];
        buf.extend_from_slice(&env[..10]);
        let mut hits: Vec<String> = Vec::new();
        parse_flow(&mut buf, &mut |op, msg| if op == 3000 { hits.push(read_name(&msg[8..])) }); // incomplete
        assert!(hits.is_empty());
        buf.extend_from_slice(&env[10..]); // deliver the rest
        parse_flow(&mut buf, &mut |op, msg| if op == 3000 { hits.push(read_name(&msg[8..])) });
        assert_eq!(hits, vec![TEST_EXPECTED_NAME.to_string()]);
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn extracts_game_port_payload() {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&[0u8; 12]); // eth dst+src MAC
        pkt.extend_from_slice(&[0x08, 0x00]); // ethertype IPv4
        // IPv4 header (ihl=5)
        pkt.push(0x45);
        pkt.push(0x00);
        pkt.extend_from_slice(&[0, 40]); // total length
        pkt.extend_from_slice(&[0, 0, 0, 0]); // id + flags
        pkt.push(64); // ttl
        pkt.push(6); // proto TCP
        pkt.extend_from_slice(&[0, 0]); // checksum
        pkt.extend_from_slice(&[143, 14, 88, 19]); // src ip (server)
        pkt.extend_from_slice(&[192, 168, 0, 117]); // dst ip (client)
        // TCP header (data offset 5)
        pkt.extend_from_slice(&GAME_PORT.to_be_bytes()); // src port 7112
        pkt.extend_from_slice(&50000u16.to_be_bytes()); // dst port
        pkt.extend_from_slice(&[0, 0, 0, 0]); // seq
        pkt.extend_from_slice(&[0, 0, 0, 0]); // ack
        pkt.push(0x50); // data offset = 5 words
        pkt.push(0x18); // flags
        pkt.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // win, csum, urg
        pkt.extend_from_slice(&[0xAA, 0xBB, 0xCC]); // payload

        let server = parse_ipv4("143.14.88.19");
        let got = extract_server_payload(&pkt, server);
        assert!(got.is_some());
        let (_, src_port, payload) = got.unwrap();
        assert_eq!(src_port, GAME_PORT);
        assert_eq!(payload, &[0xAA, 0xBB, 0xCC]);

        // A packet not from the server IP must be ignored.
        pkt[14 + 12] = 1; // change src IP first octet
        assert!(extract_server_payload(&pkt, server).is_none());
    }

    // Integration check for the dynamic wpcap.dll binding (LoadLibraryEx + GetProcAddress +
    // pcap_findalldevs). Never panics; prints the devices so live-capture setup can be verified.
    #[test]
    fn wpcap_binding_enumerates_devices() {
        let devices = list_devices();
        println!("wpcap device count: {}", devices.len());
        for (name, desc) in &devices {
            println!("  {}  |  {}", name, desc);
        }
    }

    // Compressed body (LZO1X) of a real DROP_PC envelope from real_market.txt, name "-1st".
    const TEST_ENVELOPE_BODY_HEX: &str = "0e80060000b80b0000010000002d31737400200200000c0200000002000400027e03000900018c070585240100100000802002f8002011880001b64500ffc000000ec700c700210000001600000093000000977c54c28716993e68aa3ec2bb6679bf700b0148ff66be7d00142d7c00c808200418007c07ec00e0066c242e4c006c212e4d0003e0025c0fed0704274c00e702ffff05274d0000277d0306314d0007314d0008274c00283d0109314d000a274c00288d010b274e00e02af5070c274c00289d000d274e00c1c3fc04e8027c01fc216c01fc206c01fc1f6c017c057c006c057c007c047c006c047c00285c04640520150c007c082a0c003c140101803f00006c00d704ffffffc401371800ed0b008405050100ff7fff7f5c005c00ed07ff4c26017fff7f552a4c006d3f7f4405d007b00b4c064d0238319f000300028c408d0cffbc04c807940e647455097f3f4d00642dec006572f7314d001d2d4d00d7516a7e2d4c0002d6000000fa2d4c007d09802d4d001d51715b2d4d008e31ed00fc70222d9c00904c2d4d002028ec007c266c1efc006c016e0000686402bc2e7c016d0000d0622b9c046c18780e2a2c0602fe000000c52ded00e34d020d2d4c00690c682d4d00dc5002cc024401380b00680013387000204b640003803f00f3912f200cc401022bbd0b2848644e7cbc2ee600192c64020036366436663538666138303437353561313161306237303834663662306466396531363239366461616530656266653962643231653938363034333864333339394200c42effbc4500110000";
    const TEST_EXPECTED_NAME: &str = "-1st";
}
