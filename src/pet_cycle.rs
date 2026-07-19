use crate::player;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

pub fn start(interval_secs: u64) {
    if ACTIVE.load(Ordering::Acquire) {
        return; // already running
    }

    CANCEL.store(false, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);

    thread::spawn(move || {
        println!("[Cadence] Pet cycle started (interval: {}s)", interval_secs);

        // Get scan code for 'A' key (VK 0x41)
        let vk: u16 = 0x41;
        let scan_code = player::scan_code(vk);

        loop {
            // Sleep in 1-second chunks so we can respond to cancel quickly
            for _ in 0..interval_secs {
                if CANCEL.load(Ordering::Acquire) {
                    println!("[Cadence] Pet cycle stopped.");
                    ACTIVE.store(false, Ordering::Release);
                    return;
                }
                thread::sleep(Duration::from_secs(1));
            }

            // Check cancel one more time before sending keys
            if CANCEL.load(Ordering::Acquire) {
                break;
            }

            player::send_key_press(vk, scan_code);

            thread::sleep(Duration::from_millis(300));

            player::send_key_press(vk, scan_code);

            println!("[Cadence] Pet cycle: hide/call sent.");
        }

        println!("[Cadence] Pet cycle stopped.");
        ACTIVE.store(false, Ordering::Release);
    });
}

pub fn stop() {
    if ACTIVE.load(Ordering::Acquire) {
        CANCEL.store(true, Ordering::Release);
    }
}
