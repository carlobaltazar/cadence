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

pub fn play_queue(sequences: Vec<Vec<InputEvent>>) {
    if sequences.is_empty() {
        println!("[Ranify2] Empty queue.");
        return;
    }

    if PLAYING.load(Ordering::Acquire) {
        println!("[Ranify2] Already playing.");
        return;
    }

    PLAYING.store(true, Ordering::Release);
    CANCEL.store(false, Ordering::Release);

    std::thread::spawn(move || {
        let timer = PrecisionTimer::new();

        let (vscreen_x, vscreen_y, vscreen_w, vscreen_h) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };

        let total_sequences = sequences.len();
        println!("[Ranify2] Playing queue of {} sequences...", total_sequences);

        let mut order: Vec<usize> = (0..total_sequences).collect();
        let mut cancelled = false;

        loop {
            // Shuffle order each cycle if shuffle mode is on
            if SHUFFLE_MODE.load(Ordering::Acquire) {
                use rand::seq::SliceRandom;
                order.shuffle(&mut rand::thread_rng());
            }

            for (seq_idx, &idx) in order.iter().enumerate() {
                for (j, event) in sequences[idx].iter().enumerate() {
                    if CANCEL.load(Ordering::Acquire) {
                        println!("[Ranify2] Queue playback cancelled.");
                        cancelled = true;
                        break;
                    }

                    // Zero out the initial delay between sequences
                    if seq_idx > 0 && j == 0 {
                        // skip delay for first event of non-first sequence
                    } else {
                        timer.precise_wait_micros(event.delay_micros);
                    }

                    match &event.event_type {
                        InputEventType::MouseMove { x, y } => {
                            let abs_x =
                                ((*x - vscreen_x) as f64 / vscreen_w as f64 * 65535.0) as i32;
                            let abs_y =
                                ((*y - vscreen_y) as f64 / vscreen_h as f64 * 65535.0) as i32;
                            send_mouse_input(
                                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                                abs_x,
                                abs_y,
                                0,
                            );
                        }
                        InputEventType::MouseButton { button, pressed } => {
                            let flags = match (button, pressed) {
                                (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                                (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                                (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                                (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                                (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                                (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                                (MouseButton::X1, true) | (MouseButton::X2, true) => {
                                    MOUSEEVENTF_XDOWN
                                }
                                (MouseButton::X1, false) | (MouseButton::X2, false) => {
                                    MOUSEEVENTF_XUP
                                }
                            };
                            let mouse_data = match button {
                                MouseButton::X1 => XBUTTON1,
                                MouseButton::X2 => XBUTTON2,
                                _ => 0,
                            };
                            send_mouse_input(flags, 0, 0, mouse_data as i32);
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
                if cancelled {
                    break;
                }
            }

            if cancelled || !LOOP_MODE.load(Ordering::Acquire) {
                break;
            }
        }

        println!("[Ranify2] Queue playback finished.");
        PLAYING.store(false, Ordering::Release);
    });
}

pub fn play_sequence(events: Vec<InputEvent>) {
    if events.is_empty() {
        println!("[Ranify2] No events to play.");
        return;
    }

    if PLAYING.load(Ordering::Acquire) {
        println!("[Ranify2] Already playing.");
        return;
    }

    PLAYING.store(true, Ordering::Release);
    CANCEL.store(false, Ordering::Release);

    std::thread::spawn(move || {
        let timer = PrecisionTimer::new();

        // Get virtual screen dimensions for absolute coordinate normalization
        let (vscreen_x, vscreen_y, vscreen_w, vscreen_h) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };

        let event_count = events.len();
        println!("[Ranify2] Playing {} events...", event_count);

        let mut cancelled = false;
        loop {
            for (i, event) in events.iter().enumerate() {
                if CANCEL.load(Ordering::Acquire) {
                    println!(
                        "[Ranify2] Playback cancelled at event {}/{}",
                        i, event_count
                    );
                    cancelled = true;
                    break;
                }

                // Wait the precise delay
                timer.precise_wait_micros(event.delay_micros);

                // Send the input event
                match &event.event_type {
                    InputEventType::MouseMove { x, y } => {
                        let abs_x =
                            ((*x - vscreen_x) as f64 / vscreen_w as f64 * 65535.0) as i32;
                        let abs_y =
                            ((*y - vscreen_y) as f64 / vscreen_h as f64 * 65535.0) as i32;
                        send_mouse_input(
                            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                            abs_x,
                            abs_y,
                            0,
                        );
                    }
                    InputEventType::MouseButton { button, pressed } => {
                        let flags = match (button, pressed) {
                            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                            (MouseButton::X1, true) | (MouseButton::X2, true) => {
                                MOUSEEVENTF_XDOWN
                            }
                            (MouseButton::X1, false) | (MouseButton::X2, false) => {
                                MOUSEEVENTF_XUP
                            }
                        };
                        let mouse_data = match button {
                            MouseButton::X1 => XBUTTON1,
                            MouseButton::X2 => XBUTTON2,
                            _ => 0,
                        };
                        send_mouse_input(flags, 0, 0, mouse_data as i32);
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

            if cancelled || !LOOP_MODE.load(Ordering::Acquire) {
                break;
            }
        }

        println!("[Ranify2] Playback finished.");
        PLAYING.store(false, Ordering::Release);
    });
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
