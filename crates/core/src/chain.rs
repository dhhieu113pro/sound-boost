//! The full boost chain that the APO runs on every buffer.

use crate::agc::{Agc, AgcParams, AgcStatus};
use crate::db;
use crate::limiter::{Limiter, LimiterParams};

/// Everything the audio path needs to know, as plain data so it can be
/// copied straight out of a shared control block without locking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoostParams {
    pub enabled: bool,
    /// User slider position in dB. 0 dB = 100 %, +13.98 dB = 500 %.
    pub manual_gain_db: f32,
    pub agc: AgcParams,
    pub limiter: LimiterParams,
}

impl Default for BoostParams {
    fn default() -> Self {
        Self {
            enabled: true,
            manual_gain_db: 0.0,
            agc: AgcParams::default(),
            limiter: LimiterParams::default(),
        }
    }
}

/// Read-only values for meters in the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Meter {
    /// Level of the loudest channel entering the chain, dBFS.
    pub input_db: f32,
    /// Peak leaving the chain in the last window, dBFS.
    pub output_db: f32,
    /// Automatic gain currently applied, dB.
    pub auto_gain_db: f32,
    /// Manual boost currently applied, dB.
    pub manual_gain_db: f32,
    /// Gain reduction from the limiter in the last window, dB (<= 0).
    pub reduction_db: f32,
    /// True while the source is below the noise gate.
    pub gated: bool,
    /// Whether the chain is doing anything at all.
    pub enabled: bool,
}

/// Streams audio through gate -> AGC -> manual boost -> limiter.
///
/// Channels are interleaved and the AGC detector is linked across them, so a
/// stereo pair keeps its balance instead of one side being leveled harder.
pub struct BoostChain {
    sample_rate: f32,
    channels: usize,
    params: BoostParams,
    agc: Agc,
    limiter: Limiter,
    channel_squares: Vec<f32>,
    meter: Meter,
}

impl BoostChain {
    pub fn new(sample_rate: f32, channels: usize, params: BoostParams) -> Self {
        let channels = channels.max(1);
        Self {
            sample_rate,
            channels,
            agc: Agc::new(sample_rate, params.agc),
            limiter: Limiter::new(sample_rate, channels, params.limiter),
            channel_squares: vec![0.0; channels],
            meter: Meter::default(),
            params,
        }
    }

    pub fn params(&self) -> BoostParams {
        self.params
    }

    pub fn meter(&self) -> Meter {
        self.meter
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Latency added by the limiter's look-ahead, in frames.
    pub fn latency_frames(&self) -> usize {
        self.limiter.latency_frames()
    }

    /// Apply new settings to both stages.
    ///
    /// Called from the audio thread when the user's slider or preset changes --
    /// that is a rare event, but it is a real-time one: everything here is
    /// arithmetic on state that was allocated at lock time, and the limiter's
    /// look-ahead (its only buffer-sized value) is fixed for the life of the
    /// instance, so no delay line is ever rewritten mid-stream.
    pub fn set_params(&mut self, params: BoostParams) {
        self.agc.set_params(params.agc);
        self.limiter.set_params(params.limiter);
        self.params = params;
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.agc.set_sample_rate(sample_rate);
        self.limiter.set_sample_rate(sample_rate);
    }

    pub fn reset(&mut self) {
        self.agc.reset();
        self.limiter.reset();
        self.meter = Meter {
            enabled: self.params.enabled,
            ..Meter::default()
        };
    }

    /// Process interleaved frames in place.
    pub fn process_interleaved(&mut self, frames: &mut [f32]) {
        let channels = self.channels;

        if !self.params.enabled {
            let level = db::rms_db(frames);
            self.meter = Meter {
                input_db: level,
                output_db: level,
                auto_gain_db: 0.0,
                manual_gain_db: 0.0,
                reduction_db: 0.0,
                gated: false,
                enabled: false,
            };
            return;
        }

        self.meter.enabled = true;
        self.meter.manual_gain_db = self.params.manual_gain_db;

        let total_frames = frames.len() / channels;
        let control_block = self.params.agc.control_block.max(1);
        let mut frame = 0;

        while frame < total_frames {
            let window = (total_frames - frame).min(control_block);
            let start = frame * channels;
            let end = start + window * channels;
            let chunk = &mut frames[start..end];

            let status = self.update_gain(chunk, window);
            self.meter.input_db = status.level_db;
            self.meter.auto_gain_db = status.gain_db;
            self.meter.gated = status.gated;

            let gain = db::linear_from_db(status.gain_db + self.params.manual_gain_db);
            for sample in chunk.iter_mut() {
                *sample *= gain;
            }

            self.limiter.process_interleaved(chunk);
            frame += window;
        }

        self.meter.output_db = self.limiter.last_peak_db();
        self.meter.reduction_db = self.limiter.last_reduction_db();
    }

    /// Linked level detection: the loudest channel drives the gain for all.
    fn update_gain(&mut self, chunk: &[f32], window: usize) -> AgcStatus {
        let channels = self.channels;
        debug_assert_eq!(self.channel_squares.len(), channels);
        self.channel_squares.iter_mut().for_each(|v| *v = 0.0);

        for frame in 0..window {
            let base = frame * channels;
            for channel in 0..channels {
                let sample = chunk[base + channel];
                self.channel_squares[channel] += sample * sample;
            }
        }

        let inverse = 1.0 / window.max(1) as f32;
        let linked = self
            .channel_squares
            .iter()
            .fold(0.0f32, |acc, sum| acc.max(sum * inverse));

        self.agc.update(linked, window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_chain_is_a_true_bypass() {
        let params = BoostParams {
            enabled: false,
            manual_gain_db: 12.0,
            ..BoostParams::default()
        };
        let mut chain = BoostChain::new(48_000.0, 1, params);
        let original: Vec<f32> = (0..1_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut buffer = original.clone();
        chain.process_interleaved(&mut buffer);
        assert_eq!(buffer, original);
        assert!(!chain.meter().enabled);
    }

    #[test]
    fn lookahead_is_reported() {
        let chain = BoostChain::new(48_000.0, 2, BoostParams::default());
        assert_eq!(chain.latency_frames(), 240);
        assert_eq!(chain.channels(), 2);
    }
}
