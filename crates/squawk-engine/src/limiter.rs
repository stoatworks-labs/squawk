/// A zero-lookahead peak limiter, one per output stream.
///
/// Attack is instantaneous: when a sample would exceed the threshold, the gain drops to
/// exactly the value that lands it on the threshold, on that same sample. So the output
/// is hard-guaranteed never to exceed the threshold and there is no overshoot to clip
/// downstream in the L24 conversion.
///
/// The cost of no lookahead is a little harmonic distortion on sharp transients. That is
/// the right trade for intercom: a lookahead limiter buys transparency by adding its
/// window to the mouth-to-ear latency, and on a talkback circuit latency is the thing
/// people actually notice.
pub struct Limiter {
    threshold: f32,
    gain: f32,
    release_coef: f32,
}

impl Limiter {
    /// Default threshold, -1 dBFS, leaving a little headroom below full scale.
    pub const DEFAULT_THRESHOLD: f32 = 0.891_251;
    /// Default release time constant in seconds.
    pub const DEFAULT_RELEASE_S: f32 = 0.050;

    pub fn new(sample_rate: f32) -> Self {
        Self::with_settings(sample_rate, Self::DEFAULT_THRESHOLD, Self::DEFAULT_RELEASE_S)
    }

    pub fn with_settings(sample_rate: f32, threshold: f32, release_s: f32) -> Self {
        let sr = sample_rate.max(1.0);
        Self {
            threshold,
            gain: 1.0,
            release_coef: 1.0 - (-1.0 / (release_s.max(1e-6) * sr)).exp(),
        }
    }

    /// Limit a block in place. Returns the peak absolute value after limiting, for
    /// metering — free, since the loop has already touched every sample.
    pub fn process(&mut self, buf: &mut [f32]) -> f32 {
        let mut peak = 0.0f32;
        for x in buf.iter_mut() {
            let a = x.abs();
            let target = if a > self.threshold { self.threshold / a } else { 1.0 };

            if target < self.gain {
                self.gain = target;
            } else {
                self.gain += (target - self.gain) * self.release_coef;
            }

            let y = *x * self.gain;
            *x = y;
            peak = peak.max(y.abs());
        }
        peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_exceeds_threshold() {
        let mut lim = Limiter::new(48_000.0);
        // Well over full scale, held, then released.
        let mut buf = vec![4.0f32; 256];
        buf.extend(std::iter::repeat_n(-4.0, 256));
        let peak = lim.process(&mut buf);
        assert!(
            peak <= Limiter::DEFAULT_THRESHOLD + 1e-6,
            "peak {peak} exceeded threshold"
        );
        assert!(buf.iter().all(|s| s.abs() <= Limiter::DEFAULT_THRESHOLD + 1e-6));
    }

    #[test]
    fn passes_quiet_signal_untouched() {
        let mut lim = Limiter::new(48_000.0);
        let mut buf = vec![0.25f32; 128];
        lim.process(&mut buf);
        assert!(buf.iter().all(|s| (s - 0.25).abs() < 1e-6));
    }
}
