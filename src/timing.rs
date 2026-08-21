use winapi::um::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};
use winapi::shared::ntdef::LARGE_INTEGER;
use std::sync::atomic::{AtomicBool, Ordering};

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

    /// Like `wait_until_ticks`, but wakes early once `cancel` is set. Sleeps in
    /// ~20ms slices, checking `cancel` between slices; the final <=0.7ms spin
    /// tail runs exactly as in `wait_until_ticks`, so uncancelled timing
    /// precision is unchanged (same absolute target, same final approach).
    /// Returns true when the wait was abandoned because `cancel` was set.
    pub fn wait_until_ticks_cancellable(&self, target_ticks: i64, cancel: &AtomicBool) -> bool {
        const SPIN_MARGIN_MICROS: i64 = 700;
        const CANCEL_SLICE_MICROS: i64 = 20_000;

        loop {
            if cancel.load(Ordering::Acquire) {
                return true;
            }
            let remaining_micros = self.ticks_to_micros(target_ticks - self.now_ticks());
            match sleep_slice_micros(remaining_micros, SPIN_MARGIN_MICROS, CANCEL_SLICE_MICROS) {
                Some(sleep_micros) => {
                    std::thread::sleep(std::time::Duration::from_micros(sleep_micros as u64));
                }
                None => {
                    // Done, or inside the spin margin: finish with the exact
                    // final approach `wait_until_ticks` uses (no cancel checks
                    // in the <=0.7ms tail).
                    self.wait_until_ticks(target_ticks);
                    return false;
                }
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

/// Decide the next sleep length (micros) for a sliced cancellable wait:
/// `None` = done or inside the spin tail, `Some(n)` = sleep `n` micros next.
/// Pure so the slicing decision is unit-testable without the QPC clock.
fn sleep_slice_micros(remaining_micros: i64, spin_margin_micros: i64, slice_micros: i64) -> Option<i64> {
    if remaining_micros <= spin_margin_micros {
        return None;
    }
    Some((remaining_micros - spin_margin_micros).min(slice_micros))
}

#[cfg(test)]
mod tests {
    use super::sleep_slice_micros;

    #[test]
    fn done_or_behind_schedule_never_sleeps() {
        assert_eq!(sleep_slice_micros(0, 700, 20_000), None);
        assert_eq!(sleep_slice_micros(-5_000, 700, 20_000), None);
    }

    #[test]
    fn inside_spin_margin_enters_the_tail() {
        assert_eq!(sleep_slice_micros(700, 700, 20_000), None);
        assert_eq!(sleep_slice_micros(300, 700, 20_000), None);
    }

    #[test]
    fn just_above_margin_sleeps_the_difference() {
        assert_eq!(sleep_slice_micros(1_000, 700, 20_000), Some(300));
    }

    #[test]
    fn long_wait_is_capped_at_one_slice() {
        assert_eq!(sleep_slice_micros(5_000_000, 700, 20_000), Some(20_000));
    }
}
