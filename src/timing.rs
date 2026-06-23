use winapi::um::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};
use winapi::shared::ntdef::LARGE_INTEGER;

pub struct PrecisionTimer {
    frequency: i64,
}

impl PrecisionTimer {
    pub fn new() -> Self {
        let mut freq: LARGE_INTEGER = unsafe { std::mem::zeroed() };
        unsafe {
            QueryPerformanceFrequency(&mut freq);
        }
        PrecisionTimer {
            frequency: unsafe { *freq.QuadPart() },
        }
    }

    pub fn now_ticks(&self) -> i64 {
        let mut ticks: LARGE_INTEGER = unsafe { std::mem::zeroed() };
        unsafe {
            QueryPerformanceCounter(&mut ticks);
            *ticks.QuadPart()
        }
    }

    pub fn ticks_to_micros(&self, ticks: i64) -> i64 {
        (ticks * 1_000_000) / self.frequency
    }

    /// Sleep-based wait. Relies on the process raising the OS timer resolution
    /// to ~1ms (timeBeginPeriod) at startup, so plain sleeps land within ~1ms.
    ///
    /// We deliberately avoid busy spin-waiting: in a VM with oversubscribed
    /// vCPUs, a spin loop pins the guest vCPU at 100% and starves the game's
    /// own render thread, causing in-game lag while a sequence plays. Sleeping
    /// yields the vCPU back to the scheduler so the game keeps running.
    pub fn precise_wait_micros(&self, micros: i64) {
        if micros <= 0 {
            return;
        }

        let start = self.now_ticks();
        let target_ticks = (micros * self.frequency) / 1_000_000;

        // Sleep toward the target, re-checking the high-resolution clock so we
        // self-correct for the scheduler over-/under-shooting by up to ~1ms.
        loop {
            let remaining_ticks = target_ticks - (self.now_ticks() - start);
            if remaining_ticks <= 0 {
                break;
            }
            let remaining_micros = (remaining_ticks * 1_000_000) / self.frequency;
            if remaining_micros <= 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(remaining_micros as u64));
        }
    }
}
