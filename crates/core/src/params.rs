//! User-facing tunings and unit conversions.

use crate::agc::AgcParams;
use crate::chain::BoostParams;
use crate::db;
use crate::limiter::LimiterParams;

/// The slider is expressed as a percentage of the original signal, matching
/// what a sound booster advertises: 100 % is untouched, 500 % is +14 dB.
pub const MIN_PERCENT: f32 = 100.0;
pub const MAX_PERCENT: f32 = 500.0;

/// Ready-made tunings. They differ mainly in how fast the leveler may move:
/// fast for speech, slow for music, aggressive for late-night listening.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Preset {
    /// Bypass: audio flows through untouched.
    Off,
    /// Podcasts, calls, dialogue: reacts quickly, tightens the quiet parts.
    Speech,
    /// Music and general use: slow movements, keeps dynamics intact.
    #[default]
    Music,
    /// Late-night: heavy leveling, lower ceiling so peaks stay unobtrusive.
    Night,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Off, Preset::Speech, Preset::Music, Preset::Night];

    pub fn as_u32(self) -> u32 {
        match self {
            Preset::Off => 0,
            Preset::Speech => 1,
            Preset::Music => 2,
            Preset::Night => 3,
        }
    }

    pub fn from_u32(value: u32) -> Self {
        match value {
            0 => Preset::Off,
            1 => Preset::Speech,
            3 => Preset::Night,
            _ => Preset::Music,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Preset::Off => "Off",
            Preset::Speech => "Speech",
            Preset::Music => "Music",
            Preset::Night => "Night",
        }
    }

    /// Full parameter set for this preset with the user's slider applied.
    pub fn boost_params(self, manual_gain_db: f32) -> BoostParams {
        let (enabled, agc, limiter) = match self {
            Preset::Off => (false, AgcParams::default(), LimiterParams::default()),
            Preset::Speech => (
                true,
                AgcParams {
                    target_db: -18.0,
                    max_gain_db: 18.0,
                    min_gain_db: 0.0,
                    strength: 1.0,
                    gate_db: -50.0,
                    attack_ms: 5.0,
                    release_ms: 150.0,
                    control_block: 64,
                },
                LimiterParams {
                    ceiling_db: -0.3,
                    lookahead_ms: 5.0,
                    release_ms: 80.0,
                },
            ),
            Preset::Music => (
                true,
                AgcParams {
                    target_db: -16.0,
                    max_gain_db: 12.0,
                    min_gain_db: 0.0,
                    strength: 1.0,
                    gate_db: -55.0,
                    attack_ms: 15.0,
                    release_ms: 600.0,
                    control_block: 64,
                },
                LimiterParams {
                    ceiling_db: -0.5,
                    lookahead_ms: 5.0,
                    release_ms: 200.0,
                },
            ),
            Preset::Night => (
                true,
                AgcParams {
                    target_db: -20.0,
                    max_gain_db: 20.0,
                    min_gain_db: 0.0,
                    strength: 1.0,
                    gate_db: -45.0,
                    attack_ms: 3.0,
                    release_ms: 300.0,
                    control_block: 64,
                },
                LimiterParams {
                    ceiling_db: -1.0,
                    lookahead_ms: 8.0,
                    release_ms: 120.0,
                },
            ),
        };

        BoostParams {
            enabled,
            manual_gain_db,
            agc,
            limiter,
        }
    }
}

/// 100 % -> 0 dB, 200 % -> +6.02 dB, 500 % -> +13.98 dB.
pub fn percent_to_gain_db(percent: f32) -> f32 {
    db::db_from_linear((percent / 100.0).max(1e-3))
}

/// Inverse of [`percent_to_gain_db`].
pub fn gain_db_to_percent(gain_db: f32) -> f32 {
    db::linear_from_db(gain_db) * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_round_trips() {
        assert!(percent_to_gain_db(100.0).abs() < 1e-4);
        assert!((percent_to_gain_db(200.0) - 6.0206).abs() < 1e-3);
        assert!((percent_to_gain_db(500.0) - 13.9794).abs() < 1e-3);
        for percent in [100.0, 150.0, 250.0, 300.0, 500.0] {
            assert!((gain_db_to_percent(percent_to_gain_db(percent)) - percent).abs() < 1e-2);
        }
    }

    #[test]
    fn presets_are_sane() {
        for preset in Preset::ALL {
            let params = preset.boost_params(percent_to_gain_db(300.0));
            assert_eq!(params.enabled, preset != Preset::Off);
            assert!((params.manual_gain_db - 9.5424).abs() < 1e-3);
            assert!(params.agc.max_gain_db > 0.0);
            assert!(params.agc.target_db < 0.0);
            assert!(params.limiter.ceiling_db < 0.0);
            assert!(params.limiter.lookahead_ms <= crate::limiter::MAX_LOOKAHEAD_MS);
            assert_eq!(preset, Preset::from_u32(preset.as_u32()));
        }
    }
}
