//! One-pole RMS envelope follower with independent attack and release.

use crate::db;

/// Follower that tracks the mean-square value of a signal.
///
/// Attack is used while the signal is getting louder, release while it is
/// getting quieter, so a quiet passage is tracked slowly (no gain pumping on
/// every syllable) while a sudden loud hit is tracked immediately.
pub struct RmsEnvelope {
    attack_coeff: f32,
    release_coeff: f32,
    mean_square: f32,
}

impl RmsEnvelope {
    pub fn new(sample_rate: f32, attack_ms: f32, release_ms: f32) -> Self {
        let mut envelope = Self {
            attack_coeff: 1.0,
            release_coeff: 1.0,
            mean_square: 0.0,
        };
        envelope.set_times(sample_rate, attack_ms, release_ms);
        envelope
    }

    pub fn set_times(&mut self, sample_rate: f32, attack_ms: f32, release_ms: f32) {
        self.attack_coeff = one_pole_coeff(sample_rate, attack_ms);
        self.release_coeff = one_pole_coeff(sample_rate, release_ms);
    }

    pub fn reset(&mut self) {
        self.mean_square = 0.0;
    }

    #[inline]
    pub fn mean_square(&self) -> f32 {
        self.mean_square
    }

    #[inline]
    pub fn rms(&self) -> f32 {
        self.mean_square.max(0.0).sqrt()
    }

    /// Current tracked level in dBFS.
    #[inline]
    pub fn level_db(&self) -> f32 {
        db::db_from_mean_square(self.mean_square)
    }

    /// Track a single sample, returning the level in dBFS.
    #[inline]
    pub fn process_sample(&mut self, sample: f32) -> f32 {
        let square = sample * sample;
        let coeff = if square > self.mean_square {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.mean_square += coeff * (square - self.mean_square);
        self.level_db()
    }

    /// Track a whole window at once.
    ///
    /// `window_mean_square` is the mean of `x^2` over `window_len` samples.
    /// The per-sample one-pole is collapsed into a single step for the whole
    /// window (`1 - (1 - c)^n`), which is exact for a one-pole and avoids a
    /// `powf`/`sqrt` per sample on the audio thread.
    #[inline]
    pub fn process_window(&mut self, window_mean_square: f32, window_len: usize) -> f32 {
        let coeff = if window_mean_square > self.mean_square {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        let effective = block_coeff(coeff, window_len);
        self.mean_square += effective * (window_mean_square - self.mean_square);
        self.level_db()
    }
}

/// One-pole coefficient for a given time constant.
#[inline]
pub(crate) fn one_pole_coeff(sample_rate: f32, time_ms: f32) -> f32 {
    if time_ms <= 0.0 || sample_rate <= 0.0 {
        return 1.0;
    }
    let tau = (time_ms * 0.001) * sample_rate;
    if tau < 1.0 {
        return 1.0;
    }
    (1.0 - (-1.0 / tau).exp()).clamp(0.0, 1.0)
}

/// Effective coefficient of a one-pole applied for `n` consecutive samples.
#[inline]
pub(crate) fn block_coeff(coeff: f32, n: usize) -> f32 {
    match n {
        0 => 0.0,
        _ => {
            let coeff = coeff.clamp(0.0, 1.0);
            1.0 - (1.0 - coeff).powi(n.min(i32::MAX as usize) as i32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_constants_are_sane() {
        // 10 ms at 48 kHz is ~480 samples to reach 63 %.
        let c = one_pole_coeff(48_000.0, 10.0);
        assert!(c > 0.001 && c < 0.01, "coeff was {c}");
        assert_eq!(one_pole_coeff(48_000.0, 0.0), 1.0);
    }

    #[test]
    fn block_coeff_matches_per_sample_iteration() {
        let c = one_pole_coeff(48_000.0, 10.0);
        let n = 64;
        let mut iterative = 0.0f32;
        for _ in 0..n {
            iterative += c * (1.0 - iterative);
        }
        let blocked = block_coeff(c, n);
        assert!((iterative - blocked).abs() < 1e-4);
    }

    #[test]
    fn window_tracking_converges_to_level() {
        let mut envelope = RmsEnvelope::new(48_000.0, 5.0, 50.0);
        // -20 dBFS sine, tracked in 64-sample windows.
        let amp = crate::db::linear_from_db(-20.0);
        let mut phase = 0.0f32;
        let mut level = 0.0;
        for _ in 0..2_000 {
            let mut sum = 0.0;
            for _ in 0..64 {
                let s = amp * phase.sin();
                sum += s * s;
                phase += 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0;
            }
            level = envelope.process_window(sum / 64.0, 64);
        }
        assert!((level + 23.0).abs() < 0.5, "level was {level}");
    }
}
