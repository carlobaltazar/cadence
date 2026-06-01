use crate::player;
use crate::timing::PrecisionTimer;
use crate::win32_helpers::{lock_or_recover, wide};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use winapi::shared::windef::HWND;
use winapi::um::wingdi::GetPixel;
use winapi::um::winuser::*;

/// The three bars this monitor watches. All three sit on the same game window
/// (shared anchor) and press a fixed key when their pixel drifts off-color.
#[derive(Clone, Copy)]
pub enum Bar {
    Hp = 0,
    Mp = 1,
    Sp = 2,
}

const BAR_COUNT: usize = 3;
const CLR_INVALID: u32 = 0xFFFFFFFF;
const COLOR_TOLERANCE: u32 = 48; // Manhattan distance across BGR channels
const POLL_INTERVAL_MICROS: i64 = 10_000; // ~10ms tick — detects a drop within ~10ms
// Minimum gap between presses for a single bar. The poll is tight so detection is
// near-instant, but firing is paced so each heal lands before the next press
// (avoids flooding the game with ~100 presses/sec).
const FIRE_COOLDOWN: Duration = Duration::from_millis(120);

// Fixed keys per bar: HP->Q, MP->W, SP->E.
const BAR_VK: [u16; BAR_COUNT] = [0x51, 0x57, 0x45];
const BAR_NAME: [&str; BAR_COUNT] = ["HP", "MP", "SP"];

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

static BARS: [BarState; BAR_COUNT] = [BarState::new(), BarState::new(), BarState::new()];

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
        "[Ranify2] {} monitor enabled (cx={} cy={} ref=0x{:06X})",
        BAR_NAME[bar as usize], x, y, ref_color
    );
    ensure_running();
}

/// Disable a single bar. The worker thread keeps running for the other bars and
/// exits on its own once no bar is enabled.
pub fn disable_bar(bar: Bar) {
    BARS[bar as usize].enabled.store(false, Ordering::Release);
    println!("[Ranify2] {} monitor disabled.", BAR_NAME[bar as usize]);
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

        // Per-bar scan codes computed once.
        let scans: [u16; BAR_COUNT] = {
            let mut s = [0u16; BAR_COUNT];
            for i in 0..BAR_COUNT {
                s[i] = unsafe { MapVirtualKeyW(BAR_VK[i] as u32, MAPVK_VK_TO_VSC) } as u16;
            }
            s
        };

        // Per-bar last-fire timestamps; start "armed" so the first drop fires immediately.
        let mut last_fire = [Instant::now() - FIRE_COOLDOWN; BAR_COUNT];

        loop {
            if CANCEL.load(Ordering::Acquire) {
                break;
            }
            if !any_enabled() {
                // Nothing to watch right now. Stay alive (avoids a respawn race
                // with set_bar) but idle cheaply until a bar is re-enabled.
                thread::sleep(Duration::from_millis(200));
                continue;
            }

            let (class, title) = lock_or_recover(&ANCHOR).clone();
            let use_window_anchor = !class.is_empty();

            unsafe {
                // Resolve the game window + DC ONCE per tick, then sample every
                // enabled bar against it. Shared anchor => one GetDC for all bars.
                let (hwnd, hdc) = if use_window_anchor {
                    let class_w = wide(&class);
                    let hwnd = find_window_matching(class_w.as_ptr(), &title);
                    if hwnd.is_null() {
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    if GetForegroundWindow() != hwnd {
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    let hdc = GetDC(hwnd);
                    if hdc.is_null() {
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    (hwnd, hdc)
                } else {
                    let hdc = GetDC(std::ptr::null_mut());
                    if hdc.is_null() {
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    (std::ptr::null_mut(), hdc)
                };

                for i in 0..BAR_COUNT {
                    let b = &BARS[i];
                    if !b.enabled.load(Ordering::Acquire) {
                        continue;
                    }
                    let x = b.x.load(Ordering::Acquire);
                    let y = b.y.load(Ordering::Acquire);
                    let ref_color = b.ref_color.load(Ordering::Acquire);
                    let color = GetPixel(hdc, x, y);
                    if color != CLR_INVALID
                        && color_dist(color, ref_color) > COLOR_TOLERANCE
                        && last_fire[i].elapsed() >= FIRE_COOLDOWN
                    {
                        // Bar is below threshold and the per-bar cooldown elapsed:
                        // fire once, then wait FIRE_COOLDOWN before the next press.
                        player::send_key_press(BAR_VK[i], scans[i]);
                        last_fire[i] = Instant::now();
                    }
                }

                ReleaseDC(hwnd, hdc);
            }

            // Pace the loop without burning a full core.
            timer.precise_wait_micros(POLL_INTERVAL_MICROS);
        }

        println!("[Ranify2] Bar monitor thread stopped.");
        ACTIVE.store(false, Ordering::Release);
    });
}

pub(crate) unsafe fn find_window_matching(class_ptr: *const u16, title_prefix: &str) -> HWND {
    if title_prefix.is_empty() {
        return FindWindowW(class_ptr, std::ptr::null());
    }

    let mut hwnd = FindWindowExW(
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        class_ptr,
        std::ptr::null(),
    );
    while !hwnd.is_null() {
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) as usize;
        let actual: String = String::from_utf16_lossy(&buf[..len]);
        if actual.starts_with(title_prefix) {
            return hwnd;
        }
        hwnd = FindWindowExW(std::ptr::null_mut(), hwnd, class_ptr, std::ptr::null());
    }

    FindWindowW(class_ptr, std::ptr::null())
}
