use crate::player;
use crate::timing::PrecisionTimer;
use crate::win32_helpers::wide;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use winapi::shared::windef::HWND;
use winapi::um::winuser::*;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

// Notify-on-stop callback target. Set by main.rs at startup so the worker
// thread can post a UI-refresh message back to the toolbar when it exits
// (via hotkey toggle, focus loss, or shutdown).
static NOTIFY_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static NOTIFY_MSG: AtomicU32 = AtomicU32::new(0);

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Register a thread-message channel used to wake the UI when burst stops on
/// its own (e.g. focus-loss). Pass 0 to disable.
pub fn set_notify(thread_id: u32, msg: u32) {
    NOTIFY_THREAD_ID.store(thread_id, Ordering::Release);
    NOTIFY_MSG.store(msg, Ordering::Release);
}

/// Toggle burst on/off. Caller supplies current rate and game window
/// fingerprint (same fields HP monitor uses) so we can auto-stop on focus loss.
pub fn toggle(rate_hz: u32, window_class: String, window_title: String) {
    if ACTIVE.load(Ordering::Acquire) {
        stop();
    } else {
        start(rate_hz, window_class, window_title);
    }
}

pub fn start(rate_hz: u32, window_class: String, window_title: String) {
    if ACTIVE.load(Ordering::Acquire) {
        return;
    }

    let rate = rate_hz.clamp(50, 200);

    CANCEL.store(false, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);

    thread::spawn(move || {
        println!("[Cadence] Burst Q started ({} Hz)", rate);

        let vk: u16 = 0x51; // Q
        let scan_code = player::scan_code(vk);
        let interval_micros: i64 = (1_000_000 / rate) as i64;
        let timer = PrecisionTimer::new();

        let use_window_anchor = !window_class.is_empty();
        let class_w = if use_window_anchor {
            Some(wide(&window_class))
        } else {
            None
        };

        // Resolve the game HWND once at start. We re-check foreground each
        // tick against this handle so Alt-Tab away stops the burst, but we
        // tolerate the window briefly disappearing (it just won't fire).
        let game_hwnd: HWND = unsafe {
            if let Some(ref cw) = class_w {
                crate::monitor::find_window_matching(cw.as_ptr(), &window_title)
            } else {
                std::ptr::null_mut()
            }
        };

        let start_ticks = timer.now_ticks();
        let mut tick: i64 = 0;

        while !CANCEL.load(Ordering::Acquire) {
            if !game_hwnd.is_null() {
                let fg = unsafe { GetForegroundWindow() };
                if fg != game_hwnd {
                    println!("[Cadence] Burst Q stopped (focus lost).");
                    break;
                }
            }

            player::send_key_press(vk, scan_code);

            tick += 1;
            let elapsed = timer.ticks_to_micros(timer.now_ticks() - start_ticks);
            let target = tick * interval_micros;
            let remaining = target - elapsed;
            if remaining > 0 {
                timer.precise_wait_micros(remaining);
            }
        }

        ACTIVE.store(false, Ordering::Release);
        println!("[Cadence] Burst Q stopped.");

        // Wake the UI so the toolbar visual flips back to idle even when
        // the burst stopped by itself (focus loss).
        let tid = NOTIFY_THREAD_ID.load(Ordering::Acquire);
        let msg = NOTIFY_MSG.load(Ordering::Acquire);
        if tid != 0 && msg != 0 {
            unsafe {
                winapi::um::winuser::PostThreadMessageW(tid, msg, 0, 0);
            }
        }
    });
}

pub fn stop() {
    if ACTIVE.load(Ordering::Acquire) {
        CANCEL.store(true, Ordering::Release);
    }
}
