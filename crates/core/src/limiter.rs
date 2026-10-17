//! Look-ahead brick-wall limiter.
//!
//! A fixed boost of 300-500 % clips as soon as the source is anywhere near
//! full scale, which is the crackle people hear from simpler boosters. This
//! stage looks `lookahead_ms` into the future, computes the exact gain needed
//! so the loudest sample in that window lands on the ceiling, and only then
//! releases that gain slowly. Because the gain is always derived from a
//! window that contains the sample being written, the output cannot overshoot
//! the ceiling by construction.

use std::collections::VecDeque;

use crate::db;

/// Longest look-ahead we will ever need; buffers are sized for this once.
pub const MAX_LOOKAHEAD_MS: f32 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterParams {
    /// Output ceiling, dBFS. `-0.3` leaves headroom for lossy codecs.
    pub ceiling_db: f32,
    /// Look-ahead in milliseconds. More = cleaner, adds latency.
    pub lookahead_ms: f32,
    /// How fast the gain returns after a peak.
    pub release_ms: f32,
}

impl Default for LimiterParams {
    fn default() -> Self {
        Self {
            ceiling_db: -0.3,
            lookahead_ms: 5.0,
            release_ms: 100.0,
        }
    }
}

pub struct Limiter {
    sample_rate: f32,
    channels: usize,
    params: LimiterParams,
    ceiling: f32,
    /// Active look-ahead, in frames.
    lookahead: usize,
    /// Delay line sized for [`MAX_LOOKAHEAD_MS`] so it never reallocates.
    delay: Vec<f32>,
    head: usize,
    /// Monotonic sliding maximum of frame peaks over the look-ahead window.
    window: VecDeque<(u64, f32)>,
    frame_index: u64,
    gain: f32,
    release_coeff: f32,
    last_peak: f32,
    last_min_gain: f32,
}

impl Limiter {
    pub fn new(sample_rate: f32, channels: usize, params: LimiterParams) -> Self {
        let channels = channels.max(1);
        let max_lookahead = max_lookahead_frames(sample_rate);
        let mut limiter = Self {
            sample_rate,
            channels,
            params,
            ceiling: db::linear_from_db(params.ceiling_db),
            lookahead: 0,
            delay: vec![0.0; max_lookahead.max(1) * channels],
            head: 0,
            window: VecDeque::with_capacity(max_lookahead + 2),
            frame_index: 0,
            gain: 1.0,
            release_coeff: 1.0,
            last_peak: 0.0,
            last_min_gain: 1.0,
        };
        limiter.relook();
        limiter.rebuild();
        limiter
    }

    /// Recompute the sample-rate and parameter dependent values.
    ///
    /// The look-ahead is deliberately *not* recomputed here. The engine asks
    /// for this stage's latency once, after locking, and keeps that number for
    /// the life of the stream -- an APO that then changed its delay would be
    /// reporting a latency that is not true. So the look-ahead is fixed at the
    /// value in force when the instance was built, and a preset that wants a
    /// different one takes effect on the next stream.
    fn rebuild(&mut self) {
        self.ceiling = db::linear_from_db(self.params.ceiling_db);
        self.release_coeff =
            crate::envelope::one_pole_coeff(self.sample_rate, self.params.release_ms);
    }

    /// Recompute the look-ahead and start the delay line over.
    fn relook(&mut self) {
        let wanted = ((self.params.lookahead_ms * 0.001 * self.sample_rate).round() as usize)
            .min(max_lookahead_frames(self.sample_rate));
        if wanted != self.lookahead {
            self.lookahead = wanted;
            self.clear_delay();
        }
    }

    fn clear_delay(&mut self) {
        self.delay.iter_mut().for_each(|s| *s = 0.0);
        self.window.clear();
        self.head = 0;
        self.frame_index = 0;
        self.gain = 1.0;
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.relook();
        self.rebuild();
    }

    pub fn set_params(&mut self, params: LimiterParams) {
        self.params = params;
        // A look-ahead change waits for the next instance; see `rebuild`.
        self.rebuild();
    }

    pub fn params(&self) -> LimiterParams {
        self.params
    }

    pub fn reset(&mut self) {
        self.clear_delay();
        self.last_peak = 0.0;
        self.last_min_gain = 1.0;
    }

    /// Latency this stage introduces, in frames.
    pub fn latency_frames(&self) -> usize {
        self.lookahead
    }

    /// Peak of the last processed block, post-limiter.
    pub fn last_peak_db(&self) -> f32 {
        db::db_from_linear(self.last_peak)
    }

    /// Amount of gain reduction applied in the last block, as a negative or
    /// zero dB value.
    pub fn last_reduction_db(&self) -> f32 {
        db::db_from_linear(self.last_min_gain).min(0.0)
    }

    /// Process interleaved frames in place.
    pub fn process_interleaved(&mut self, frames: &mut [f32]) {
        let channels = self.channels;
        let frame_count = frames.len() / channels;
        self.last_min_gain = 1.0;
        self.last_peak = 0.0;

        for frame in 0..frame_count {
            let base = frame * channels;

            let mut frame_peak = 0.0f32;
            for c in 0..channels {
                let magnitude = frames[base + c].abs();
                if magnitude > frame_peak {
                    frame_peak = magnitude;
                }
            }

            // Sliding maximum over [frame_index - lookahead, frame_index].
            while let Some(&(_, value)) = self.window.back() {
                if value <= frame_peak {
                    self.window.pop_back();
                } else {
                    break;
                }
            }
            self.window.push_back((self.frame_index, frame_peak));
            let stale_before = self.frame_index.saturating_sub(self.lookahead as u64);
            while let Some(&(index, _)) = self.window.front() {
                if index < stale_before {
                    self.window.pop_front();
                } else {
                    break;
                }
            }

            let window_max = self.window.front().map(|&(_, v)| v).unwrap_or(frame_peak);
            let desired = if window_max > self.ceiling {
                self.ceiling / window_max
            } else {
                1.0
            };

            if desired < self.gain {
                self.gain = desired; // instant attack
            } else {
                self.gain += (desired - self.gain) * self.release_coeff;
            }
            let gain = self.gain.min(desired);

            if self.lookahead > 0 {
                let delayed_base = self.head * channels;
                for c in 0..channels {
                    let delayed = self.delay[delayed_base + c];
                    self.delay[delayed_base + c] = frames[base + c];
                    frames[base + c] = delayed * gain;
                }
                self.head += 1;
                if self.head == self.lookahead {
                    self.head = 0;
                }
            } else {
                for c in 0..channels {
                    frames[base + c] *= gain;
                }
            }

            let out_peak = frames[base..base + channels]
                .iter()
                .fold(0.0f32, |acc, s| acc.max(s.abs()));
            if out_peak > self.last_peak {
                self.last_peak = out_peak;
            }
            if gain < self.last_min_gain {
                self.last_min_gain = gain;
            }
            self.frame_index += 1;
        }
    }
}

fn max_lookahead_frames(sample_rate: f32) -> usize {
    ((MAX_LOOKAHEAD_MS * 0.001 * sample_rate).ceil() as usize).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amplitude: f32, frames: usize, fs: f32, freq: f32) -> Vec<f32> {
        (0..frames)
            .map(|i| amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / fs).sin())
            .collect()
    }

    #[test]
    fn looks_ahead_by_the_configured_amount() {
        let params = LimiterParams {
            lookahead_ms: 5.0,
            ..LimiterParams::default()
        };
        let limiter = Limiter::new(48_000.0, 1, params);
        assert_eq!(limiter.latency_frames(), 240);
    }

    #[test]
    fn output_never_exceeds_the_ceiling() {
        let params = LimiterParams {
            ceiling_db: -1.0,
            lookahead_ms: 5.0,
            release_ms: 100.0,
        };
        let ceiling = db::linear_from_db(-1.0);
        let mut limiter = Limiter::new(48_000.0, 2, params);
        // Way over full scale, so limiting is definitely active.
        let mut buffer: Vec<f32> = sine(4.0, 48_000, 48_000.0, 997.0)
            .into_iter()
            .flat_map(|s| [s, s * 0.7])
            .collect();
        limiter.process_interleaved(&mut buffer);
        let peak = buffer.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            peak <= ceiling + 1e-4,
            "peak {peak} exceeded ceiling {ceiling}"
        );
    }

    #[test]
    fn short_impulse_is_caught_before_it_leaves() {
        let params = LimiterParams {
            ceiling_db: -3.0,
            lookahead_ms: 5.0,
            release_ms: 50.0,
        };
        let ceiling = db::linear_from_db(-3.0);
        let mut buffer = vec![0.0f32; 4_800];
        buffer[2_000] = 1.0;
        let mut limiter = Limiter::new(48_000.0, 1, params);
        limiter.process_interleaved(&mut buffer);
        let peak = buffer.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak <= ceiling + 1e-4, "peak {peak}");
    }

    #[test]
    fn quiet_signal_passes_through_untouched() {
        let mut limiter = Limiter::new(48_000.0, 1, LimiterParams::default());
        let signal = sine(0.1, 4_800, 48_000.0, 440.0);
        let mut buffer = signal.clone();
        limiter.process_interleaved(&mut buffer);
        let lookahead = limiter.latency_frames();
        for i in lookahead..buffer.len() {
            assert!((buffer[i] - signal[i - lookahead]).abs() < 1e-5);
        }
    }

    #[test]
    fn silence_stays_silent() {
        let mut limiter = Limiter::new(48_000.0, 2, LimiterParams::default());
        let mut buffer = vec![0.0f32; 2 * 4_800];
        limiter.process_interleaved(&mut buffer);
        assert!(buffer.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_mid_stream_lookahead_change_waits_for_the_next_instance() {
        let mut limiter = Limiter::new(48_000.0, 1, LimiterParams::default());
        assert_eq!(limiter.latency_frames(), 240);

        limiter.set_params(LimiterParams {
            lookahead_ms: 20.0,
            ..LimiterParams::default()
        });
        // The number the engine was told must not move under it.
        assert_eq!(limiter.latency_frames(), 240);

        // A fresh instance picks the new one up.
        let fresh = Limiter::new(
            48_000.0,
            1,
            LimiterParams {
                lookahead_ms: 20.0,
                ..LimiterParams::default()
            },
        );
        assert_eq!(fresh.latency_frames(), 960);
    }

    #[test]
    fn a_new_ceiling_applies_to_the_very_next_buffer() {
        let mut limiter = Limiter::new(48_000.0, 1, LimiterParams::default());
        limiter.set_params(LimiterParams {
            ceiling_db: -20.0,
            ..LimiterParams::default()
        });
        let mut buffer = vec![1.0f32; 4_800];
        limiter.process_interleaved(&mut buffer);
        let peak = buffer.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak <= db::linear_from_db(-20.0) + 1e-3, "peak {peak}");
    }
}
