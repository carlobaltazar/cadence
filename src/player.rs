use crate::sequence::{fix_extended, queue_pass_micros, InputEvent, InputEventType, MouseButton};
use crate::timing::PrecisionTimer;
use crate::win32_helpers::lock_or_recover;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use winapi::um::winuser::*;

static PLAYING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static LOOP_MODE: AtomicBool = AtomicBool::new(false);
static SHUFFLE_MODE: AtomicBool = AtomicBool::new(false);
// For the toolbar clock: when the current playback started and how long one pass takes.
static PLAY_STARTED: Mutex<Option<Instant>> = Mutex::new(None);
static PASS_MICROS: AtomicI64 = AtomicI64::new(0);

/// `(elapsed, one-pass duration in micros)` while playing; None otherwise.
pub fn progress() -> Option<(Duration, i64)> {
    if !is_playing() {
        return None;
    }
    let started = (*lock_or_recover(&PLAY_STARTED))?;
    Some((started.elapsed(), PASS_MICROS.load(Ordering::Acquire)))
}

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

/// What the current (or most recent) playback was started from, so the auto-updater can
/// resume the same thing after it restarts the process. `Adhoc` covers playback with no
/// on-disk identity — unnamed LAST_EVENTS and proximity one-shot reactions — which is
/// deliberately not resumable.
#[derive(Clone)]
pub enum PlaybackSource {
    Sequence(String),
    Queue(Vec<String>),
    Adhoc,
}

static SOURCE: Mutex<PlaybackSource> = Mutex::new(PlaybackSource::Adhoc);

/// Record where playback came from. Callers set this immediately before `play_*`; it is
/// never cleared when playback ends — it is only read while `is_playing()` says so (and
/// the benign race there resolves to "resume it anyway", which is what a 24/7 VM wants).
/// Named playback is also remembered on disk so the toolbar can answer "what did I play?"
/// across sessions; Adhoc (unsaved events, proximity one-shots) keeps the previous memory.
pub fn set_source(source: PlaybackSource) {
    match &source {
        PlaybackSource::Sequence(name) => {
            crate::storage::set_last_played(name.clone(), Vec::new())
        }
        PlaybackSource::Queue(names) => {
            crate::storage::set_last_played(String::new(), names.clone())
        }
        PlaybackSource::Adhoc => {}
    }
    *lock_or_recover(&SOURCE) = source;
}

pub fn current_source() -> PlaybackSource {
    lock_or_recover(&SOURCE).clone()
}

/// Cancel playback and wait (up to `timeout_ms`) for the playback thread to acknowledge.
/// The thread checks CANCEL before each event, so this lands on an event boundary — but
/// it can sit inside one long inter-event delay, hence the timeout. Returns whether
/// something was playing when called.
pub fn stop_and_wait(timeout_ms: u64) -> bool {
    if !is_playing() {
        return false;
    }
    cancel_playback();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while is_playing() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}

/// Free the player for a new playback: cancel any current run and wait (up to
/// `timeout_ms`) for the playback thread to fully exit — including its
/// held-input release sweep. Unlike `stop_and_wait` (which reports "was
/// something playing"), this returns whether the player is actually idle now,
/// so a caller about to start a new playback can trust `true` to mean the
/// backstop guard in `play_*` won't no-op it.
pub fn take_over(timeout_ms: u64) -> bool {
    if !is_playing() {
        return true;
    }
    cancel_playback();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while is_playing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !is_playing()
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
            // Applied here too so recordings made before the fix play back correctly.
            if fix_extended(*vk_code, *extended) {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            send_key_input(*vk_code, *scan_code, flags);
        }
    }
}

/// Tracks which keys/mouse buttons a playback pass currently holds down, so a
/// cancelled pass can release them instead of leaving the game with a stuck
/// Shift/W. Pure state machine: feed every performed event to `observe`, ask
/// `releases` for the synthetic up-events. Keys are identified by vk alone —
/// recordings contain OS auto-repeat, so a second down for a held vk is not a
/// second hold.
#[derive(Default)]
struct HeldInputs {
    /// `(vk_code, scan_code, extended)` of each held key, in press order. The
    /// raw recorded `extended` flag is kept; `fix_extended` is applied at
    /// dispatch, same as the down that opened it.
    keys: Vec<(u16, u16, bool)>,
    buttons: Vec<MouseButton>,
}

impl HeldInputs {
    fn observe(&mut self, event: &InputEventType) {
        match event {
            InputEventType::KeyPress { vk_code, scan_code, pressed, extended } => {
                if *pressed {
                    if !self.keys.iter().any(|(vk, _, _)| vk == vk_code) {
                        self.keys.push((*vk_code, *scan_code, *extended));
                    }
                } else {
                    self.keys.retain(|(vk, _, _)| vk != vk_code);
                }
            }
            InputEventType::MouseButton { button, pressed } => {
                if *pressed {
                    if !self.buttons.contains(button) {
                        self.buttons.push(button.clone());
                    }
                } else {
                    self.buttons.retain(|b| b != button);
                }
            }
            InputEventType::MouseMove { .. } | InputEventType::MouseWheel { .. } => {}
        }
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Up-events for everything still held, most-recent press first (unwind
    /// modifiers the way a human would).
    fn releases(&self) -> Vec<InputEventType> {
        let keys = self.keys.iter().rev().map(|&(vk_code, scan_code, extended)| {
            InputEventType::KeyPress { vk_code, scan_code, pressed: false, extended }
        });
        let buttons = self.buttons.iter().rev().map(|button| InputEventType::MouseButton {
            button: button.clone(),
            pressed: false,
        });
        keys.chain(buttons).collect()
    }
}

/// Dispatch all of `held`'s release events as ONE SendInput call under
/// INPUT_LOCK, so monitor/burst/pet presses can't interleave and the next
/// playback can't start mid-sweep.
fn release_held(held: &HeldInputs) {
    if held.is_empty() {
        return;
    }
    let mut inputs: Vec<INPUT> = Vec::new();
    for event in held.releases() {
        match event {
            InputEventType::KeyPress { vk_code, scan_code, extended, .. } => {
                let mut flags = KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP;
                if fix_extended(vk_code, extended) {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                inputs.push(make_kbd(vk_code, scan_code, flags));
            }
            InputEventType::MouseButton { ref button, .. } => {
                let (flags, mouse_data) = mouse_button_flags(button, false);
                inputs.push(make_mouse(flags, 0, 0, mouse_data));
            }
            _ => {}
        }
    }
    dispatch(&mut inputs);
}

/// Shared playback runner for both single-sequence and queue playback. A single
/// sequence is just a queue of one, so both entry points funnel through here.
/// Honors CANCEL, LOOP_MODE, and SHUFFLE_MODE, and zeroes the leading delay
/// between consecutive sequences so queued playlists don't stall between items.
fn run_playback_opts(sequences: Vec<Vec<InputEvent>>, force_once: bool) {
    PLAYING.store(true, Ordering::Release);
    CANCEL.store(false, Ordering::Release);
    // Shuffle only changes which item keeps its leading delay; close enough for a clock.
    let timings: Vec<(i64, i64)> = sequences
        .iter()
        .map(|events| {
            let total: i64 = events.iter().map(|e| e.delay_micros).sum();
            (total, events.first().map_or(0, |e| e.delay_micros))
        })
        .collect();
    PASS_MICROS.store(queue_pass_micros(&timings), Ordering::Release);
    *lock_or_recover(&PLAY_STARTED) = Some(Instant::now());

    std::thread::spawn(move || {
        let timer = PrecisionTimer::new();
        let vs = vscreen();

        let total_sequences = sequences.len();
        println!("[Cadence] Playing {} sequence(s)...", total_sequences);

        let mut order: Vec<usize> = (0..total_sequences).collect();
        let mut cancelled = false;
        // Held state persists across loop-mode passes: an unbalanced pass gets
        // its ups matched by the next pass or swept at cancel.
        let mut held = HeldInputs::default();

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
                        if timer.wait_until_ticks_cancellable(
                            anchor + timer.micros_to_ticks(acc_micros),
                            &CANCEL,
                        ) {
                            println!("[Cadence] Playback cancelled.");
                            cancelled = true;
                            break;
                        }
                    }

                    perform_event(&event.event_type, vs);
                    held.observe(&event.event_type);
                }
                if cancelled {
                    break;
                }
            }

            if cancelled || force_once || !LOOP_MODE.load(Ordering::Acquire) {
                break;
            }
        }

        // Sweep BEFORE clearing PLAYING: take_over/stop_and_wait return on
        // PLAYING == false, so whatever starts next sees a clean key state.
        if cancelled {
            release_held(&held);
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

#[cfg(test)]
mod tests {
    use super::HeldInputs;
    use crate::sequence::{InputEventType, MouseButton};

    fn key(vk: u16, pressed: bool) -> InputEventType {
        InputEventType::KeyPress { vk_code: vk, scan_code: vk + 100, pressed, extended: false }
    }

    fn button(b: MouseButton, pressed: bool) -> InputEventType {
        InputEventType::MouseButton { button: b, pressed }
    }

    #[test]
    fn balanced_pass_holds_nothing() {
        let mut h = HeldInputs::default();
        for e in [key(0x57, true), key(0x57, false), button(MouseButton::Left, true), button(MouseButton::Left, false)] {
            h.observe(&e);
        }
        assert!(h.is_empty());
        assert!(h.releases().is_empty());
    }

    #[test]
    fn held_key_releases_with_same_identity() {
        let mut h = HeldInputs::default();
        h.observe(&InputEventType::KeyPress { vk_code: 0x27, scan_code: 0x4D, pressed: true, extended: true });
        assert_eq!(
            h.releases(),
            vec![InputEventType::KeyPress { vk_code: 0x27, scan_code: 0x4D, pressed: false, extended: true }]
        );
    }

    #[test]
    fn auto_repeat_downs_are_one_hold() {
        let mut h = HeldInputs::default();
        for _ in 0..3 {
            h.observe(&key(0x57, true));
        }
        assert_eq!(h.releases().len(), 1);
        h.observe(&key(0x57, false));
        assert!(h.is_empty());
    }

    #[test]
    fn releases_unwind_in_reverse_press_order() {
        let mut h = HeldInputs::default();
        h.observe(&key(0x41, true)); // A
        h.observe(&key(0xA0, true)); // Shift
        let ups = h.releases();
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0], key(0xA0, false));
        assert_eq!(ups[1], key(0x41, false));
    }

    #[test]
    fn mouse_buttons_tracked_distinctly() {
        let mut h = HeldInputs::default();
        h.observe(&button(MouseButton::X1, true));
        h.observe(&button(MouseButton::X2, true));
        h.observe(&button(MouseButton::X1, false));
        assert_eq!(h.releases(), vec![button(MouseButton::X2, false)]);
    }

    #[test]
    fn moves_and_wheel_are_ignored() {
        let mut h = HeldInputs::default();
        h.observe(&InputEventType::MouseMove { x: 10, y: 20 });
        h.observe(&InputEventType::MouseWheel { delta: -120 });
        assert!(h.is_empty());
    }

    #[test]
    fn interleaved_holds_release_only_whats_down() {
        let mut h = HeldInputs::default();
        h.observe(&key(0x41, true));
        h.observe(&key(0x42, true));
        h.observe(&key(0x41, false));
        assert_eq!(h.releases(), vec![key(0x42, false)]);
    }
}
