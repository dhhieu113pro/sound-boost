//! Hand-written ABI for the Windows audio-engine APO interfaces.
//!
//! `windows-rs` does not generate bindings for `audioenginebaseapo.h`, so these
//! declarations were transcribed from the Windows SDK header of the same name
//! (`um\audioenginebaseapo.h`, `um\audiomediatype.h`, `um\AudioAPOTypes.h`).
//!
//! Getting a size or an offset wrong here means the audio engine reads or
//! writes the wrong memory, so the layout is pinned by the tests at the bottom
//! of this file. The transcribed sources of truth are:
//!
//! | Entity | Header location |
//! | --- | --- |
//! | `APO_CONNECTION_PROPERTY` | `AudioAPOTypes.h` |
//! | `APO_CONNECTION_DESCRIPTOR`, `APO_REG_PROPERTIES`, `APO_FLAG` | `audioenginebaseapo.h` |
//! | `IAudioProcessingObject*` method order | `audioenginebaseapo.h` |
//! | `IAudioMediaType`, `UNCOMPRESSEDAUDIOFORMAT` | `audiomediatype.h` |

#![allow(non_camel_case_types, non_snake_case, clippy::upper_case_acronyms)]

use core::ffi::c_void;

pub type HRESULT = i32;
pub type ULONG = u32;
pub type BOOL = i32;
/// Hundred-nanosecond units, as used by `IAudioProcessingObject::GetLatency`.
pub type HNSTIME = i64;

pub const S_OK: HRESULT = 0;
pub const S_FALSE: HRESULT = 1;
pub const E_NOTIMPL: HRESULT = 0x8000_4001u32 as i32;
pub const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as i32;
pub const E_POINTER: HRESULT = 0x8000_4003u32 as i32;
pub const E_FAIL: HRESULT = 0x8000_4005u32 as i32;
pub const E_UNEXPECTED: HRESULT = 0x8000_FFFFu32 as i32;
pub const E_OUTOFMEMORY: HRESULT = 0x8007_000Eu32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057u32 as i32;
pub const CLASS_E_NOAGGREGATION: HRESULT = 0x8004_0110u32 as i32;
pub const CLASS_E_CLASSNOTAVAILABLE: HRESULT = 0x8004_0111u32 as i32;

// APO-specific error codes, facility 0x87D.
pub const APOERR_ALREADY_INITIALIZED: HRESULT = 0x887D_0001u32 as i32;
pub const APOERR_NOT_INITIALIZED: HRESULT = 0x887D_0002u32 as i32;
pub const APOERR_FORMAT_NOT_SUPPORTED: HRESULT = 0x887D_0003u32 as i32;
pub const APOERR_BUFFERS_OVERLAP: HRESULT = 0x887D_0005u32 as i32;

#[inline]
pub const fn succeeded(hr: HRESULT) -> bool {
    hr >= 0
}

// ---------------------------------------------------------------------------
// GUIDs
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl GUID {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }

    pub const fn nil() -> Self {
        Self::new(0, 0, 0, [0; 8])
    }
}

impl core::fmt::Display for GUID {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1,
            self.data2,
            self.data3,
            self.data4[0],
            self.data4[1],
            self.data4[2],
            self.data4[3],
            self.data4[4],
            self.data4[5],
            self.data4[6],
            self.data4[7]
        )
    }
}

pub const IID_IUNKNOWN: GUID = GUID::new(
    0x0000_0000,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);
pub const IID_ICLASS_FACTORY: GUID = GUID::new(
    0x0000_0001,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);
pub const IID_IAUDIO_PROCESSING_OBJECT: GUID = GUID::new(
    0xFD7F_2B29,
    0x24D0,
    0x4B5C,
    [0xB1, 0x77, 0x59, 0x2C, 0x39, 0xF9, 0xCA, 0x10],
);
pub const IID_IAUDIO_PROCESSING_OBJECT_RT: GUID = GUID::new(
    0x9E1D_6A6D,
    0xDDBC,
    0x4E95,
    [0xA4, 0xC7, 0xAD, 0x64, 0xBA, 0x37, 0x84, 0x6C],
);
pub const IID_IAUDIO_PROCESSING_OBJECT_CONFIGURATION: GUID = GUID::new(
    0x0E5E_D805,
    0xABA6,
    0x49C3,
    [0x8F, 0x9A, 0x2B, 0x8C, 0x88, 0x9C, 0x4F, 0xA8],
);
pub const IID_IAUDIO_SYSTEM_EFFECTS: GUID = GUID::new(
    0x5FA0_0F27,
    0xADD6,
    0x499A,
    [0x8A, 0x9D, 0x6B, 0x98, 0x52, 0x1F, 0xA7, 0x5B],
);
pub const IID_IAUDIO_SYSTEM_EFFECTS2: GUID = GUID::new(
    0xBAFE_99D2,
    0x7436,
    0x44CE,
    [0x9E, 0x0E, 0x4D, 0x89, 0xAF, 0xBF, 0xFF, 0x56],
);
/// `IAudioSystemEffects3` (`audioengineextensionapo.h`), which the engine asks
/// for immediately after `IAudioSystemEffects2`.
///
/// It is the Windows 11 "controllable effects" interface: the system lists the
/// effects an APO provides and can switch each one on and off from Settings.
/// The reference product does **not** implement it, and neither do most
/// drivers, so the engine must cope without it -- but see the note on
/// `IID_IAUDIO_SYSTEM_EFFECTS2` in `object.rs` for why we answer it anyway.
pub const IID_IAUDIO_SYSTEM_EFFECTS3: GUID = GUID::new(
    0xC58B_31CD,
    0xFC6A,
    0x4255,
    [0xBC, 0x1F, 0xAD, 0x29, 0xBB, 0x0A, 0x4A, 0x17],
);
pub const IID_IAUDIO_MEDIA_TYPE: GUID = GUID::new(
    0x4E99_7F73,
    0xB71F,
    0x4798,
    [0x87, 0x3B, 0xED, 0x7D, 0xFC, 0xF1, 0x5B, 0x4D],
);

/// Wave format subtypes, used to tell float from integer in an extensible
/// `WAVEFORMATEX` (and reported by `UNCOMPRESSEDAUDIOFORMAT::guidFormatType`).
pub const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::new(
    0x0000_0001,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);
pub const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID::new(
    0x0000_0003,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

// ---------------------------------------------------------------------------
// COM plumbing
// ---------------------------------------------------------------------------

pub type QueryInterfaceFn =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
pub type AddRefFn = unsafe extern "system" fn(*mut c_void) -> ULONG;
pub type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> ULONG;

#[repr(C)]
pub struct UnknownVtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: AddRefFn,
    pub release: ReleaseFn,
}

/// An interface pointer is just a pointer to its vtable pointer.
#[repr(C)]
pub struct IUnknown {
    pub lp_vtbl: *const UnknownVtbl,
}

// ---------------------------------------------------------------------------
// APO structures
// ---------------------------------------------------------------------------

pub const APO_BUFFER_INVALID: u32 = 0;
pub const APO_BUFFER_VALID: u32 = 1;
pub const APO_BUFFER_SILENT: u32 = 2;

/// `APO_CONNECTION_PROPERTY`: the live buffer for one connection.
///
/// The trailing `u32Signature` only exists on newer engines. We always leave it
/// untouched so the layout is valid on both old and new builds.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ApoConnectionProperty {
    pub buffer: usize,
    pub valid_frame_count: u32,
    pub buffer_flags: u32,
    pub signature: u32,
}

impl ApoConnectionProperty {
    pub const fn new(buffer: usize, valid_frame_count: u32, buffer_flags: u32) -> Self {
        Self {
            buffer,
            valid_frame_count,
            buffer_flags,
            signature: 0,
        }
    }

    pub fn is_silent(&self) -> bool {
        self.buffer_flags == APO_BUFFER_SILENT
    }
}

pub const APO_CONNECTION_BUFFER_TYPE_ALLOCATED: u32 = 0;
pub const APO_CONNECTION_BUFFER_TYPE_EXTERNAL: u32 = 1;
pub const APO_CONNECTION_BUFFER_TYPE_DEPENDANT: u32 = 2;

/// `APO_CONNECTION_DESCRIPTOR`: the format and capacity agreed at lock time.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ApoConnectionDescriptor {
    pub kind: u32,
    pub buffer: usize,
    pub max_frame_count: u32,
    pub format: *mut IAudioMediaType,
    pub signature: u32,
}

/// `APO_FLAG`.
pub const APO_FLAG_NONE: u32 = 0;
pub const APO_FLAG_INPLACE: u32 = 0x1;
pub const APO_FLAG_SAMPLESPERFRAME_MUST_MATCH: u32 = 0x2;
pub const APO_FLAG_FRAMESPERSECOND_MUST_MATCH: u32 = 0x4;
pub const APO_FLAG_BITSPERSAMPLE_MUST_MATCH: u32 = 0x8;
pub const APO_FLAG_MIXER: u32 = 0x10;
pub const APO_FLAG_DEFAULT: u32 = APO_FLAG_SAMPLESPERFRAME_MUST_MATCH
    | APO_FLAG_FRAMESPERSECOND_MUST_MATCH
    | APO_FLAG_BITSPERSAMPLE_MUST_MATCH;

/// `APO_REG_PROPERTIES`.
///
/// Declared with a single-element interface list, exactly like the C struct;
/// callers allocate `size_of::<Self>() + (n - 1) * size_of::<GUID>()` for `n`
/// interfaces. The engine frees it with `CoTaskMemFree`.
#[repr(C)]
pub struct ApoRegProperties {
    pub clsid: GUID,
    pub flags: u32,
    pub friendly_name: [u16; 256],
    pub copyright_info: [u16; 256],
    pub major_version: u32,
    pub minor_version: u32,
    pub min_input_connections: u32,
    pub max_input_connections: u32,
    pub min_output_connections: u32,
    pub max_output_connections: u32,
    pub max_instances: u32,
    pub num_apo_interfaces: u32,
    pub iid_apo_interface_list: [GUID; 1],
}

impl ApoRegProperties {
    pub fn set_friendly_name(&mut self, name: &str) {
        write_wide(&mut self.friendly_name, name);
    }

    pub fn set_copyright(&mut self, text: &str) {
        write_wide(&mut self.copyright_info, text);
    }
}

fn write_wide(dst: &mut [u16], value: &str) {
    dst.fill(0);
    for (slot, unit) in dst.iter_mut().zip(value.encode_utf16()) {
        *slot = unit;
    }
}

/// `APOInitBaseStruct`: the header every APO init structure starts with.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ApoInitBase {
    pub cb_size: u32,
    pub clsid: GUID,
}

/// `WAVEFORMATEX`.
///
/// `mmreg.h` is wrapped in `pshpack1.h` / `poppack.h`, so this structure is
/// byte-packed: it is 18 bytes with `align(1)`, not the 20 bytes a natural
/// layout would give. Getting that wrong shifts every field after the first
/// and silently misreads the format.
#[repr(C, packed(1))]
#[derive(Clone, Copy)]
pub struct WaveFormatEx {
    pub w_format_tag: u16,
    pub n_channels: u16,
    pub n_samples_per_sec: u32,
    pub n_avg_bytes_per_sec: u32,
    pub n_block_align: u16,
    pub w_bits_per_sample: u16,
    pub cb_size: u16,
}

pub const WAVE_FORMAT_PCM: u16 = 1;
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `WAVEFORMATEXTENSIBLE` (used when `wFormatTag == WAVE_FORMAT_EXTENSIBLE`).
///
/// `Samples` is a union of a single `WORD`, so it is one field here. Byte
/// packed, like `WAVEFORMATEX`: 40 bytes with `align(1)`.
#[repr(C, packed(1))]
#[derive(Clone, Copy)]
pub struct WaveFormatExtensible {
    pub format: WaveFormatEx,
    /// `Samples.wValidBitsPerSample` / `wSamplesPerBlock` / `wReserved`.
    pub samples: u16,
    pub channel_mask: u32,
    pub sub_format: GUID,
}

/// `UNCOMPRESSEDAUDIOFORMAT`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UncompressedAudioFormat {
    pub guid_format_type: GUID,
    pub samples_per_frame: u32,
    pub bytes_per_sample_container: u32,
    pub valid_bits_per_sample: u32,
    pub frames_per_second: f32,
    pub channel_mask: u32,
}

impl UncompressedAudioFormat {
    pub const ZERO: Self = Self {
        guid_format_type: GUID::nil(),
        samples_per_frame: 0,
        bytes_per_sample_container: 0,
        valid_bits_per_sample: 0,
        frames_per_second: 0.0,
        channel_mask: 0,
    };
}

// ---------------------------------------------------------------------------
// IAudioMediaType
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct IAudioMediaTypeVtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: AddRefFn,
    pub release: ReleaseFn,
    pub is_compressed_format: unsafe extern "system" fn(*mut c_void, *mut BOOL) -> HRESULT,
    pub is_equal: unsafe extern "system" fn(*mut c_void, *mut IAudioMediaType, *mut u32) -> HRESULT,
    pub get_audio_format: unsafe extern "system" fn(*mut c_void) -> *const WaveFormatEx,
    pub get_uncompressed_audio_format:
        unsafe extern "system" fn(*mut c_void, *mut UncompressedAudioFormat) -> HRESULT,
}

#[repr(C)]
pub struct IAudioMediaType {
    pub lp_vtbl: *const IAudioMediaTypeVtbl,
}

// ---------------------------------------------------------------------------
// APO interfaces
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct IAudioProcessingObjectVtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: AddRefFn,
    pub release: ReleaseFn,
    pub reset: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub get_latency: unsafe extern "system" fn(*mut c_void, *mut HNSTIME) -> HRESULT,
    pub get_registration_properties:
        unsafe extern "system" fn(*mut c_void, *mut *mut ApoRegProperties) -> HRESULT,
    pub initialize: unsafe extern "system" fn(*mut c_void, u32, *mut u8) -> HRESULT,
    pub is_input_format_supported: unsafe extern "system" fn(
        *mut c_void,
        *mut IAudioMediaType,
        *mut IAudioMediaType,
        *mut *mut IAudioMediaType,
    ) -> HRESULT,
    pub is_output_format_supported: unsafe extern "system" fn(
        *mut c_void,
        *mut IAudioMediaType,
        *mut IAudioMediaType,
        *mut *mut IAudioMediaType,
    ) -> HRESULT,
    pub get_input_channel_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
}

#[repr(C)]
pub struct IAudioProcessingObject {
    pub lp_vtbl: *const IAudioProcessingObjectVtbl,
}

#[repr(C)]
pub struct IAudioProcessingObjectConfigurationVtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: AddRefFn,
    pub release: ReleaseFn,
    pub lock_for_process: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut *mut ApoConnectionDescriptor,
        u32,
        *mut *mut ApoConnectionDescriptor,
    ) -> HRESULT,
    pub unlock_for_process: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

#[repr(C)]
pub struct IAudioProcessingObjectConfiguration {
    pub lp_vtbl: *const IAudioProcessingObjectConfigurationVtbl,
}

#[repr(C)]
pub struct IAudioProcessingObjectRTVtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: AddRefFn,
    pub release: ReleaseFn,
    pub apo_process: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut *mut ApoConnectionProperty,
        u32,
        *mut *mut ApoConnectionProperty,
    ),
    pub calc_input_frames: unsafe extern "system" fn(*mut c_void, u32) -> u32,
    pub calc_output_frames: unsafe extern "system" fn(*mut c_void, u32) -> u32,
}

#[repr(C)]
pub struct IAudioProcessingObjectRT {
    pub lp_vtbl: *const IAudioProcessingObjectRTVtbl,
}

#[repr(C)]
pub struct IAudioSystemEffects2Vtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: AddRefFn,
    pub release: ReleaseFn,
    pub get_effects_list:
        unsafe extern "system" fn(*mut c_void, *mut *mut GUID, *mut u32, *mut c_void) -> HRESULT,
}

#[repr(C)]
pub struct IAudioSystemEffects2 {
    pub lp_vtbl: *const IAudioSystemEffects2Vtbl,
}

/// `AUDIO_SYSTEMEFFECT_STATE`.
pub const AUDIO_SYSTEMEFFECT_STATE_OFF: i32 = 0;
pub const AUDIO_SYSTEMEFFECT_STATE_ON: i32 = 1;

/// `AUDIO_SYSTEMEFFECT`: one switchable effect inside an APO.
///
/// ```text
/// typedef struct AUDIO_SYSTEMEFFECT {
///     GUID id;
///     BOOL canSetState;
///     AUDIO_SYSTEMEFFECT_STATE state;
/// } AUDIO_SYSTEMEFFECT;
/// ```
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioSystemEffect {
    pub id: GUID,
    pub can_set_state: BOOL,
    pub state: i32,
}

#[repr(C)]
pub struct IAudioSystemEffects3Vtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: AddRefFn,
    pub release: ReleaseFn,
    pub get_effects_list:
        unsafe extern "system" fn(*mut c_void, *mut *mut GUID, *mut u32, *mut c_void) -> HRESULT,
    pub get_controllable_system_effects_list: unsafe extern "system" fn(
        *mut c_void,
        *mut *mut AudioSystemEffect,
        *mut u32,
        *mut c_void,
    ) -> HRESULT,
    pub set_audio_system_effect_state:
        unsafe extern "system" fn(*mut c_void, GUID, i32) -> HRESULT,
}

#[repr(C)]
pub struct IAudioSystemEffects3 {
    pub lp_vtbl: *const IAudioSystemEffects3Vtbl,
}

#[repr(C)]
pub struct IClassFactoryVtbl {
    pub query_interface: QueryInterfaceFn,
    pub add_ref: AddRefFn,
    pub release: ReleaseFn,
    pub create_instance: unsafe extern "system" fn(
        *mut c_void,
        *mut IUnknown,
        *const GUID,
        *mut *mut c_void,
    ) -> HRESULT,
    pub lock_server: unsafe extern "system" fn(*mut c_void, BOOL) -> HRESULT,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn guids_are_sixteen_bytes() {
        assert_eq!(size_of::<GUID>(), 16);
        assert_eq!(align_of::<GUID>(), 4);
    }

    #[test]
    fn connection_property_layout_matches_the_header() {
        // UINT_PTR pBuffer; UINT32 u32ValidFrameCount; APO_BUFFER_FLAGS u32BufferFlags; UINT32 u32Signature;
        assert_eq!(offset_of!(ApoConnectionProperty, buffer), 0);
        assert_eq!(offset_of!(ApoConnectionProperty, valid_frame_count), 8);
        assert_eq!(offset_of!(ApoConnectionProperty, buffer_flags), 12);
        assert_eq!(offset_of!(ApoConnectionProperty, signature), 16);
        assert_eq!(size_of::<ApoConnectionProperty>(), 24);
    }

    #[test]
    fn connection_descriptor_layout_matches_the_header() {
        // APO_CONNECTION_BUFFER_TYPE Type; UINT_PTR pBuffer; UINT32 u32MaxFrameCount;
        // IAudioMediaType *pFormat; UINT32 u32Signature;
        assert_eq!(offset_of!(ApoConnectionDescriptor, kind), 0);
        assert_eq!(offset_of!(ApoConnectionDescriptor, buffer), 8);
        assert_eq!(offset_of!(ApoConnectionDescriptor, max_frame_count), 16);
        assert_eq!(offset_of!(ApoConnectionDescriptor, format), 24);
        assert_eq!(offset_of!(ApoConnectionDescriptor, signature), 32);
        assert_eq!(size_of::<ApoConnectionDescriptor>(), 40);
    }

    #[test]
    fn registration_properties_layout_matches_the_header() {
        assert_eq!(offset_of!(ApoRegProperties, clsid), 0);
        assert_eq!(offset_of!(ApoRegProperties, flags), 16);
        assert_eq!(offset_of!(ApoRegProperties, friendly_name), 20);
        assert_eq!(offset_of!(ApoRegProperties, copyright_info), 532);
        assert_eq!(offset_of!(ApoRegProperties, major_version), 1044);
        assert_eq!(offset_of!(ApoRegProperties, max_instances), 1068);
        assert_eq!(offset_of!(ApoRegProperties, num_apo_interfaces), 1072);
        assert_eq!(offset_of!(ApoRegProperties, iid_apo_interface_list), 1076);
        assert_eq!(size_of::<ApoRegProperties>(), 1092);
    }

    #[test]
    fn audio_format_structs_match_the_header() {
        // mmreg.h is byte packed, which is why WAVEFORMATEX is 18 and not 20.
        assert_eq!(size_of::<WaveFormatEx>(), 18);
        assert_eq!(align_of::<WaveFormatEx>(), 1);
        assert_eq!(offset_of!(WaveFormatExtensible, samples), 18);
        assert_eq!(offset_of!(WaveFormatExtensible, channel_mask), 20);
        assert_eq!(offset_of!(WaveFormatExtensible, sub_format), 24);
        assert_eq!(size_of::<WaveFormatExtensible>(), 40);
        assert_eq!(align_of::<WaveFormatExtensible>(), 1);
        assert_eq!(size_of::<UncompressedAudioFormat>(), 36);
        assert_eq!(offset_of!(UncompressedAudioFormat, frames_per_second), 28);
    }

    #[test]
    fn init_struct_header_layout_matches_the_header() {
        assert_eq!(offset_of!(ApoInitBase, cb_size), 0);
        assert_eq!(offset_of!(ApoInitBase, clsid), 4);
        assert_eq!(size_of::<ApoInitBase>(), 20);
    }

    #[test]
    fn interface_pointers_are_vtable_pointers() {
        assert_eq!(size_of::<IAudioProcessingObject>(), 8);
        assert_eq!(size_of::<IAudioProcessingObjectConfiguration>(), 8);
        assert_eq!(size_of::<IAudioProcessingObjectRT>(), 8);
        assert_eq!(size_of::<IAudioSystemEffects2>(), 8);
        assert_eq!(size_of::<IAudioSystemEffects3>(), 8);
        assert_eq!(size_of::<IAudioMediaType>(), 8);
    }

    #[test]
    fn system_effect_layout_matches_the_header() {
        // GUID id; BOOL canSetState; AUDIO_SYSTEMEFFECT_STATE state;
        assert_eq!(offset_of!(AudioSystemEffect, id), 0);
        assert_eq!(offset_of!(AudioSystemEffect, can_set_state), 16);
        assert_eq!(offset_of!(AudioSystemEffect, state), 20);
        assert_eq!(size_of::<AudioSystemEffect>(), 24);
    }

    #[test]
    fn system_effects3_ids_match_the_header() {
        assert_eq!(
            IID_IAUDIO_SYSTEM_EFFECTS3.to_string(),
            "{C58B31CD-FC6A-4255-BC1F-AD29BB0A4A17}"
        );
        assert_eq!(AUDIO_SYSTEMEFFECT_STATE_OFF, 0);
        assert_eq!(AUDIO_SYSTEMEFFECT_STATE_ON, 1);
    }

    #[test]
    fn guid_display_is_windows_order() {
        assert_eq!(
            IID_IAUDIO_PROCESSING_OBJECT.to_string(),
            "{FD7F2B29-24D0-4B5C-B177-592C39F9CA10}"
        );
    }
}
