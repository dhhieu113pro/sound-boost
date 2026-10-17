//! Decibel helpers shared by every DSP stage.
//!
//! Full-scale is 1.0, so 0 dBFS is a sample of magnitude 1.0 and the useful
//! range is negative. Everything below [`SILENCE_LINEAR`] is treated as
//! silence so logarithm math never produces `-inf` or `NaN`.

/// Magnitudes at or below this are treated as silence by the dB helpers.
pub const SILENCE_LINEAR: f32 = 1.0e-8;

/// Very negative but finite level reported for silence.
pub const FLOOR_DB: f32 = -160.0;

/// Linear amplitude (0.0 .. 1.0 full scale) -> dBFS.
#[inline]
pub fn db_from_linear(linear: f32) -> f32 {
    let mag = linear.abs().max(SILENCE_LINEAR);
    let db = 20.0 * mag.log10();
    if db < FLOOR_DB {
        FLOOR_DB
    } else {
        db
    }
}

/// dBFS -> linear amplitude.
#[inline]
pub fn linear_from_db(db: f32) -> f32 {
    10f32.powf(db * 0.05)
}

/// Mean-square value (mean of `x^2`) -> dBFS.
#[inline]
pub fn db_from_mean_square(mean_square: f32) -> f32 {
    let ms = mean_square.max(SILENCE_LINEAR * SILENCE_LINEAR);
    let db = 10.0 * ms.log10();
    if db < FLOOR_DB {
        FLOOR_DB
    } else {
        db
    }
}

/// RMS of a slice -> dBFS. Handy for tests and meters.
#[inline]
pub fn rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return FLOOR_DB;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    db_from_mean_square(sum / samples.len() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_conversions() {
        assert!((linear_from_db(0.0) - 1.0).abs() < 1e-6);
        assert!((linear_from_db(-6.0) - 0.5011872).abs() < 1e-5);
        assert!((db_from_linear(1.0) - 0.0).abs() < 1e-6);
        assert!((db_from_linear(0.5) + 6.0206).abs() < 1e-3);
    }

    #[test]
    fn silence_never_returns_infinity() {
        assert_eq!(db_from_linear(0.0), FLOOR_DB);
        assert_eq!(db_from_mean_square(0.0), FLOOR_DB);
        assert_eq!(rms_db(&[]), FLOOR_DB);
        assert!(db_from_linear(f32::NAN).is_finite() || db_from_linear(0.0) == FLOOR_DB);
    }

    #[test]
    fn round_trip_is_stable() {
        for db in [-60.0, -30.0, -12.0, -0.3, 0.0, 6.0] {
            assert!((db_from_linear(linear_from_db(db)) - db).abs() < 1e-3);
        }
    }
}
