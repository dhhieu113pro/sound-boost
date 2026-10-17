//! Automatic gain control: the part that makes quiet videos loud and loud
//! videos no louder than everything else.

use crate::envelope::{block_coeff, one_pole_coeff, RmsEnvelope};

/// Tuning for the auto leveler. All levels are dBFS, all times are
/// milliseconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgcParams {
    /// Short-term level the leveler aims for.
    pub target_db: f32,
    /// Upper clamp for automatic gain (never add more than this).
    pub max_gain_db: f32,
    /// Lower clamp. `0.0` means the leveler never attenuates on its own.
    pub min_gain_db: f32,
    /// `0.0` = leveling off, `1.0` = full leveling. Scales the computed gain.
    pub strength: f32,
    /// Below this input level the gain is frozen instead of raised, so
    /// silence and low hiss are never amplified.
    pub gate_db: f32,
    /// How fast the gain drops when the source gets loud.
    pub attack_ms: f32,
    /// How fast the gain rises when the source gets quiet.
    pub release_ms: f32,
    /// Frames between gain recomputations. Larger = cheaper, less precise.
    pub control_block: usize,
}

impl Default for AgcParams {
    fn default() -> Self {
        Self {
            target_db: -18.0,
            max_gain_db: 18.0,
            min_gain_db: 0.0,
            strength: 1.0,
            gate_db: -50.0,
            attack_ms: 5.0,
            release_ms: 300.0,
            control_block: 64,
        }
    }
}

/// What the leveler decided for the last window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgcStatus {
    /// Measured input level, dBFS.
    pub level_db: f32,
    /// Automatic gain currently applied, dB (may be negative if attenuation
    /// was allowed via `min_gain_db`).
    pub gain_db: f32,
    /// True while the signal sits below `gate_db` and the gain is frozen.
    pub gated: bool,
}

pub struct Agc {
    sample_rate: f32,
    params: AgcParams,
    detector: RmsEnvelope,
    gain_db: f32,
    attack_coeff: f32,
    release_coeff: f32,
}

impl Agc {
    pub fn new(sample_rate: f32, params: AgcParams) -> Self {
        let mut agc = Self {
            sample_rate,
            params,
            detector: RmsEnvelope::new(sample_rate, params.attack_ms, params.release_ms),
            gain_db: 0.0,
            attack_coeff: 1.0,
            release_coeff: 1.0,
        };
        agc.rebuild();
        agc
    }

    fn rebuild(&mut self) {
        self.detector.set_times(
            self.sample_rate,
            self.params.attack_ms,
            self.params.release_ms,
        );
        self.attack_coeff = one_pole_coeff(self.sample_rate, self.params.attack_ms);
        self.release_coeff = one_pole_coeff(self.sample_rate, self.params.release_ms);
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.rebuild();
    }

    pub fn set_params(&mut self, params: AgcParams) {
        self.params = params;
        self.rebuild();
    }

    pub fn params(&self) -> AgcParams {
        self.params
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        self.gain_db = 0.0;
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    pub fn level_db(&self) -> f32 {
        self.detector.level_db()
    }

    /// Feed the linked window level and get the gain to apply to that window.
    ///
    /// `window_mean_square` is the loudest channel's mean square over the
    /// window, so a stereo pair keeps its balance (linked detection).
    pub fn update(&mut self, window_mean_square: f32, window_len: usize) -> AgcStatus {
        let level_db = self.detector.process_window(window_mean_square, window_len);
        let gated = level_db <= self.params.gate_db;

        if !gated {
            let strength = self.params.strength.clamp(0.0, 1.0);
            let low = self.params.min_gain_db.min(self.params.max_gain_db);
            let high = self.params.min_gain_db.max(self.params.max_gain_db);
            let wanted = ((self.params.target_db - level_db) * strength).clamp(low, high);

            // Gain falls fast (protect), rises slowly (stay musical).
            let coeff = if wanted < self.gain_db {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            let effective = block_coeff(coeff, window_len);
            self.gain_db += effective * (wanted - self.gain_db);
        }

        AgcStatus {
            level_db,
            gain_db: self.gain_db,
            gated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn feed(agc: &mut Agc, mean_square: f32, windows: usize) -> AgcStatus {
        let mut status = AgcStatus::default();
        for _ in 0..windows {
            status = agc.update(mean_square, 64);
        }
        status
    }

    #[test]
    fn quiet_input_is_lifted_towards_target() {
        let params = AgcParams {
            max_gain_db: 30.0,
            ..AgcParams::default()
        };
        let mut agc = Agc::new(48_000.0, params);
        // -40 dBFS, 7500 windows of 64 samples = 10 s.
        let status = feed(&mut agc, db::linear_from_db(-40.0).powi(2), 7_500);
        assert!(!status.gated);
        assert!(
            (status.gain_db - 22.0).abs() < 1.0,
            "gain {}",
            status.gain_db
        );
    }

    #[test]
    fn loud_input_is_never_boosted_past_zero_db() {
        let mut agc = Agc::new(48_000.0, AgcParams::default());
        let status = feed(&mut agc, db::linear_from_db(-6.0).powi(2), 7_500);
        assert!(status.gain_db <= 0.0, "gain {}", status.gain_db);
        assert!(status.gain_db > -0.001);
    }

    #[test]
    fn max_gain_is_respected() {
        let params = AgcParams {
            max_gain_db: 12.0,
            // Below the default gate, so open it up for this test.
            gate_db: -70.0,
            ..AgcParams::default()
        };
        let mut agc = Agc::new(48_000.0, params);
        let status = feed(&mut agc, db::linear_from_db(-60.0).powi(2), 40_000);
        assert!(
            (status.gain_db - 12.0).abs() < 0.1,
            "gain {}",
            status.gain_db
        );
    }

    #[test]
    fn gate_freezes_gain_below_threshold() {
        let mut agc = Agc::new(48_000.0, AgcParams::default());
        // Start loud so the gain settles.
        feed(&mut agc, db::linear_from_db(-20.0).powi(2), 2_000);

        // Go silent and wait for the release envelope to fall below the gate.
        let mut status = AgcStatus::default();
        for _ in 0..20_000 {
            status = agc.update(0.0, 64);
            if status.gated {
                break;
            }
        }
        assert!(status.gated, "gate never closed");
        let frozen = status.gain_db;

        // More silence must not move the gain at all.
        for _ in 0..2_000 {
            status = agc.update(0.0, 64);
        }
        assert!(status.gated);
        assert!(
            (status.gain_db - frozen).abs() < 1e-6,
            "gain moved while gated"
        );
    }

    #[test]
    fn strength_scales_the_gain() {
        let params = AgcParams {
            strength: 0.5,
            ..AgcParams::default()
        };
        let mut agc = Agc::new(48_000.0, params);
        let status = feed(&mut agc, db::linear_from_db(-30.0).powi(2), 30_000);
        // Full leveling would give +12 dB; half strength => +6 dB.
        assert!(
            (status.gain_db - 6.0).abs() < 0.75,
            "gain {}",
            status.gain_db
        );
    }
}
