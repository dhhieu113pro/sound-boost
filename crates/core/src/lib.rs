//! Real-time DSP used by the Sound Booster APO.
//!
//! The chain, evaluated once per control window of frames:
//!
//! ```text
//! input -> [level detector] -> [noise gate] -> [auto gain / AGC]
//!       -> [manual boost] -> [look-ahead limiter] -> output
//! ```
//!
//! Design rules that matter inside `audiodg.exe`:
//!
//! * No allocation while processing. Every buffer is sized in the
//!   constructors; `set_params` only touches buffers when the look-ahead
//!   length itself changes (a control-path event, not an audio-path one).
//! * No locks, no COM calls, no registry access in the processing path.
//! * Gains are computed in dB and smoothed in dB, which is what keeps the
//!   leveler from pumping.
//!
//! The manual boost is applied *after* the level detector, so moving the
//! slider never changes how strongly the auto leveler works.

pub mod agc;
pub mod chain;
pub mod db;
pub mod envelope;
pub mod limiter;
pub mod params;

pub use agc::{Agc, AgcParams, AgcStatus};
pub use chain::{BoostChain, BoostParams, Meter};
pub use limiter::{Limiter, LimiterParams};
pub use params::Preset;
