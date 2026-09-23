use std::hint::black_box;
use std::time::{Duration, Instant};

/// Executes only the operation inside the measured region.
pub struct Bencher {
    pub(crate) iterations: u64,
    pub(crate) elapsed: Duration,
    pub(crate) profile: bool,
    pub(crate) calls: u32,
}

impl Bencher {
    pub(crate) fn new(iterations: u64, profile: bool) -> Self {
        Self {
            iterations,
            elapsed: Duration::ZERO,
            profile,
            calls: 0,
        }
    }

    /// Time repeated calls, including destruction of their return values.
    pub fn iter<T>(&mut self, mut operation: impl FnMut() -> T) {
        self.calls += 1;
        if self.profile {
            self.start_profile();
            for _ in 0..self.iterations {
                black_box(operation());
            }
            self.stop_profile();
        } else {
            let start = Instant::now();
            for _ in 0..self.iterations {
                black_box(operation());
            }
            self.elapsed += start.elapsed();
        }
    }

    /// Prepare each input outside measurement and drop returned outputs outside it.
    /// Input destruction within the consuming operation remains part of its cost.
    pub fn iter_batched<I, O>(
        &mut self,
        mut setup: impl FnMut() -> I,
        mut operation: impl FnMut(I) -> O,
    ) {
        self.calls += 1;
        for _ in 0..self.iterations {
            let input = black_box(setup());
            let output = if self.profile {
                self.start_profile();
                let output = black_box(operation(input));
                self.stop_profile();
                output
            } else {
                let start = Instant::now();
                let output = black_box(operation(input));
                self.elapsed += start.elapsed();
                output
            };
            drop(output);
        }
    }

    fn start_profile(&self) {
        #[cfg(feature = "cachegrind")]
        if self.profile {
            gungraun::client_requests::cachegrind::start_instrumentation();
        }
    }

    fn stop_profile(&self) {
        #[cfg(feature = "cachegrind")]
        if self.profile {
            gungraun::client_requests::cachegrind::stop_instrumentation();
        }
    }
}

/// Header version used for function-region instrumentation, when enabled.
pub(crate) fn header_version() -> Option<(u32, u32)> {
    #[cfg(feature = "cachegrind")]
    {
        gungraun::client_requests::VALGRIND_VERSION
    }
    #[cfg(not(feature = "cachegrind"))]
    {
        None
    }
}

/// Discard the cold call, then scale warm batches until their measured work meets
/// the requested duration. Setup remains outside the elapsed duration supplied here.
pub(crate) fn calibrate(
    target: Duration,
    mut measure: impl FnMut(u64) -> crate::Result<Duration>,
) -> crate::Result<u64> {
    measure(1)?;
    let mut iterations = 1u64;
    loop {
        let elapsed = measure(iterations)?;
        if elapsed >= target {
            return Ok(iterations);
        }
        anyhow::ensure!(
            iterations < 1_000_000_000,
            "function calibration exceeded its iteration budget; set --iterations explicitly"
        );
        let ratio = target
            .as_nanos()
            .div_ceil(elapsed.as_nanos().max(1))
            .clamp(2, 10) as u64;
        iterations = iterations.saturating_mul(ratio).min(1_000_000_000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calibration_ignores_cold_cost_and_reaches_target_with_warm_batches() {
        let mut calls = 0;
        let iterations = calibrate(Duration::from_millis(100), |iterations| {
            calls += 1;
            Ok(if calls == 1 {
                Duration::from_secs(1)
            } else {
                Duration::from_micros(iterations * 10)
            })
        })
        .unwrap();
        assert!(calls > 2);
        assert!(Duration::from_micros(iterations * 10) >= Duration::from_millis(100));
    }
}
