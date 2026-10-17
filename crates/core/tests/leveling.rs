//! End-to-end checks for the boost chain, expressed the way the user thinks
//! about it: quiet videos should come up, loud ones should not, nothing may
//! clip, and silence must stay silent.
//!
//! Levels here are RMS dBFS, which is what the AGC targets, so a source
//! described as `-40.0` has an RMS of -40 dBFS (its peaks sit 3 dB higher).

use sb_core::agc::AgcParams;
use sb_core::chain::{BoostChain, BoostParams};
use sb_core::db;
use sb_core::limiter::LimiterParams;
use sb_core::params::{percent_to_gain_db, Preset};

const FS: f32 = 48_000.0;

/// A sine whose RMS level is `rms_db` dBFS.
fn sine(freq: f32, seconds: f32, rms_db: f32, channels: usize) -> Vec<f32> {
    let amplitude = db::linear_from_db(rms_db) * std::f32::consts::SQRT_2;
    let frames = (seconds * FS) as usize;
    let mut buffer = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        let value = amplitude * (2.0 * std::f32::consts::PI * freq * frame as f32 / FS).sin();
        for _ in 0..channels {
            buffer.push(value);
        }
    }
    buffer
}

fn chain_params(agc: AgcParams, manual_gain_db: f32) -> BoostParams {
    BoostParams {
        enabled: true,
        manual_gain_db,
        agc,
        limiter: LimiterParams {
            ceiling_db: -0.3,
            lookahead_ms: 5.0,
            release_ms: 100.0,
        },
    }
}

/// RMS level of the last `seconds` of a buffer.
fn tail_rms_db(buffer: &[f32], channels: usize, seconds: f32) -> f32 {
    let samples = (seconds * FS) as usize * channels;
    db::rms_db(&buffer[buffer.len() - samples..])
}

/// RMS level of one channel over the last `seconds`.
fn tail_channel_db(buffer: &[f32], channels: usize, channel: usize, seconds: f32) -> f32 {
    let frames = (seconds * FS) as usize;
    let start = buffer.len() / channels - frames;
    let samples: Vec<f32> = (start..start + frames)
        .map(|frame| buffer[frame * channels + channel])
        .collect();
    db::rms_db(&samples)
}

fn peak(buffer: &[f32]) -> f32 {
    buffer.iter().fold(0.0f32, |acc, s| acc.max(s.abs()))
}

#[test]
fn quiet_source_is_lifted_towards_the_target() {
    let agc = AgcParams {
        target_db: -18.0,
        max_gain_db: 30.0,
        min_gain_db: 0.0,
        strength: 1.0,
        gate_db: -60.0,
        attack_ms: 10.0,
        release_ms: 300.0,
        control_block: 64,
    };
    let mut chain = BoostChain::new(FS, 1, chain_params(agc, 0.0));

    // A quiet video: -40 dBFS for six seconds.
    let mut buffer = sine(1_000.0, 6.0, -40.0, 1);
    chain.process_interleaved(&mut buffer);

    let level = tail_rms_db(&buffer, 1, 1.0);
    assert!(
        (level + 18.0).abs() < 2.0,
        "expected about -18 dBFS, got {level}"
    );
    assert!(chain.meter().auto_gain_db > 15.0);
}

#[test]
fn loud_source_is_not_boosted_above_itself() {
    let agc = AgcParams {
        target_db: -18.0,
        max_gain_db: 30.0,
        min_gain_db: 0.0,
        strength: 1.0,
        gate_db: -60.0,
        attack_ms: 10.0,
        release_ms: 300.0,
        control_block: 64,
    };
    let mut chain = BoostChain::new(FS, 1, chain_params(agc, 0.0));

    let mut buffer = sine(1_000.0, 4.0, -6.0, 1);
    chain.process_interleaved(&mut buffer);

    let level = tail_rms_db(&buffer, 1, 1.0);
    assert!(
        (level + 6.0).abs() < 1.5,
        "a loud source should stay where it is, got {level}"
    );
    // The leveler must not pull loud material down.
    assert!(chain.meter().auto_gain_db.abs() < 0.5);
}

#[test]
fn quiet_and_loud_sources_end_up_much_closer_together() {
    // The whole point of the feature: two sources 30 dB apart should land far
    // closer than they started. A residual gap remains on purpose, because
    // the leveler never attenuates: loud material is left alone rather than
    // being turned down (min_gain_db = 0).
    let preset = Preset::Speech;

    let mut quiet_chain = BoostChain::new(FS, 1, preset.boost_params(0.0));
    let mut loud_chain = BoostChain::new(FS, 1, preset.boost_params(0.0));

    let mut quiet = sine(440.0, 6.0, -38.0, 1);
    let mut loud = sine(440.0, 6.0, -8.0, 1);
    quiet_chain.process_interleaved(&mut quiet);
    loud_chain.process_interleaved(&mut loud);

    let quiet_level = tail_rms_db(&quiet, 1, 1.0);
    let loud_level = tail_rms_db(&loud, 1, 1.0);
    let gap = (quiet_level - loud_level).abs();

    assert!(
        gap < 15.0,
        "levels stayed apart: {quiet_level} vs {loud_level}"
    );
    assert!(
        quiet_level > -25.0,
        "quiet source was not lifted at all: {quiet_level}"
    );
    assert!(
        loud_level > -12.0,
        "loud source was pulled down: {loud_level}"
    );
}

#[test]
fn manual_boost_scales_the_output() {
    // Leveling off, so only the slider acts.
    let agc = AgcParams {
        strength: 0.0,
        max_gain_db: 0.0,
        gate_db: -90.0,
        ..AgcParams::default()
    };
    let manual = percent_to_gain_db(500.0);
    let mut chain = BoostChain::new(FS, 1, chain_params(agc, manual));

    let mut buffer = sine(1_000.0, 2.0, -30.0, 1);
    chain.process_interleaved(&mut buffer);

    let level = tail_rms_db(&buffer, 1, 1.0);
    let expected = -30.0 + manual;
    assert!(
        (level - expected).abs() < 0.5,
        "expected {expected} dBFS, got {level}"
    );
}

#[test]
fn limiter_ceiling_holds_at_maximum_boost() {
    let agc = AgcParams {
        strength: 0.0,
        max_gain_db: 0.0,
        gate_db: -90.0,
        ..AgcParams::default()
    };
    let manual = percent_to_gain_db(500.0);
    let mut chain = BoostChain::new(FS, 1, chain_params(agc, manual));

    // A full-scale sine (peaks at 1.0), then boosted by 500 %.
    let mut buffer = sine(997.0, 2.0, -3.0103, 1);
    chain.process_interleaved(&mut buffer);

    let ceiling = db::linear_from_db(-0.3);
    let peak = peak(&buffer);
    assert!(
        peak <= ceiling + 1e-4,
        "peak {peak} exceeded ceiling {ceiling}"
    );
    assert!(
        (peak - ceiling).abs() < 1e-3,
        "expected the limiter to sit right on the ceiling, peak {peak}"
    );
}

#[test]
fn transient_does_not_escape_the_limiter() {
    let agc = AgcParams {
        strength: 0.0,
        max_gain_db: 0.0,
        gate_db: -90.0,
        ..AgcParams::default()
    };
    let manual = percent_to_gain_db(500.0);
    let mut chain = BoostChain::new(FS, 1, chain_params(agc, manual));

    let mut buffer = vec![0.0f32; 9_600];
    buffer[4_000] = 1.0; // a single full-scale click
    chain.process_interleaved(&mut buffer);

    let ceiling = db::linear_from_db(-0.3);
    let peak = peak(&buffer);
    assert!(peak <= ceiling + 1e-4, "peak {peak}");
}

#[test]
fn silence_is_never_amplified() {
    let mut chain = BoostChain::new(FS, 2, Preset::Night.boost_params(percent_to_gain_db(500.0)));
    let mut buffer = vec![0.0f32; 2 * 48_000];
    chain.process_interleaved(&mut buffer);
    assert!(buffer.iter().all(|s| *s == 0.0));
    assert!(chain.meter().gated, "gate should report silence");
}

#[test]
fn stereo_pair_stays_balanced() {
    // Left silent, right quiet: gain must be shared, not applied per channel.
    let agc = AgcParams {
        target_db: -18.0,
        max_gain_db: 30.0,
        gate_db: -60.0,
        attack_ms: 10.0,
        release_ms: 300.0,
        ..AgcParams::default()
    };
    let mut chain = BoostChain::new(FS, 2, chain_params(agc, 0.0));

    let frames = (6.0 * FS) as usize;
    let amplitude = db::linear_from_db(-30.0) * std::f32::consts::SQRT_2;
    let mut buffer = Vec::with_capacity(frames * 2);
    for frame in 0..frames {
        let right = amplitude * (2.0 * std::f32::consts::PI * 500.0 * frame as f32 / FS).sin();
        buffer.push(0.0); // left stays silent
        buffer.push(right);
    }
    chain.process_interleaved(&mut buffer);

    let left: Vec<f32> = buffer.chunks_exact(2).map(|frame| frame[0]).collect();
    assert!(left.iter().all(|s| *s == 0.0), "silent channel moved");

    let right_level = tail_channel_db(&buffer, 2, 1, 1.0);
    assert!(
        (right_level + 18.0).abs() < 2.5,
        "right channel should reach the target, got {right_level}"
    );
}

#[test]
fn gain_moves_smoothly_between_quiet_and_loud() {
    let preset = Preset::Speech;
    let mut chain = BoostChain::new(FS, 1, preset.boost_params(0.0));

    // Alternate quiet and loud passages every 250 ms.
    let mut buffer = Vec::new();
    for block in 0..8 {
        let level = if block % 2 == 0 { -40.0 } else { -5.0 };
        buffer.extend(sine(600.0, 0.25, level, 1));
    }

    let mut previous = f32::NAN;
    let mut largest_step: f32 = 0.0;
    for chunk in buffer.chunks_mut(256) {
        chain.process_interleaved(chunk);
        let gain = chain.meter().auto_gain_db;
        if previous.is_finite() {
            largest_step = largest_step.max((gain - previous).abs());
        }
        previous = gain;
    }

    assert!(
        largest_step < 8.0,
        "gain jumped {largest_step} dB in one 256-frame window"
    );
}

#[test]
fn presets_all_process_without_panicking() {
    for preset in Preset::ALL {
        let mut chain = BoostChain::new(FS, 2, preset.boost_params(percent_to_gain_db(250.0)));
        let mut buffer = sine(300.0, 0.5, -25.0, 2);
        chain.process_interleaved(&mut buffer);
        let _ = chain.meter();
    }
}
