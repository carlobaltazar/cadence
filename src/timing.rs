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

    /// Convert microseconds to QPC ticks (inverse of `ticks_to_micros`).
    pub fn micros_to_ticks(&self, micros: i64) -> i64 {
        (micros * self.frequency) / 1_000_000
    }

    /// Wait until the QPC clock reaches `target_ticks`.
    ///
    /// Hybrid pacing: sleep off the bulk of the wait (yielding the vCPU) and
    /// busy-spin only the final ~0.7ms for sub-millisecond precision. When we're
    /// already at/behind the target, returns immediately without spinning.
    ///
    /// The tiny spin tail restores accuracy the pure-sleep version lost while
    /// keeping the VM-lag fix intact: long cooldown gaps sleep almost entirely
    /// and spin only their tail, and a run that's behind schedule never spins.
    pub fn wait_until_ticks(&self, target_ticks: i64) {
        // ~0.7ms spin tail. Larger = sharper timing but more CPU; smaller = less
        // CPU but the OS sleep granularity (~1ms) leaks through. Tune here.
        const SPIN_MARGIN_MICROS: i64 = 700;
        let spin_margin = self.micros_to_ticks(SPIN_MARGIN_MICROS);

        loop {
            let remaining = target_ticks - self.now_ticks();
            if remaining <= 0 {
                break;
            }
            if remaining > spin_margin {
                // Sleep off everything but the spin margin, yielding the vCPU.
                let sleep_micros = self.ticks_to_micros(remaining - spin_margin);
                if sleep_micros > 0 {
                    std::thread::sleep(std::time::Duration::from_micros(sleep_micros as u64));
                }
            } else {
                // Final approach: busy-spin for sub-millisecond precision.
                std::hint::spin_loop();
            }
        }
    }

    /// Wait `micros` from now, using the hybrid `wait_until_ticks` pacing.
    pub fn precise_wait_micros(&self, micros: i64) {
        if micros <= 0 {
            return;
        }
        self.wait_until_ticks(self.now_ticks() + self.micros_to_ticks(micros));
    }
}
