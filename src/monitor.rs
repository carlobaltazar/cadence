use crate::player;
use crate::timing::PrecisionTimer;
use crate::win32_helpers::lock_or_recover;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use winapi::shared::windef::HWND;
use winapi::um::wingdi::GetPixel;
use winapi::um::winuser::*;

/// The pixels this monitor watches. All sit on the same game window (shared anchor).
/// HP/MP/SP press a fixed key when their pixel drifts off-color. `Skill` is the Idle
/// Guard's slot: it never presses anything (see the Idle Guard block below).
#[derive(Clone, Copy)]
pub enum Bar {
    Hp = 0,
    Mp = 1,
    Sp = 2,
    Skill = 3,
}

const BAR_COUNT: usize = 4;
const CLR_INVALID: u32 = 0xFFFFFFFF;
const COLOR_TOLERANCE: u32 = 48; // Manhattan distance across BGR channels
const POLL_INTERVAL_MICROS: i64 = 10_000; // ~10ms tick — detects a drop within ~10ms
// Minimum gap between presses for a single bar. The poll is tight so detection is
// near-instant, but firing is paced so each heal lands before the next press
// (avoids flooding the game with ~100 presses/sec).
const FIRE_COOLDOWN: Duration = Duration::from_millis(120);

// Fixed keys per bar: HP->Q, MP->W, SP->E. 0 = observe only (Skill slot).
const BAR_VK: [u16; BAR_COUNT] = [0x51, 0x57, 0x45, 0];
const BAR_NAME: [&str; BAR_COUNT] = ["HP", "MP", "SP", "SKILL"];

// --- Idle Guard ------------------------------------------------------------------------------
// When the bag is full the game parks an item on the cursor; playback keeps pressing keys but
// no skill fires, and the HP proxy sees nothing until the character actually dies. The guard
// watches one pixel on a quickbar skill icon whose reference colour was sampled WHILE the skill
// showed its cooldown overlay (the hotbar animates constantly, so plain pixel change is not a
// usable signal). Each tick where the pixel matches that colour = "a skill was just used".
// The `Skill` slot therefore reuses the bar machinery with the meaning inverted: `low` (off the
// sampled colour) is the quiet state; `!low` is activity. No cooldown seen for IDLE_AFTER_MS
// while a sequence is playing => idle. Ticks that could not sample (game window missing / not
// foreground) and ticks while nothing is playing reset the streak: a gap must never read as idle.
static IDLE_AFTER_MS: AtomicU64 = AtomicU64::new(60_000);
/// Wall-clock ms when the current "no cooldown seen while playing" streak began; 0 = none.
static SKILL_NOMATCH_SINCE_MS: AtomicU64 = AtomicU64::new(0);

// --- Pet Guard ------------------------------------------------------------------------------
// The pet is the character's regen source; with it out, MP/SP regen so fast the "low" pixel is
// unreachable. So MP or SP reading low means the pet is NOT out (a hide/call toggle got dropped by
// lag, or a hungry pet auto-hid). The guard presses the pet toggle key to call it back, and flags
// "pet might be hungry" for the dashboard if the bar stays low anyway.
const PET_VK: u16 = 0x41; // 'A' -- same pet call/hide toggle pet_cycle.rs uses
// Low must persist this long before the first press: a one-frame glitch or a loading screen reads
// off-color too, and toggling a healthy pet would HIDE it.
const PET_CALL_DEBOUNCE: Duration = Duration::from_millis(1500);
// Re-press while still low, in case the press itself was eaten by the same lag.
const PET_CALL_RETRY: Duration = Duration::from_secs(8);

static PET_GUARD: AtomicBool = AtomicBool::new(false);
static PET_HUNGRY_AFTER_MS: AtomicU64 = AtomicU64::new(5000);
/// Wall-clock ms when the current guard-relevant low episode began; 0 = not low.
static PET_LOW_SINCE_MS: AtomicU64 = AtomicU64::new(0);
/// Pet-toggle presses sent during the current episode.
static PET_CALLS: AtomicU32 = AtomicU32::new(0);
/// Wall-clock ms of the FIRST pet-toggle press in this episode; 0 = none yet.
/// "Hungry" is measured from here: still low N s after we actually called the pet.
/// (Low-with-no-call is a held state — HP low too, i.e. loading screen / death.)
static PET_FIRST_CALL_MS: AtomicU64 = AtomicU64::new(0);

struct BarState {
    enabled: AtomicBool,
    x: AtomicI32,
    y: AtomicI32,
    ref_color: AtomicU32,
}

impl BarState {
    const fn new() -> Self {
        BarState {
            enabled: AtomicBool::new(false),
            x: AtomicI32::new(0),
            y: AtomicI32::new(0),
            ref_color: AtomicU32::new(0),
        }
    }
}

static BARS: [BarState; BAR_COUNT] =
    [BarState::new(), BarState::new(), BarState::new(), BarState::new()];

/// Passive per-bar observations for the reporting thread: when the pixel was
/// last sampled and whether it currently reads off-color ("low"). Written by
/// the worker loop below, read by `snapshot()`. Independent of FIRE_COOLDOWN —
/// this records what the bar looks like, not when we healed.
struct BarObs {
    last_sample_ms: AtomicU64,
    low: AtomicBool,
    low_since_ms: AtomicU64, // 0 = not currently low
}

impl BarObs {
    const fn new() -> Self {
        BarObs {
            last_sample_ms: AtomicU64::new(0),
            low: AtomicBool::new(false),
            low_since_ms: AtomicU64::new(0),
        }
    }
}

static OBS: [BarObs; BAR_COUNT] = [BarObs::new(), BarObs::new(), BarObs::new(), BarObs::new()];

/// Whether the anchored game window currently exists (regardless of focus).
/// Meaningful only while the worker thread runs with a window anchor.
static WINDOW_FOUND: AtomicBool = AtomicBool::new(true);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy)]
pub struct BarInfo {
    pub enabled: bool,
    pub low: bool,
    pub low_since_ms: u64,
    pub last_sample_ms: u64,
}

#[derive(Clone, Copy)]
pub struct BarsSnapshot {
    pub window_found: bool,
    pub bars: [BarInfo; BAR_COUNT],
    /// Pet Guard is switched on (regardless of whether MP/SP monitors are enabled).
    pub pet_guard: bool,
    /// Wall-clock ms when the guard's low episode began; 0 = not low.
    pub pet_low_since_ms: u64,
    /// Still low past the configured threshold AFTER the pet was called.
    pub pet_hungry: bool,
    /// Pet-toggle presses in the current episode.
    pub pet_calls: u32,
    /// Idle Guard: the skill pixel is enabled, a sequence is playing, and no cooldown colour
    /// has been seen for the configured window.
    pub skill_idle: bool,
    /// Wall-clock ms when the current no-cooldown streak began; 0 = none.
    pub skill_nomatch_since_ms: u64,
}

/// Point-in-time view of the bar observations, for the reporting thread.
pub fn snapshot() -> BarsSnapshot {
    let mut bars = [BarInfo {
        enabled: false,
        low: false,
        low_since_ms: 0,
        last_sample_ms: 0,
    }; BAR_COUNT];
    for i in 0..BAR_COUNT {
        bars[i] = BarInfo {
            enabled: BARS[i].enabled.load(Ordering::Acquire),
            low: OBS[i].low.load(Ordering::Acquire),
            low_since_ms: OBS[i].low_since_ms.load(Ordering::Acquire),
            last_sample_ms: OBS[i].last_sample_ms.load(Ordering::Acquire),
        };
    }
    let pet_low_since_ms = PET_LOW_SINCE_MS.load(Ordering::Acquire);
    let first_call_ms = PET_FIRST_CALL_MS.load(Ordering::Acquire);
    let now = now_ms();
    let pet_hungry = pet_low_since_ms > 0
        && first_call_ms > 0
        && now.saturating_sub(first_call_ms) >= PET_HUNGRY_AFTER_MS.load(Ordering::Acquire);
    let skill_nomatch_since_ms = SKILL_NOMATCH_SINCE_MS.load(Ordering::Acquire);
    let skill_idle = bars[Bar::Skill as usize].enabled
        && skill_nomatch_since_ms > 0
        && now.saturating_sub(skill_nomatch_since_ms) >= IDLE_AFTER_MS.load(Ordering::Acquire);
    BarsSnapshot {
        window_found: WINDOW_FOUND.load(Ordering::Acquire),
        bars,
        pet_guard: PET_GUARD.load(Ordering::Acquire),
        pet_low_since_ms,
        pet_hungry,
        pet_calls: PET_CALLS.load(Ordering::Acquire),
        skill_idle,
        skill_nomatch_since_ms,
    }
}

/// Set how long the skill pixel may go without showing its cooldown colour (while a
/// sequence plays) before the snapshot reports `skill_idle`. Takes effect on the next tick.
pub fn set_idle_guard(after_secs: u32) {
    IDLE_AFTER_MS.store(u64::from(after_secs.max(5)) * 1000, Ordering::Release);
}

/// Idle Guard is armed (= the Skill pixel slot is enabled).
pub fn idle_guard_enabled() -> bool {
    is_active(Bar::Skill)
}

/// Forget the current no-cooldown streak. Called on every tick that did not (or could not)
/// observe the skill pixel while playing, so gaps never accumulate into a false idle.
fn idle_streak_reset() {
    SKILL_NOMATCH_SINCE_MS.store(0, Ordering::Release);
}

/// Switch Pet Guard on/off and set how long MP/SP must stay low before the
/// snapshot reports `pet_hungry`. Takes effect on the next monitor tick.
pub fn set_pet_guard(enabled: bool, hungry_secs: u32) {
    PET_HUNGRY_AFTER_MS.store(u64::from(hungry_secs.max(1)) * 1000, Ordering::Release);
    let was = PET_GUARD.swap(enabled, Ordering::AcqRel);
    if !enabled {
        reset_pet_episode();
    }
    if was != enabled {
        println!(
            "[Cadence] Pet guard {} (hungry after {}s).",
            if enabled { "enabled" } else { "disabled" },
            hungry_secs
        );
    }
}

pub fn pet_guard_enabled() -> bool {
    PET_GUARD.load(Ordering::Acquire)
}

fn reset_pet_episode() {
    PET_LOW_SINCE_MS.store(0, Ordering::Release);
    PET_CALLS.store(0, Ordering::Release);
    PET_FIRST_CALL_MS.store(0, Ordering::Release);
}

// Shared game-window anchor (class, title) — all bars sample from this window.
static ANCHOR: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

#[inline]
fn color_dist(a: u32, b: u32) -> u32 {
    let ar = (a & 0xFF) as i32;
    let br = (b & 0xFF) as i32;
    let ag = ((a >> 8) & 0xFF) as i32;
    let bg = ((b >> 8) & 0xFF) as i32;
    let ab = ((a >> 16) & 0xFF) as i32;
    let bb = ((b >> 16) & 0xFF) as i32;
    (ar - br).unsigned_abs() + (ag - bg).unsigned_abs() + (ab - bb).unsigned_abs()
}

/// True if the given bar is currently enabled (used for toolbar status/state).
pub fn is_active(bar: Bar) -> bool {
    BARS[bar as usize].enabled.load(Ordering::Acquire)
}

/// True if any bar is enabled.
fn any_enabled() -> bool {
    BARS.iter().any(|b| b.enabled.load(Ordering::Acquire))
}

/// Enable a bar with its pixel coords + reference color, set the shared window
/// anchor, and spawn the worker thread if it isn't already running.
///
/// `window_class` empty means legacy absolute-screen-coord sampling (configs from
/// older versions); users should re-pick to anchor to the game window.
pub fn set_bar(
    bar: Bar,
    window_class: String,
    window_title: String,
    x: i32,
    y: i32,
    ref_color: u32,
) {
    {
        let mut anchor = lock_or_recover(&ANCHOR);
        *anchor = (window_class, window_title);
    }
    let b = &BARS[bar as usize];
    b.x.store(x, Ordering::Release);
    b.y.store(y, Ordering::Release);
    b.ref_color.store(ref_color, Ordering::Release);
    b.enabled.store(true, Ordering::Release);
    println!(
        "[Cadence] {} monitor enabled (cx={} cy={} ref=0x{:06X})",
        BAR_NAME[bar as usize], x, y, ref_color
    );
    ensure_running();
}

/// Disable a single bar. The worker thread keeps running for the other bars and
/// exits on its own once no bar is enabled.
pub fn disable_bar(bar: Bar) {
    BARS[bar as usize].enabled.store(false, Ordering::Release);
    println!("[Cadence] {} monitor disabled.", BAR_NAME[bar as usize]);
}

/// Stop the worker thread (and implicitly all bars stay configured but unsampled).
pub fn stop_all() {
    if ACTIVE.load(Ordering::Acquire) {
        CANCEL.store(true, Ordering::Release);
    }
}

fn ensure_running() {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        // Already running.
        return;
    }
    CANCEL.store(false, Ordering::Release);

    thread::spawn(move || {
        let timer = PrecisionTimer::new();

        // Per-bar scan codes computed once (0 for observe-only slots).
        let scans: [u16; BAR_COUNT] = {
            let mut s = [0u16; BAR_COUNT];
            for i in 0..BAR_COUNT {
                if BAR_VK[i] != 0 {
                    s[i] = player::scan_code(BAR_VK[i]);
                }
            }
            s
        };

        // Per-bar last-fire timestamps; start "armed" so the first drop fires immediately.
        let mut last_fire = [Instant::now() - FIRE_COOLDOWN; BAR_COUNT];

        // Pet Guard episode state (thread-local; the atomics above mirror it for snapshot()).
        let pet_scan = player::scan_code(PET_VK);
        let mut pet_low_start: Option<Instant> = None;
        let mut pet_last_call: Option<Instant> = None;
        // Idle Guard: whether the current streak has already been announced on the console.
        let mut idle_announced = false;

        loop {
            if CANCEL.load(Ordering::Acquire) {
                break;
            }
            if !any_enabled() {
                // Nothing to watch right now. Stay alive (avoids a respawn race
                // with set_bar) but idle cheaply until a bar is re-enabled.
                pet_low_start = None;
                pet_last_call = None;
                reset_pet_episode();
                idle_streak_reset();
                thread::sleep(Duration::from_millis(200));
                continue;
            }

            let (class, title) = lock_or_recover(&ANCHOR).clone();
            let use_window_anchor = !class.is_empty();

            unsafe {
                // Resolve the game window + DC ONCE per tick, then sample every
                // enabled bar against it. Shared anchor => one GetDC for all bars.
                // Every early-out below is a tick that observed nothing, so the
                // Idle Guard streak is reset on each of them.
                let (hwnd, hdc) = if use_window_anchor {
                    let hwnd = find_game_window(&class, &title);
                    WINDOW_FOUND.store(!hwnd.is_null(), Ordering::Release);
                    if hwnd.is_null() {
                        idle_streak_reset();
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    if GetForegroundWindow() != hwnd {
                        idle_streak_reset();
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    let hdc = GetDC(hwnd);
                    if hdc.is_null() {
                        idle_streak_reset();
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    (hwnd, hdc)
                } else {
                    let hdc = GetDC(std::ptr::null_mut());
                    if hdc.is_null() {
                        idle_streak_reset();
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    (std::ptr::null_mut(), hdc)
                };

                // Per-tick low readings (None = bar disabled or pixel unreadable this tick).
                let mut tick_low: [Option<bool>; BAR_COUNT] = [None; BAR_COUNT];

                for i in 0..BAR_COUNT {
                    let b = &BARS[i];
                    if !b.enabled.load(Ordering::Acquire) {
                        continue;
                    }
                    let x = b.x.load(Ordering::Acquire);
                    let y = b.y.load(Ordering::Acquire);
                    let ref_color = b.ref_color.load(Ordering::Acquire);
                    let color = GetPixel(hdc, x, y);
                    if color != CLR_INVALID {
                        let low = color_dist(color, ref_color) > COLOR_TOLERANCE;
                        tick_low[i] = Some(low);
                        let now = now_ms();
                        OBS[i].last_sample_ms.store(now, Ordering::Release);
                        OBS[i].low.store(low, Ordering::Release);
                        if low {
                            let _ = OBS[i].low_since_ms.compare_exchange(
                                0,
                                now,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            );
                        } else {
                            OBS[i].low_since_ms.store(0, Ordering::Release);
                        }
                        if low && BAR_VK[i] != 0 && last_fire[i].elapsed() >= FIRE_COOLDOWN {
                            // Bar is below threshold and the per-bar cooldown elapsed:
                            // fire once, then wait FIRE_COOLDOWN before the next press.
                            player::send_key_press(BAR_VK[i], scans[i]);
                            last_fire[i] = Instant::now();
                        }
                    }
                }

                ReleaseDC(hwnd, hdc);

                // --- Idle Guard ---
                // Skill pixel matching its sampled cooldown colour (= !low) is activity. A
                // no-match streak only accumulates while the pixel is actually observed AND a
                // sequence is playing; anything else forgets the streak.
                match tick_low[Bar::Skill as usize] {
                    Some(false) => {
                        idle_streak_reset();
                        idle_announced = false;
                    }
                    Some(true) if player::is_playing() => {
                        let now = now_ms();
                        let _ = SKILL_NOMATCH_SINCE_MS.compare_exchange(
                            0,
                            now,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        let since = SKILL_NOMATCH_SINCE_MS.load(Ordering::Acquire);
                        if !idle_announced
                            && since > 0
                            && now.saturating_sub(since) >= IDLE_AFTER_MS.load(Ordering::Acquire)
                        {
                            idle_announced = true;
                            println!(
                                "[Cadence] Idle guard: no skill cooldown for {}s while playing.",
                                now.saturating_sub(since) / 1000
                            );
                        }
                    }
                    _ => {
                        // Skill slot disabled / unreadable this tick / not playing.
                        idle_streak_reset();
                        idle_announced = false;
                    }
                }

                // --- Pet Guard ---
                // MP or SP low (whichever monitors are on) => pet not out. While HP reads low
                // too the guard PAUSES (no press, no new episode, no reset): a long HP-low is
                // death / a loading screen / the whole HUD off-color, none of which should
                // toggle the pet; a short one is just a hit landing before the Q potion, and
                // resetting the debounce on every hit would postpone the call indefinitely.
                let mp_sp_low = tick_low[Bar::Mp as usize] == Some(true)
                    || tick_low[Bar::Sp as usize] == Some(true);
                let hp_low = tick_low[Bar::Hp as usize] == Some(true);
                let guard_on = PET_GUARD.load(Ordering::Acquire);
                if guard_on && hp_low {
                    // hold
                } else if guard_on && mp_sp_low {
                    let start = *pet_low_start.get_or_insert_with(Instant::now);
                    let _ = PET_LOW_SINCE_MS.compare_exchange(
                        0,
                        now_ms(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    let due = match pet_last_call {
                        None => start.elapsed() >= PET_CALL_DEBOUNCE,
                        Some(t) => t.elapsed() >= PET_CALL_RETRY,
                    };
                    if due {
                        player::send_key_press(PET_VK, pet_scan);
                        pet_last_call = Some(Instant::now());
                        let _ = PET_FIRST_CALL_MS.compare_exchange(
                            0,
                            now_ms(),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        let n = PET_CALLS.fetch_add(1, Ordering::AcqRel) + 1;
                        println!(
                            "[Cadence] Pet guard: MP/SP low for {}s, calling pet (#{}).",
                            start.elapsed().as_secs(),
                            n
                        );
                    }
                } else if pet_low_start.is_some() || PET_LOW_SINCE_MS.load(Ordering::Acquire) != 0 {
                    pet_low_start = None;
                    pet_last_call = None;
                    reset_pet_episode();
                }
            }

            // Pace the loop without burning a full core.
            timer.precise_wait_micros(POLL_INTERVAL_MICROS);
        }

        println!("[Cadence] Bar monitor thread stopped.");
        ACTIVE.store(false, Ordering::Release);
    });
}

// --- Game-window resolution ------------------------------------------------------------------
//
// The pick flow stores the game window's class and title verbatim, but the class alone is not a
// stable identity: the RAN client is MFC, and MFC auto-registers class names of the form
// `Afx:<hInstance>:<style>:<hCursor>:<hbrBackground>:<hIcon>` whose last three fields are GDI
// handle values that change on every game launch. An exact-class match therefore dies the moment
// the client is reopened — which used to force a pixel re-pick on every VM after every
// disconnect. Resolution is tiered instead: exact match first (fast path within a session), then
// the stable Afx prefix, then the stored title — so the same saved anchor keeps working across
// game relaunches.

/// "Afx:00400000:8:00010003:00900011:024A1B55" -> Some("Afx:00400000:8:"). The first two fields
/// after "Afx" (module base, window style) are stable per game build; the rest are per-launch
/// GDI handles. None when the class isn't an MFC auto-generated name with volatile fields.
fn afx_stable_prefix(class: &str) -> Option<String> {
    let parts: Vec<&str> = class.split(':').collect();
    if parts.len() >= 4 && parts[0] == "Afx" && !parts[1].is_empty() && !parts[2].is_empty() {
        // The trailing ':' is load-bearing: "Afx:00400000:8:" must not match "Afx:00400000:80:…".
        Some(format!("Afx:{}:{}:", parts[1], parts[2]))
    } else {
        None
    }
}

/// Score a candidate window against the stored anchor. Lower is better; None = no match.
///   0: exact class + title prefix (the pick-time window, same session)
///   1: exact class, any title
///   2: same MFC stable prefix (game relaunched), visible, not ours; title must match when stored
///   3: title prefix only (class changed entirely), visible, not ours
/// Tiers 0/1 skip the visibility test to preserve the old FindWindowW semantics exactly.
fn match_tier(
    stored_class: &str,
    afx_prefix: Option<&str>,
    stored_title: &str,
    actual_class: &str,
    actual_title: &str,
    visible: bool,
    own_process: bool,
) -> Option<u8> {
    let title_ok = !stored_title.is_empty() && actual_title.starts_with(stored_title);
    if actual_class == stored_class {
        return Some(if title_ok { 0 } else { 1 });
    }
    if !visible || own_process {
        return None;
    }
    if let Some(prefix) = afx_prefix {
        if actual_class.starts_with(prefix) && (stored_title.is_empty() || title_ok) {
            return Some(2);
        }
    }
    if title_ok {
        Some(3)
    } else {
        None
    }
}

struct FindCtx<'a> {
    stored_class: &'a str,
    afx_prefix: Option<&'a str>,
    stored_title: &'a str,
    own_pid: u32,
    foreground: HWND,
    best: HWND,
    best_tier: u8,
}

/// Read a live window's identity and score it. Safe against hung windows: `GetClassNameW` sends
/// no messages, and `GetWindowTextW` on another process's window reads the cached title without
/// sending WM_GETTEXT.
unsafe fn window_tier(hwnd: HWND, ctx: &FindCtx) -> Option<u8> {
    let mut cbuf = [0u16; 256];
    let clen = GetClassNameW(hwnd, cbuf.as_mut_ptr(), cbuf.len() as i32) as usize;
    let class = String::from_utf16_lossy(&cbuf[..clen]);
    let mut tbuf = [0u16; 256];
    let tlen = GetWindowTextW(hwnd, tbuf.as_mut_ptr(), tbuf.len() as i32) as usize;
    let title = String::from_utf16_lossy(&tbuf[..tlen]);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    match_tier(
        ctx.stored_class,
        ctx.afx_prefix,
        ctx.stored_title,
        &class,
        &title,
        IsWindowVisible(hwnd) != 0,
        pid == ctx.own_pid,
    )
}

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: isize) -> i32 {
    let ctx = &mut *(lparam as *mut FindCtx);
    if let Some(tier) = window_tier(hwnd, ctx) {
        if tier < ctx.best_tier || (tier == ctx.best_tier && hwnd == ctx.foreground) {
            ctx.best = hwnd;
            ctx.best_tier = tier;
        }
        if tier == 0 && hwnd == ctx.foreground {
            return 0; // can't do better; stop enumerating
        }
    }
    1
}

// Resolved-window cache. The monitor calls the finder every 10 ms tick, so a full EnumWindows
// each time would be wasteful; conversely, re-scanning at most every 2 s lets a weak title-only
// match upgrade itself shortly after the real game window (re)appears.
static CACHED_HWND: AtomicIsize = AtomicIsize::new(0);
static LAST_ENUM_MS: AtomicU64 = AtomicU64::new(0);
static LAST_TIER: AtomicU32 = AtomicU32::new(u32::MAX);
const REENUM_INTERVAL_MS: u64 = 2000;

/// Find the game window for a stored (class, title) anchor, tolerating the class changes a game
/// relaunch causes. Returns null when nothing matches. Shared by the bar monitor, Burst Q's
/// focus check, and the Settings "Sample" button so they always agree on the window.
pub(crate) unsafe fn find_game_window(stored_class: &str, stored_title: &str) -> HWND {
    let now = now_ms();
    let afx = afx_stable_prefix(stored_class);
    let mut ctx = FindCtx {
        stored_class,
        afx_prefix: afx.as_deref(),
        stored_title,
        own_pid: winapi::um::processthreadsapi::GetCurrentProcessId(),
        foreground: GetForegroundWindow(),
        best: std::ptr::null_mut(),
        best_tier: u8::MAX,
    };

    // Trust the cached handle while it still exists and still matches the anchor we were
    // handed (a re-pick mid-session changes the anchor; HWND values can also be recycled).
    let cached = CACHED_HWND.load(Ordering::Acquire) as HWND;
    if !cached.is_null()
        && now.saturating_sub(LAST_ENUM_MS.load(Ordering::Acquire)) < REENUM_INTERVAL_MS
        && IsWindow(cached) != 0
        && window_tier(cached, &ctx).is_some()
    {
        return cached;
    }

    LAST_ENUM_MS.store(now, Ordering::Release);
    EnumWindows(Some(enum_windows_cb), &mut ctx as *mut FindCtx as isize);
    CACHED_HWND.store(ctx.best as isize, Ordering::Release);

    let tier = if ctx.best.is_null() { u32::MAX } else { ctx.best_tier as u32 };
    if LAST_TIER.swap(tier, Ordering::AcqRel) != tier {
        match tier {
            u32::MAX => println!("[Cadence] Game window lost."),
            t => println!("[Cadence] Game window resolved (tier {}).", t),
        }
    }
    ctx.best
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "Afx:00400000:8:00010003:00900011:024A1B55";
    const RELAUNCHED: &str = "Afx:00400000:8:00020007:00A10035:0F1A2B3C";

    #[test]
    fn afx_prefix_of_real_class() {
        assert_eq!(afx_stable_prefix(REAL).as_deref(), Some("Afx:00400000:8:"));
    }

    #[test]
    fn afx_prefix_requires_volatile_fields() {
        assert_eq!(afx_stable_prefix("Afx:00400000:8"), None); // short form: exact match covers it
        assert_eq!(afx_stable_prefix("Afx:00400000"), None);
        assert_eq!(afx_stable_prefix("Afx:"), None);
        assert_eq!(afx_stable_prefix(""), None);
    }

    #[test]
    fn afx_prefix_rejects_non_afx() {
        assert_eq!(afx_stable_prefix("Notepad"), None);
        assert_eq!(afx_stable_prefix("AfxFrameOrView140u"), None);
        assert_eq!(afx_stable_prefix("Afxx:1:2:3:4:5"), None);
    }

    fn tier(actual_class: &str, actual_title: &str, visible: bool, own: bool) -> Option<u8> {
        let afx = afx_stable_prefix(REAL);
        match_tier(REAL, afx.as_deref(), "Ran Client", actual_class, actual_title, visible, own)
    }

    #[test]
    fn tier0_exact_class_and_title() {
        assert_eq!(tier(REAL, "Ran Client", true, false), Some(0));
        // Title matching is a prefix match, like the old finder.
        assert_eq!(tier(REAL, "Ran Client - Char", true, false), Some(0));
        // Exact class doesn't require visibility (old FindWindowW semantics).
        assert_eq!(tier(REAL, "Ran Client", false, false), Some(0));
    }

    #[test]
    fn tier1_exact_class_wrong_title() {
        assert_eq!(tier(REAL, "Loading", true, false), Some(1));
        // Empty stored title: class match lands on tier 1.
        let afx = afx_stable_prefix(REAL);
        assert_eq!(match_tier(REAL, afx.as_deref(), "", REAL, "anything", true, false), Some(1));
    }

    #[test]
    fn tier2_relaunch_changes_volatile_fields() {
        assert_eq!(tier(RELAUNCHED, "Ran Client", true, false), Some(2));
        assert_eq!(tier(RELAUNCHED, "Ran Client", false, false), None); // must be visible
        assert_eq!(tier(RELAUNCHED, "Other", true, false), None); // stored title must match
        assert_eq!(tier(RELAUNCHED, "Ran Client", true, true), None); // never our own window
    }

    #[test]
    fn tier2_prefix_boundary() {
        // "Afx:00400000:8:" must not match a class whose style field merely starts with '8'.
        assert_eq!(tier("Afx:00400000:80:x:y:z", "Some Title", true, false), None);
        // …but the title fallback still applies if the title matches.
        assert_eq!(tier("Afx:00400000:80:x:y:z", "Ran Client", true, false), Some(3));
    }

    #[test]
    fn tier2_with_empty_stored_title() {
        let afx = afx_stable_prefix(REAL);
        assert_eq!(
            match_tier(REAL, afx.as_deref(), "", RELAUNCHED, "whatever", true, false),
            Some(2)
        );
    }

    #[test]
    fn tier3_title_only() {
        assert_eq!(tier("SomeOtherClass", "Ran Client", true, false), Some(3));
        assert_eq!(tier("SomeOtherClass", "Ran Client", true, true), None); // own process
        assert_eq!(tier("SomeOtherClass", "Ran Client", false, false), None); // invisible
        // Empty stored title disables the title fallback entirely.
        let afx: Option<String> = None;
        assert_eq!(
            match_tier("SomeClass", afx.as_deref(), "", "Other", "Anything", true, false),
            None
        );
    }

    #[test]
    fn no_match_at_all() {
        assert_eq!(tier("Explorer", "Recycle Bin", true, false), None);
    }

    /// Live test: requires a running RAN client. Feeds the finder the anchor a PREVIOUS
    /// game session would have stored (same stable Afx prefix, different volatile
    /// GDI-handle tail) and asserts it resolves to the live game window anyway — the
    /// exact "game relaunched, config is stale" scenario. Run with:
    /// `cargo test live_finds -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_finds_relaunched_game_window() {
        let stale = "Afx:00400000:8:0000DEAD:0000BEEF:00C0FFEE";
        let hwnd = unsafe { find_game_window(stale, "Ran Client") };
        assert!(!hwnd.is_null(), "no window matched the stale anchor — is the game running?");
        let mut buf = [0u16; 256];
        let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), 256) } as usize;
        let class = String::from_utf16_lossy(&buf[..len]);
        println!("resolved live game window: class = {}", class);
        assert!(class.starts_with("Afx:00400000:8:"), "resolved wrong window: {}", class);
        assert_ne!(class, stale, "matched the stale class exactly — that can't happen live");
    }
}
