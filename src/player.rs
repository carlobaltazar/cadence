use crate::sequence::{InputEvent, InputEventType, MouseButton};
use crate::timing::PrecisionTimer;
use crate::win32_helpers::lock_or_recover;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use winapi::um::winuser::*;

static PLAYING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static LOOP_MODE: AtomicBool = AtomicBool::new(false);
static SHUFFLE_MODE: AtomicBool = AtomicBool::new(false);

// Serializes every SendInput call so background features (HP monitor, burst,
// pet cycle) can't interleave events with queue playback. Each dispatch is
// one atomic syscall — synthesized down+up pairs ship as a 2-event array so
// the game never observes an orphaned key state.
static INPUT_LOCK: Mutex<()> = Mutex::new(());

pub fn is_playing() -> bool {
    PLAYING.load(Ordering::Acquire)
}

pub fn cancel_playback() {
    CANCEL.store(true, Ordering::Release);
}

pub fn set_loop_mode(enabled: bool) {
    LOOP_MODE.store(enabled, Ordering::Release);
}

pub fn is_loop_mode() -> bool {
    LOOP_MODE.load(Ordering::Acquire)
}

pub fn set_shuffle_mode(enabled: bool) {
    SHUFFLE_MODE.store(enabled, Ordering::Release);
}

pub fn is_shuffle_mode() -> bool {
    SHUFFLE_MODE.load(Ordering::Acquire)
}

/// Virtual-screen origin and span, used to normalize absolute mouse coords.
fn vscreen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Normalize a virtual-desktop pixel coordinate to the 0..65535 range SendInput
/// expects for MOUSEEVENTF_ABSOLUTE moves.
fn to_abs(coord: i32, origin: i32, span: i32) -> i32 {
    ((coord - origin) as f64 / span as f64 * 65535.0) as i32
}

/// Map a mouse button + press state to its SendInput (flags, mouse_data) pair.
fn mouse_button_flags(button: &MouseButton, pressed: bool) -> (u32, i32) {
    let flags = match (button, pressed) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        (MouseButton::X1, true) | (MouseButton::X2, true) => MOUSEEVENTF_XDOWN,
        (MouseButton::X1, false) | (MouseButton::X2, false) => MOUSEEVENTF_XUP,
    };
    let mouse_data = match button {
        MouseButton::X1 => XBUTTON1 as i32,
        MouseButton::X2 => XBUTTON2 as i32,
        _ => 0,
    };
    (flags, mouse_data)
}

/// Synthesize a single recorded event to the OS. `vs` is the virtual-screen
/// tuple from `vscreen()`, reused across all events in a playback run.
fn perform_event(event_type: &InputEventType, vs: (i32, i32, i32, i32)) {
    let (vx, vy, vw, vh) = vs;
    match event_type {
        InputEventType::MouseMove { x, y } => {
            send_mouse_input(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                to_abs(*x, vx, vw),
                to_abs(*y, vy, vh),
                0,
            );
        }
        InputEventType::MouseButton { button, pressed } => {
            let (flags, mouse_data) = mouse_button_flags(button, *pressed);
            send_mouse_input(flags, 0, 0, mouse_data);
        }
        InputEventType::MouseWheel { delta } => {
            send_mouse_input(MOUSEEVENTF_WHEEL, 0, 0, *delta);
        }
        InputEventType::KeyPress {
            vk_code,
            scan_code,
            pressed,
            extended,
        } => {
            let mut flags = KEYEVENTF_SCANCODE;
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            if *extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            send_key_input(*vk_code, *scan_code, flags);
        }
    }
}

/// Shared playback runner for both single-sequence and queue playback. A single
/// sequence is just a queue of one, so both entry points funnel through here.
/// Honors CANCEL, LOOP_MODE, and SHUFFLE_MODE, and zeroes the leading delay
/// between consecutive sequences so queued playlists don't stall between items.
fn run_playback_opts(sequences: Vec<Vec<InputEvent>>, force_once: bool) {
    PLAYING.store(true, Ordering::Release);
    CANCEL.store(false, Ordering::Release);

    std::thread::spawn(move || {
        let timer = PrecisionTimer::new();
        let vs = vscreen();

        let total_sequences = sequences.len();
        println!("[Cadence] Playing {} sequence(s)...", total_sequences);

        let mut order: Vec<usize> = (0..total_sequences).collect();
        let mut cancelled = false;

        loop {
            // Shuffle order each cycle if shuffle mode is on
            if SHUFFLE_MODE.load(Ordering::Acquire) {
                use rand::seq::SliceRandom;
                order.shuffle(&mut rand::thread_rng());
            }

            // Schedule against an absolute timeline anchored at the start of this
            // pass: each event waits until `anchor + sum(delays so far)` rather
            // than sleeping each gap independently. This keeps per-event sleep
            // overshoot from accumulating, so cooldown-timed presses stay aligned
            // over long/looped sequences instead of drifting late.
            let mut anchor = timer.now_ticks();
            let mut acc_micros: i64 = 0;

            for (seq_idx, &idx) in order.iter().enumerate() {
                for (j, event) in sequences[idx].iter().enumerate() {
                    if CANCEL.load(Ordering::Acquire) {
                        println!("[Cadence] Playback cancelled.");
                        cancelled = true;
                        break;
                    }

                    if seq_idx > 0 && j == 0 {
                        // Zero out the initial delay between queued sequences and
                        // re-anchor the timeline so the next item starts "now".
                        anchor = timer.now_ticks();
                        acc_micros = 0;
                    } else {
                        acc_micros += event.delay_micros;
                        timer.wait_until_ticks(anchor + timer.micros_to_ticks(acc_micros));
                    }

                    perform_event(&event.event_type, vs);
                }
                if cancelled {
                    break;
                }
            }

            if cancelled || force_once || !LOOP_MODE.load(Ordering::Acquire) {
                break;
            }
        }

        println!("[Cadence] Playback finished.");
        PLAYING.store(false, Ordering::Release);
    });
}

fn run_playback(sequences: Vec<Vec<InputEvent>>) {
    run_playback_opts(sequences, false);
}

pub fn play_queue(sequences: Vec<Vec<InputEvent>>) {
    if sequences.is_empty() {
        println!("[Cadence] Empty queue.");
        return;
    }
    if PLAYING.load(Ordering::Acquire) {
        println!("[Cadence] Already playing.");
        return;
    }
    run_playback(sequences);
}

pub fn play_sequence(events: Vec<InputEvent>) {
    if events.is_empty() {
        println!("[Cadence] No events to play.");
        return;
    }
    if PLAYING.load(Ordering::Acquire) {
        println!("[Cadence] Already playing.");
        return;
    }
    run_playback(vec![events]);
}

/// Play a sequence exactly once, ignoring the global Loop toggle. For one-shot reactions
/// (e.g. proximity "go to market") that must not loop even when Loop is checked.
pub fn play_sequence_once(events: Vec<InputEvent>) {
    if events.is_empty() || PLAYING.load(Ordering::Acquire) {
        return;
    }
    run_playback_opts(vec![events], true);
}

fn make_kbd(vk: u16, scan_code: u16, flags: u32) -> INPUT {
    let mut input = unsafe { std::mem::zeroed::<INPUT>() };
    input.type_ = INPUT_KEYBOARD;
    unsafe {
        let ki = input.u.ki_mut();
        ki.wVk = vk;
        ki.wScan = scan_code;
        ki.dwFlags = flags;
        ki.time = 0;
        ki.dwExtraInfo = 0;
    }
    input
}

fn make_mouse(flags: u32, dx: i32, dy: i32, mouse_data: i32) -> INPUT {
    let mut input = unsafe { std::mem::zeroed::<INPUT>() };
    input.type_ = INPUT_MOUSE;
    unsafe {
        let mi = input.u.mi_mut();
        mi.dx = dx;
        mi.dy = dy;
        mi.mouseData = mouse_data as u32;
        mi.dwFlags = flags;
        mi.time = 0;
        mi.dwExtraInfo = 0;
    }
    input
}

fn dispatch(events: &mut [INPUT]) {
    let _g = lock_or_recover(&INPUT_LOCK);
    unsafe {
        SendInput(
            events.len() as u32,
            events.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

fn send_mouse_input(flags: u32, dx: i32, dy: i32, mouse_data: i32) {
    let mut e = make_mouse(flags, dx, dy, mouse_data);
    dispatch(std::slice::from_mut(&mut e));
}

pub fn send_key_input(vk: u16, scan_code: u16, flags: u32) {
    let mut e = make_kbd(vk, scan_code, flags);
    dispatch(std::slice::from_mut(&mut e));
}

/// Atomic key down + key up in a single SendInput call. Use for synthesized
/// presses (HP monitor, burst, pet cycle) so the game can never observe a
/// dangling down without its matching up, even when other threads dispatch
/// concurrently.
pub fn send_key_press(vk: u16, scan_code: u16) {
    let mut events = [
        make_kbd(vk, scan_code, KEYEVENTF_SCANCODE),
        make_kbd(vk, scan_code, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP),
    ];
    dispatch(&mut events);
}

/// Resolve the hardware scan code for a virtual-key code (MAPVK_VK_TO_VSC).
/// Synthesized presses ship scan codes so games reading raw input see them.
pub fn scan_code(vk: u16) -> u16 {
    unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 }
}
