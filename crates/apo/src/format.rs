//! Sample-format negotiation and conversion.
//!
//! The engine can hand an APO float32 (the usual shared-mode mix format) or
//! integer PCM. The DSP works in `f32`, so integer buffers get converted on the
//! way in and out using a preallocated scratch buffer -- never an allocation on
//! the audio thread.

use core::ffi::c_void;
use core::ptr;

use crate::abi::{
    IAudioMediaType, UncompressedAudioFormat, WaveFormatEx, WaveFormatExtensible,
    KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE,
    WAVE_FORMAT_IEEE_FLOAT, WAVE_FORMAT_PCM,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleFormat {
    Float32,
    Pcm16,
    Pcm32,
}

impl SampleFormat {
    pub const fn container_bytes(self) -> usize {
        match self {
            SampleFormat::Float32 => 4,
            SampleFormat::Pcm16 => 2,
            SampleFormat::Pcm32 => 4,
        }
    }
}

/// The format an APO instance is locked to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl StreamFormat {
    pub fn block_align(&self) -> usize {
        self.channels as usize * self.sample_format.container_bytes()
    }

    /// Interpret the `UNCOMPRESSEDAUDIOFORMAT` the engine reported.
    pub fn from_uncompressed(raw: &UncompressedAudioFormat) -> Option<Self> {
        if raw.samples_per_frame == 0 || raw.frames_per_second <= 0.0 {
            return None;
        }
        let sample_format = if raw.guid_format_type == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            match raw.bytes_per_sample_container {
                4 => SampleFormat::Float32,
                _ => return None,
            }
        } else if raw.guid_format_type == KSDATAFORMAT_SUBTYPE_PCM {
            match (raw.bytes_per_sample_container, raw.valid_bits_per_sample) {
                (2, 16) => SampleFormat::Pcm16,
                (4, 32) => SampleFormat::Pcm32,
                _ => return None,
            }
        } else {
            return None;
        };

        Some(Self {
            sample_rate: raw.frames_per_second.round() as u32,
            channels: raw.samples_per_frame as u16,
            sample_format,
        })
    }

    /// Fallback path for engines that only expose a `WAVEFORMATEX`.
    ///
    /// # Safety
    /// `format` must point to a valid `WAVEFORMATEX`; when the tag is
    /// `WAVE_FORMAT_EXTENSIBLE` it must actually be a `WAVEFORMATEXTENSIBLE`.
    pub unsafe fn from_wave_format(format: *const WaveFormatEx) -> Option<Self> {
        if format.is_null() {
            return None;
        }
        let wave = *format;
        if wave.n_channels == 0 || wave.n_samples_per_sec == 0 {
            return None;
        }

        let sample_format = match wave.w_format_tag {
            WAVE_FORMAT_IEEE_FLOAT => SampleFormat::Float32,
            WAVE_FORMAT_PCM => match wave.w_bits_per_sample {
                16 => SampleFormat::Pcm16,
                32 => SampleFormat::Pcm32,
                _ => return None,
            },
            WAVE_FORMAT_EXTENSIBLE => {
                if wave.cb_size < 22 {
                    return None;
                }
                // The SDK header is byte packed and the engine's pointer carries
                // no alignment guarantee, so copy the structure out rather than
                // referencing it.
                let extensible = core::ptr::read_unaligned(format as *const WaveFormatExtensible);
                let container = (wave.w_bits_per_sample / 8) as usize;
                let valid_bits = if extensible.samples == 0 {
                    wave.w_bits_per_sample as u32
                } else {
                    extensible.samples as u32
                };
                // `sub_format` has to be copied out: taking a reference to a
                // field of a byte-packed struct is undefined behaviour.
                let sub_format = extensible.sub_format;
                if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    if container != 4 {
                        return None;
                    }
                    SampleFormat::Float32
                } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                    match (container, valid_bits) {
                        (2, 16) => SampleFormat::Pcm16,
                        (4, 32) => SampleFormat::Pcm32,
                        _ => return None,
                    }
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        Some(Self {
            sample_rate: wave.n_samples_per_sec,
            channels: wave.n_channels,
            sample_format,
        })
    }
}

/// Take a reference on an `IAudioMediaType`.
///
/// Used when handing a media type back to the engine: the caller owns the
/// reference it receives, exactly as `CBaseAudioProcessingObject` arranges it.
///
/// # Safety
/// `media_type` must be null or a live `IAudioMediaType` from the audio engine.
pub unsafe fn add_ref(media_type: *mut IAudioMediaType) {
    if media_type.is_null() {
        return;
    }
    let vtbl = (*media_type).lp_vtbl;
    if vtbl.is_null() {
        return;
    }
    ((*vtbl).add_ref)(media_type as *mut c_void);
}

/// Ask an `IAudioMediaType` what it actually contains.
///
/// # Safety
/// `media_type` must be null or a live `IAudioMediaType` from the audio engine.
pub unsafe fn read_stream_format(media_type: *mut IAudioMediaType) -> Option<StreamFormat> {
    if media_type.is_null() {
        return None;
    }
    let vtbl = (*media_type).lp_vtbl;
    if vtbl.is_null() {
        return None;
    }

    // Preferred: the uncompressed description, which needs no parsing.
    let mut raw = UncompressedAudioFormat::ZERO;
    let hr = ((*vtbl).get_uncompressed_audio_format)(media_type as *mut c_void, &mut raw);
    if crate::abi::succeeded(hr) {
        if let Some(format) = StreamFormat::from_uncompressed(&raw) {
            return Some(format);
        }
    }

    // Fallback: the raw WAVEFORMATEX.
    let wave = ((*vtbl).get_audio_format)(media_type as *mut c_void);
    StreamFormat::from_wave_format(wave)
}

/// Copy `samples` interleaved samples from `src` into `dst` as `f32`.
///
/// # Safety
/// Both pointers must be valid for `samples` samples of the format's container
/// size and must not overlap.
pub unsafe fn decode_into(format: &StreamFormat, src: *const u8, dst: *mut f32, samples: usize) {
    match format.sample_format {
        SampleFormat::Float32 => {
            ptr::copy_nonoverlapping(src as *const f32, dst, samples);
        }
        SampleFormat::Pcm16 => {
            let input = src as *const i16;
            for index in 0..samples {
                *dst.add(index) = *input.add(index) as f32 / 32_768.0;
            }
        }
        SampleFormat::Pcm32 => {
            let input = src as *const i32;
            for index in 0..samples {
                *dst.add(index) = *input.add(index) as f32 / 2_147_483_648.0;
            }
        }
    }
}

/// Inverse of [`decode_into`], clamping to full scale.
///
/// # Safety
/// Both pointers must be valid for `samples` samples of the format's container
/// size and must not overlap.
pub unsafe fn encode_from(format: &StreamFormat, src: *const f32, dst: *mut u8, samples: usize) {
    match format.sample_format {
        SampleFormat::Float32 => {
            ptr::copy_nonoverlapping(src, dst as *mut f32, samples);
        }
        SampleFormat::Pcm16 => {
            let output = dst as *mut i16;
            for index in 0..samples {
                let value = (*src.add(index)).clamp(-1.0, 1.0);
                *output.add(index) = (value * 32_767.0).round() as i16;
            }
        }
        SampleFormat::Pcm32 => {
            let output = dst as *mut i32;
            for index in 0..samples {
                let value = (*src.add(index)).clamp(-1.0, 1.0);
                *output.add(index) = (value * 2_147_483_647.0) as i32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uncompressed(
        subtype: crate::abi::GUID,
        container: u32,
        valid: u32,
    ) -> UncompressedAudioFormat {
        UncompressedAudioFormat {
            guid_format_type: subtype,
            samples_per_frame: 2,
            bytes_per_sample_container: container,
            valid_bits_per_sample: valid,
            frames_per_second: 48_000.0,
            channel_mask: 3,
        }
    }

    #[test]
    fn float32_is_recognised() {
        let raw = uncompressed(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, 4, 32);
        let format = StreamFormat::from_uncompressed(&raw).expect("float32");
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.channels, 2);
        assert_eq!(format.sample_format, SampleFormat::Float32);
        assert_eq!(format.block_align(), 8);
    }

    #[test]
    fn pcm_containers_are_recognised_and_others_rejected() {
        let pcm16 = uncompressed(KSDATAFORMAT_SUBTYPE_PCM, 2, 16);
        assert_eq!(
            StreamFormat::from_uncompressed(&pcm16)
                .unwrap()
                .sample_format,
            SampleFormat::Pcm16
        );

        let pcm32 = uncompressed(KSDATAFORMAT_SUBTYPE_PCM, 4, 32);
        assert_eq!(
            StreamFormat::from_uncompressed(&pcm32)
                .unwrap()
                .sample_format,
            SampleFormat::Pcm32
        );

        // 24-bit in a 4 byte container, and 8-bit, are not supported.
        assert!(
            StreamFormat::from_uncompressed(&uncompressed(KSDATAFORMAT_SUBTYPE_PCM, 4, 24))
                .is_none()
        );
        assert!(
            StreamFormat::from_uncompressed(&uncompressed(KSDATAFORMAT_SUBTYPE_PCM, 1, 8))
                .is_none()
        );
        assert!(
            StreamFormat::from_uncompressed(&uncompressed(crate::abi::GUID::nil(), 4, 32))
                .is_none()
        );
    }

    #[test]
    fn extensible_wave_format_is_parsed() {
        let mut wave = WaveFormatExtensible {
            format: WaveFormatEx {
                w_format_tag: WAVE_FORMAT_EXTENSIBLE,
                n_channels: 2,
                n_samples_per_sec: 44_100,
                n_avg_bytes_per_sec: 176_400,
                n_block_align: 4,
                w_bits_per_sample: 16,
                cb_size: 22,
            },
            samples: 16,
            channel_mask: 3,
            sub_format: KSDATAFORMAT_SUBTYPE_PCM,
        };
        let format =
            unsafe { StreamFormat::from_wave_format(&wave.format) }.expect("extensible pcm16");
        assert_eq!(format.sample_rate, 44_100);
        assert_eq!(format.sample_format, SampleFormat::Pcm16);

        wave.sub_format = KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        wave.format.w_bits_per_sample = 32;
        wave.samples = 32;
        let format =
            unsafe { StreamFormat::from_wave_format(&wave.format) }.expect("extensible float32");
        assert_eq!(format.sample_format, SampleFormat::Float32);
    }

    #[test]
    fn pcm_round_trip_is_stable() {
        let format = StreamFormat {
            sample_rate: 48_000,
            channels: 1,
            sample_format: SampleFormat::Pcm16,
        };
        let input: Vec<i16> = (0..256)
            .map(|i| ((i as f32 / 256.0) * 60_000.0) as i16)
            .collect();
        let mut floats = vec![0.0f32; input.len()];
        unsafe {
            decode_into(
                &format,
                input.as_ptr() as *const u8,
                floats.as_mut_ptr(),
                input.len(),
            )
        };
        let mut output = vec![0i16; input.len()];
        unsafe {
            encode_from(
                &format,
                floats.as_ptr(),
                output.as_mut_ptr() as *mut u8,
                input.len(),
            )
        };
        for (expected, actual) in input.iter().zip(output.iter()) {
            assert!(
                (*expected as i32 - *actual as i32).abs() <= 2,
                "{expected} vs {actual}"
            );
        }
    }

    #[test]
    fn full_scale_does_not_wrap_around() {
        let format = StreamFormat {
            sample_rate: 48_000,
            channels: 1,
            sample_format: SampleFormat::Pcm16,
        };
        let floats = [2.0f32, -2.0, 1.0, -1.0];
        let mut output = vec![0i16; floats.len()];
        unsafe {
            encode_from(
                &format,
                floats.as_ptr(),
                output.as_mut_ptr() as *mut u8,
                floats.len(),
            )
        };
        assert_eq!(output, vec![32767, -32767, 32767, -32767]);
    }
}
