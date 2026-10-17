//! Drives the APO through its real COM vtables, the way `audiodg.exe` would.
//!
//! This is as close as we can get to "does the audio engine accept this DLL"
//! without registering it against a live endpoint and restarting the audio
//! service, which is what `crates/setup` will do and what `docs/risks.md`
//! lists as the first thing to prove on real hardware.

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use sb_apo::abi::*;
use sb_apo::object::{
    add_ref, query_interface, release, ApoRole, BoostApo, CLSID_ENDPOINT, CLSID_STREAM,
};
use sb_core::params::Preset;
use sb_ipc::mapping::MappedControl;
use sb_ipc::ControlSnapshot;
use windows_sys::Win32::System::Com::CoTaskMemFree;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const FRAMES: usize = 1024;

/// The shared control section is process-global and the APO reads it on every
/// buffer, so tests that change it must not run at the same time.
static CONTROL: Mutex<()> = Mutex::new(());

/// Take the control-section lock, tolerating a panic in another test: a
/// poisoned mutex would otherwise turn one real failure into a wall of them.
fn control_lock() -> std::sync::MutexGuard<'static, ()> {
    CONTROL.lock().unwrap_or_else(|error| error.into_inner())
}

// ---------------------------------------------------------------------------
// A stand-in for the engine's IAudioMediaType
// ---------------------------------------------------------------------------

#[repr(C)]
struct FakeMediaType {
    vtbl: *const IAudioMediaTypeVtbl,
    format: UncompressedAudioFormat,
}

unsafe extern "system" fn media_query_interface(
    _this: *mut c_void,
    _riid: *const GUID,
    _ppv: *mut *mut c_void,
) -> HRESULT {
    E_NOINTERFACE
}

unsafe extern "system" fn media_add_ref(_this: *mut c_void) -> ULONG {
    1
}

unsafe extern "system" fn media_release(_this: *mut c_void) -> ULONG {
    0
}

unsafe extern "system" fn media_is_compressed(
    _this: *mut c_void,
    compressed: *mut BOOL,
) -> HRESULT {
    if compressed.is_null() {
        return E_POINTER;
    }
    *compressed = 0;
    S_OK
}

unsafe extern "system" fn media_is_equal(
    _this: *mut c_void,
    _other: *mut IAudioMediaType,
    _flags: *mut u32,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn media_get_wave_format(_this: *mut c_void) -> *const WaveFormatEx {
    core::ptr::null()
}

unsafe extern "system" fn media_get_uncompressed(
    this: *mut c_void,
    format: *mut UncompressedAudioFormat,
) -> HRESULT {
    if format.is_null() {
        return E_POINTER;
    }
    *format = (*(this as *const FakeMediaType)).format;
    S_OK
}

static MEDIA_VTBL: IAudioMediaTypeVtbl = IAudioMediaTypeVtbl {
    query_interface: media_query_interface,
    add_ref: media_add_ref,
    release: media_release,
    is_compressed_format: media_is_compressed,
    is_equal: media_is_equal,
    get_audio_format: media_get_wave_format,
    get_uncompressed_audio_format: media_get_uncompressed,
};

fn float32_media_type() -> FakeMediaType {
    FakeMediaType {
        vtbl: &MEDIA_VTBL,
        format: UncompressedAudioFormat {
            guid_format_type: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            samples_per_frame: CHANNELS,
            bytes_per_sample_container: 4,
            valid_bits_per_sample: 32,
            frames_per_second: SAMPLE_RATE as f32,
            channel_mask: 3,
        },
    }
}

// ---------------------------------------------------------------------------
// Driving the object
// ---------------------------------------------------------------------------

/// An instantiated APO plus the interfaces the engine would hold.
struct Instance {
    apo: *mut c_void,
    config: *mut c_void,
    rt: *mut c_void,
    effects: *mut c_void,
    /// References we own, so `destroy` gives them all back.
    refs: u32,
}

impl Instance {
    fn role(&self) -> ApoRole {
        unsafe {
            let vtbl = *(self.apo as *mut *const IAudioProcessingObjectVtbl);
            let mut properties: *mut ApoRegProperties = null_mut();
            ((*vtbl).get_registration_properties)(self.apo, &mut properties);
            let clsid = (*properties).clsid;
            CoTaskMemFree(properties as *const c_void);
            ApoRole::from_clsid(&clsid).expect("our own CLSID")
        }
    }
}

/// `DllGetClassObject` -> `IClassFactory::CreateInstance`, then the interfaces
/// the engine asks for.
unsafe fn create(clsid: GUID) -> Instance {
    let mut factory: *mut c_void = null_mut();
    assert_eq!(
        sb_apo::DllGetClassObject(&clsid, &IID_ICLASS_FACTORY, &mut factory),
        S_OK
    );
    assert!(!factory.is_null(), "no class factory");
    let factory_vtbl = *(factory as *mut *const IClassFactoryVtbl);

    let mut apo: *mut c_void = null_mut();
    assert_eq!(
        ((*factory_vtbl).create_instance)(
            factory,
            null_mut(),
            &IID_IAUDIO_PROCESSING_OBJECT,
            &mut apo
        ),
        S_OK
    );
    assert!(!apo.is_null(), "no APO");
    ((*factory_vtbl).release)(factory);

    let vtbl = *(apo as *mut *const IAudioProcessingObjectVtbl);
    let mut config: *mut c_void = null_mut();
    let mut rt: *mut c_void = null_mut();
    let mut effects: *mut c_void = null_mut();
    assert_eq!(
        ((*vtbl).query_interface)(
            apo,
            &IID_IAUDIO_PROCESSING_OBJECT_CONFIGURATION,
            &mut config
        ),
        S_OK
    );
    assert_eq!(
        ((*vtbl).query_interface)(apo, &IID_IAUDIO_PROCESSING_OBJECT_RT, &mut rt),
        S_OK
    );
    assert_eq!(
        ((*vtbl).query_interface)(apo, &IID_IAUDIO_SYSTEM_EFFECTS2, &mut effects),
        S_OK
    );

    Instance {
        apo,
        config,
        rt,
        effects,
        // One from the QueryInterface in CreateInstance, one per
        // QueryInterface above. The object itself starts at zero.
        refs: 4,
    }
}

unsafe fn destroy(instance: &Instance) {
    let vtbl = *(instance.apo as *mut *const IAudioProcessingObjectVtbl);
    for _ in 0..instance.refs {
        ((*vtbl).release)(instance.apo);
    }
}

unsafe fn initialize(instance: &Instance, clsid: GUID) -> HRESULT {
    let vtbl = *(instance.apo as *mut *const IAudioProcessingObjectVtbl);
    let init = ApoInitBase {
        cb_size: size_of::<ApoInitBase>() as u32,
        clsid,
    };
    ((*vtbl).initialize)(
        instance.apo,
        size_of::<ApoInitBase>() as u32,
        &init as *const ApoInitBase as *mut u8,
    )
}

unsafe fn lock(instance: &Instance, media: &mut FakeMediaType, max_frames: u32) -> HRESULT {
    let vtbl = *(instance.config as *mut *const IAudioProcessingObjectConfigurationVtbl);
    let mut descriptor = ApoConnectionDescriptor {
        kind: APO_CONNECTION_BUFFER_TYPE_ALLOCATED,
        buffer: 0,
        max_frame_count: max_frames,
        format: media as *mut FakeMediaType as *mut IAudioMediaType,
        signature: 0,
    };
    let mut pointer: *mut ApoConnectionDescriptor = &mut descriptor;
    ((*vtbl).lock_for_process)(instance.config, 1, &mut pointer, 1, &mut pointer)
}

unsafe fn latency_hns(instance: &Instance) -> HNSTIME {
    let vtbl = *(instance.apo as *mut *const IAudioProcessingObjectVtbl);
    let mut value: HNSTIME = -1;
    assert_eq!(((*vtbl).get_latency)(instance.apo, &mut value), S_OK);
    value
}

/// One `APOProcess` call with one input and one output connection.
///
/// # Safety
/// Both pointers must be valid for `frames * CHANNELS` `f32` values, and may
/// be equal: `APO_FLAG_INPLACE` means the engine can hand us one buffer.
unsafe fn process_buffers(instance: &Instance, input: *const f32, output: *mut f32, frames: u32) {
    let vtbl = *(instance.rt as *mut *const IAudioProcessingObjectRTVtbl);
    let mut input_property = ApoConnectionProperty::new(input as usize, frames, APO_BUFFER_VALID);
    let mut output_property =
        ApoConnectionProperty::new(output as usize, frames, APO_BUFFER_INVALID);
    let mut inputs: *mut ApoConnectionProperty = &mut input_property;
    let mut outputs: *mut ApoConnectionProperty = &mut output_property;
    ((*vtbl).apo_process)(instance.rt, 1, &mut inputs, 1, &mut outputs);
    assert_eq!(output_property.valid_frame_count, frames);
    assert_eq!(output_property.buffer_flags, APO_BUFFER_VALID);
}

/// As [`process_buffers`], with separate buffers.
unsafe fn process(instance: &Instance, input: &mut [f32], output: &mut [f32], frames: u32) {
    process_buffers(instance, input.as_ptr(), output.as_mut_ptr(), frames);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Publish a snapshot into the shared control block.
///
/// The returned mapping must be kept alive: the section disappears once every
/// handle to it is closed, and the APO only attaches in `Initialize`.
fn publish(snapshot: ControlSnapshot) -> MappedControl {
    let mapped = MappedControl::create_or_open().expect("control section");
    mapped.block().initialize();
    mapped.block().write(&snapshot);
    mapped
}

/// An APO ready to process: initialized, control block attached, locked.
unsafe fn ready(clsid: GUID) -> (Instance, FakeMediaType) {
    let instance = create(clsid);
    assert_eq!(initialize(&instance, clsid), S_OK);
    let mut media = float32_media_type();
    assert_eq!(lock(&instance, &mut media, FRAMES as u32), S_OK);
    (instance, media)
}

fn sine(amplitude: f32, frames: usize) -> Vec<f32> {
    let mut buffer = Vec::with_capacity(frames * CHANNELS as usize);
    for frame in 0..frames {
        let phase = 2.0 * core::f32::consts::PI * 1000.0 * frame as f32 / SAMPLE_RATE as f32;
        let value = amplitude * phase.sin();
        for _ in 0..CHANNELS {
            buffer.push(value);
        }
    }
    buffer
}

fn rms(buffer: &[f32]) -> f32 {
    if buffer.is_empty() {
        return 0.0;
    }
    (buffer.iter().map(|v| v * v).sum::<f32>() / buffer.len() as f32).sqrt()
}

fn peak(buffer: &[f32]) -> f32 {
    buffer.iter().fold(0.0f32, |acc, v| acc.max(v.abs()))
}

/// The steady-state part of a buffer, past the limiter's look-ahead.
fn settled(buffer: &[f32]) -> &[f32] {
    let skip = 512 * CHANNELS as usize;
    if buffer.len() > skip {
        &buffer[skip..]
    } else {
        buffer
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn unknown_clsid_is_not_available() {
    let mut factory: *mut c_void = null_mut();
    let hr = unsafe { sb_apo::DllGetClassObject(&GUID::nil(), &IID_ICLASS_FACTORY, &mut factory) };
    assert_eq!(hr, CLASS_E_CLASSNOTAVAILABLE);
    assert!(factory.is_null());
}

#[test]
fn dll_can_unload_now_keeps_the_module_loaded() {
    assert_eq!(sb_apo::DllCanUnloadNow(), S_FALSE);
}

#[test]
fn both_clsids_are_served_by_the_factory() {
    unsafe {
        for clsid in [CLSID_STREAM, CLSID_ENDPOINT] {
            let instance = create(clsid);
            assert_eq!(instance.role().clsid(), clsid);
            destroy(&instance);
        }
    }
}

#[test]
fn every_declared_interface_is_queryable_and_releases() {
    unsafe {
        let apo = BoostApo::create(ApoRole::Stream);
        let iids = [
            IID_IUNKNOWN,
            IID_IAUDIO_PROCESSING_OBJECT,
            IID_IAUDIO_PROCESSING_OBJECT_CONFIGURATION,
            IID_IAUDIO_PROCESSING_OBJECT_RT,
            IID_IAUDIO_SYSTEM_EFFECTS,
            IID_IAUDIO_SYSTEM_EFFECTS2,
        ];
        let mut addresses = Vec::new();
        // The object starts with no references outstanding, so the test takes
        // one and keeps it for the duration of the loop. (`add_ref` reports the
        // count it leaves behind.)
        assert_eq!(add_ref(apo), 1);
        for iid in iids {
            let mut pointer: *mut c_void = null_mut();
            assert_eq!(query_interface(apo, &iid, &mut pointer), S_OK, "{iid}");
            assert!(!pointer.is_null());
            addresses.push(pointer);
            release(apo);
        }

        // IUnknown and IAudioProcessingObject are the same identity...
        assert_eq!(addresses[0], addresses[1]);
        // ...and every other interface must live at its own address, or the
        // engine would call the wrong vtable.
        for i in 2..addresses.len() {
            for j in 0..i {
                assert_ne!(addresses[i], addresses[j], "interfaces share an address");
            }
        }

        // An interface we do not implement must be refused.
        let mut pointer: *mut c_void = null_mut();
        assert_eq!(
            query_interface(apo, &IID_IAUDIO_MEDIA_TYPE, &mut pointer),
            E_NOINTERFACE
        );
        assert!(pointer.is_null());

        // References counted from the other entry point: every QueryInterface
        // above was given back, so the one the test took is the last. Release
        // reports the count it leaves behind, and zero frees the object.
        assert_eq!(release(apo), 0);
    }
}

#[test]
fn registration_properties_are_engine_owned_and_complete() {
    unsafe {
        let apo = BoostApo::create(ApoRole::Endpoint);
        let vtbl = *(apo as *mut *const IAudioProcessingObjectVtbl);
        let mut properties: *mut ApoRegProperties = null_mut();
        assert_eq!(
            ((*vtbl).get_registration_properties)(apo as *mut c_void, &mut properties),
            S_OK
        );
        assert!(!properties.is_null());
        let registration = &*properties;

        assert_eq!(registration.clsid, CLSID_ENDPOINT);
        // One interface, as the reference product reports: the engine asks for
        // the rest by IID through QueryInterface.
        assert_eq!(registration.num_apo_interfaces, 1);
        assert_eq!(
            registration.iid_apo_interface_list[0],
            IID_IAUDIO_PROCESSING_OBJECT
        );
        // INPLACE | FRAMESPERSECOND_MUST_MATCH | BITSPERSAMPLE_MUST_MATCH, the
        // same set the reference product reports.
        assert_eq!(registration.flags, 0x0D);
        assert_ne!(registration.flags & APO_FLAG_INPLACE, 0);
        assert_ne!(registration.flags & APO_FLAG_FRAMESPERSECOND_MUST_MATCH, 0);
        assert_eq!(registration.min_input_connections, 1);
        assert_eq!(registration.max_output_connections, 1);
        assert_eq!(registration.max_instances, u32::MAX);
        assert_eq!(registration.major_version, 1);
        assert_eq!(registration.minor_version, 0);

        let name: String = registration
            .friendly_name
            .iter()
            .take_while(|unit| **unit != 0)
            .map(|unit| char::from_u32(*unit as u32).unwrap_or('?'))
            .collect();
        assert!(name.contains("Endpoint"), "unexpected name {name:?}");

        // What the DLL reports must be what the installer writes into the
        // engine's APO store, or the engine will refuse the effect.
        let expected = sb_apo::registration::for_role(ApoRole::Endpoint);
        assert_eq!(registration.clsid, expected.clsid);
        assert_eq!(registration.flags, expected.flags);
        assert_eq!(
            registration.num_apo_interfaces,
            expected.interfaces.len() as u32
        );
        assert_eq!(name, expected.friendly_name);

        // The engine frees this with CoTaskMemFree; if it were a Rust
        // allocation that would corrupt the heap inside audiodg.exe.
        CoTaskMemFree(properties as *const c_void);
        // No `QueryInterface` ever handed a reference out here, so this is not
        // `release` (which would be releasing something nobody took).
        sb_apo::object::destroy(apo);
    }
}

#[test]
fn latency_reports_the_lookahead_and_is_zero_before_locking() {
    unsafe {
        let instance = create(CLSID_ENDPOINT);
        assert_eq!(latency_hns(&instance), 0, "no format yet, no latency");

        let _guard = control_lock();
        let control = publish(ControlSnapshot::from_ui(Preset::Music, 250, None));
        assert_eq!(initialize(&instance, CLSID_ENDPOINT), S_OK);
        let mut media = float32_media_type();
        assert_eq!(lock(&instance, &mut media, FRAMES as u32), S_OK);

        // Music uses a 5 ms look-ahead.
        let hundred_ns = latency_hns(&instance);
        assert!(
            (hundred_ns - 50_000).abs() < 1_000,
            "expected ~5 ms, got {hundred_ns} (100 ns units)"
        );

        destroy(&instance);
        drop(control);
    }
}

#[test]
fn endpoint_stage_boosts_towards_the_ceiling_without_exceeding_it() {
    let _guard = control_lock();
    // Full leveling strength would move the signal; keep the stage switches
    // clean so this test is about boost and limiting only.
    let control = publish(ControlSnapshot::from_ui(Preset::Music, 500, Some(0.0)));

    unsafe {
        let (instance, _media) = ready(CLSID_ENDPOINT);
        let mut input = sine(0.5, FRAMES);
        let mut output = vec![0.0f32; input.len()];
        process(&instance, &mut input, &mut output, FRAMES as u32);

        // 500 % is +13.98 dB; 0.5 * 4.94 would be 2.47 without a limiter.
        assert!(
            peak(&output) > 0.5,
            "boost did not happen: {}",
            peak(&output)
        );
        // Music's ceiling is -0.5 dBFS.
        assert!(peak(&output) <= 0.95, "ceiling broken: {}", peak(&output));
        assert!(
            peak(&output) > 0.8,
            "never reached the ceiling: {}",
            peak(&output)
        );

        destroy(&instance);
    }
    drop(control);
}

#[test]
fn endpoint_stage_at_one_hundred_percent_is_transparent() {
    let _guard = control_lock();
    let control = publish(ControlSnapshot::from_ui(Preset::Music, 100, Some(0.0)));

    unsafe {
        let (instance, _media) = ready(CLSID_ENDPOINT);
        let mut input = sine(0.25, FRAMES);
        let mut output = vec![0.0f32; FRAMES * CHANNELS as usize];
        process(&instance, &mut input, &mut output, FRAMES as u32);

        // No boost, no leveling, and 0.25 is below the ceiling: the only
        // difference is the limiter's delay.
        let before = rms(&input[512 * CHANNELS as usize..]);
        let after = rms(settled(&output));
        assert!((before - after).abs() < 1e-3, "{before} vs {after}");

        destroy(&instance);
    }
    drop(control);
}

#[test]
fn stream_stage_lifts_quiet_audio_and_leaves_loud_audio_alone() {
    let _guard = control_lock();
    // Full strength leveling, slider at 500 %: the stream stage must level but
    // must not apply the manual boost (that belongs to the endpoint stage).
    let control = publish(ControlSnapshot::from_ui(Preset::Music, 500, Some(1.0)));

    unsafe {
        let (instance, _media) = ready(CLSID_STREAM);

        // Quiet source: ~-29 dBFS RMS, well under Music's -16 dBFS target.
        let mut quiet = sine(0.05, FRAMES);
        let mut output = vec![0.0f32; FRAMES * CHANNELS as usize];
        // The leveler's release is 600 ms, so give it a few seconds.
        for _ in 0..200 {
            process(&instance, &mut quiet, &mut output, FRAMES as u32);
        }
        let ratio = rms(settled(&output)) / rms(settled(&quiet));
        assert!(ratio > 2.5, "quiet source was not lifted (ratio {ratio})");

        // Loud source: already above the target, and the leveler never
        // attenuates, so it must come out at its original level.
        let mut loud = sine(0.5, FRAMES);
        for _ in 0..200 {
            process(&instance, &mut loud, &mut output, FRAMES as u32);
        }
        let ratio = rms(settled(&output)) / rms(settled(&loud));
        assert!(
            (0.9..1.1).contains(&ratio),
            "loud source was changed (ratio {ratio})"
        );

        destroy(&instance);
    }
    drop(control);
}

#[test]
fn a_silent_connection_stays_silent() {
    let _guard = control_lock();
    let control = publish(ControlSnapshot::from_ui(Preset::Night, 500, None));

    unsafe {
        let (instance, _media) = ready(CLSID_STREAM);

        // Explicitly flagged silent: we must pass the flag through untouched.
        let mut input = vec![0.0f32; FRAMES * CHANNELS as usize];
        let mut output = vec![1.0f32; FRAMES * CHANNELS as usize];
        let vtbl = *(instance.rt as *mut *const IAudioProcessingObjectRTVtbl);
        let mut input_property =
            ApoConnectionProperty::new(input.as_ptr() as usize, FRAMES as u32, APO_BUFFER_SILENT);
        let mut output_property = ApoConnectionProperty::new(
            output.as_mut_ptr() as usize,
            FRAMES as u32,
            APO_BUFFER_INVALID,
        );
        let mut inputs: *mut ApoConnectionProperty = &mut input_property;
        let mut outputs: *mut ApoConnectionProperty = &mut output_property;
        ((*vtbl).apo_process)(instance.rt, 1, &mut inputs, 1, &mut outputs);
        assert_eq!(output_property.buffer_flags, APO_BUFFER_SILENT);
        assert!(
            output.iter().all(|value| *value == 1.0),
            "silent buffer was written to"
        );

        // Digital silence that was *not* flagged: the gate must keep it at
        // zero rather than amplifying dither or rounding noise.
        let mut output = vec![0.0f32; FRAMES * CHANNELS as usize];
        for _ in 0..10 {
            process(&instance, &mut input, &mut output, FRAMES as u32);
        }
        assert_eq!(peak(&output), 0.0, "silence was amplified");

        destroy(&instance);
    }
    drop(control);
}

#[test]
fn the_effect_list_names_exactly_this_effect() {
    unsafe {
        let instance = create(CLSID_ENDPOINT);
        let vtbl = *(instance.effects as *mut *const IAudioSystemEffects2Vtbl);
        let mut effects: *mut GUID = null_mut();
        let mut count: u32 = 0;
        assert_eq!(
            ((*vtbl).get_effects_list)(instance.effects, &mut effects, &mut count, null_mut()),
            S_OK
        );
        assert_eq!(count, 1);
        assert_eq!(*effects, CLSID_ENDPOINT);
        CoTaskMemFree(effects as *const c_void);
        destroy(&instance);
    }
}

#[test]
fn locking_before_initialize_is_refused() {
    // LockForProcess is meaningless until the engine has handed us the APOInit
    // data, and answering S_OK here would let the engine call APOProcess on an
    // object with no format, no chain and no delay line.
    unsafe {
        let instance = create(CLSID_ENDPOINT);
        let mut media = float32_media_type();
        assert_eq!(
            lock(&instance, &mut media, FRAMES as u32),
            APOERR_NOT_INITIALIZED
        );
        destroy(&instance);
    }
}

#[test]
fn in_place_processing_takes_the_same_buffer() {
    // We register APO_FLAG_INPLACE, so the engine can hand us one buffer for
    // both connections. Processing has to survive that aliasing.
    let _guard = control_lock();
    let control = publish(ControlSnapshot::from_ui(Preset::Music, 500, Some(0.0)));

    unsafe {
        let (instance, _media) = ready(CLSID_ENDPOINT);
        let mut buffer = sine(0.5, FRAMES);
        let untouched = buffer.clone();
        let pointer = buffer.as_mut_ptr();
        process_buffers(&instance, pointer, pointer, FRAMES as u32);

        assert!(
            peak(&buffer) > 0.5,
            "boost did not happen: {}",
            peak(&buffer)
        );
        assert!(peak(&buffer) <= 0.95, "ceiling broken: {}", peak(&buffer));
        assert_ne!(buffer, untouched, "aliased buffer was left alone");

        destroy(&instance);
    }
    drop(control);
}

#[test]
fn an_unpublished_control_block_keeps_the_apo_a_true_bypass() {
    // The APO is the side that creates the section (audiodg.exe runs as LOCAL
    // SERVICE and may create a `Global\` object, the tray app may only open
    // one), so a section that exists but that no UI has stamped has to read as
    // "no UI yet" and leave audio alone. `initialize` is the only thing that
    // changes that, and only the tray app calls it.
    let _guard = control_lock();
    let control = publish(ControlSnapshot::default());
    control.block().magic.store(0, Ordering::Relaxed);

    unsafe {
        let (instance, _media) = ready(CLSID_ENDPOINT);
        let mut input = sine(0.5, FRAMES);
        let mut output = vec![0.0f32; FRAMES * CHANNELS as usize];
        process(&instance, &mut input, &mut output, FRAMES as u32);
        assert_eq!(output, input, "boosted with no UI running");
        destroy(&instance);
    }
}

#[test]
fn the_control_section_outlives_the_instance_that_created_it() {
    // One section per process, on purpose: the engine builds a fresh instance
    // for every stream and drops it when the stream ends, and the tray app can
    // only ever *attach* to a section somebody else is holding open. So after
    // the last instance is gone the DLL must still hold it.
    let _guard = control_lock();

    unsafe {
        let (instance, _media) = ready(CLSID_ENDPOINT);
        destroy(&instance);
    }

    assert!(
        MappedControl::open().is_ok(),
        "the section died with the APO instance"
    );
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// A stand-in for the engine's controlling unknown: an identity plus a
/// reference count, so a test can see which one the aggregate handed out.
#[repr(C)]
struct FakeOuter {
    vtbl: *const UnknownVtbl,
    refs: AtomicU32,
}

impl FakeOuter {
    fn new() -> Self {
        Self {
            vtbl: &OUTER_VTBL,
            // The engine's own reference to its wrapper.
            refs: AtomicU32::new(1),
        }
    }

    fn pointer(&mut self) -> *mut c_void {
        self as *mut FakeOuter as *mut c_void
    }
}

unsafe extern "system" fn outer_query_interface(
    this: *mut c_void,
    _riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    (*(this as *mut FakeOuter)).refs.fetch_add(1, Ordering::Relaxed);
    *ppv = this;
    S_OK
}

unsafe extern "system" fn outer_add_ref(this: *mut c_void) -> ULONG {
    (*(this as *mut FakeOuter)).refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn outer_release(this: *mut c_void) -> ULONG {
    (*(this as *mut FakeOuter)).refs.fetch_sub(1, Ordering::Relaxed) - 1
}

static OUTER_VTBL: UnknownVtbl = UnknownVtbl {
    query_interface: outer_query_interface,
    add_ref: outer_add_ref,
    release: outer_release,
};

#[test]
fn an_aggregated_object_answers_iunknown_with_the_controlling_unknown() {
    // `audiodg.exe` creates every APO as part of an aggregate and asks for
    // `IID_IUnknown` as its very next act. COM's identity rule says the answer
    // to that is the *outer*: one aggregate, one identity. Handing back our own
    // identity instead is what made the engine drop the APO -- created,
    // initialized, format-checked, never locked, over and over.
    unsafe {
        let mut outer = FakeOuter::new();
        let outer_pointer = outer.pointer();
        let apo = BoostApo::create_aggregated(ApoRole::Endpoint, outer_pointer);
        // Stands in for the reference `CreateInstance` hands the outer: without
        // it, the object would already have nothing keeping it alive.
        add_ref(apo);

        let mut identity: *mut c_void = null_mut();
        assert_eq!(query_interface(apo, &IID_IUNKNOWN, &mut identity), S_OK);
        assert_eq!(identity, outer_pointer, "did not answer with the outer");
        assert_eq!(
            outer.refs.load(Ordering::Relaxed),
            2,
            "the reference goes to the outer, not to us"
        );

        // The effect interfaces are still this object's own.
        let mut effect: *mut c_void = null_mut();
        assert_eq!(
            query_interface(apo, &IID_IAUDIO_PROCESSING_OBJECT, &mut effect),
            S_OK
        );
        assert_ne!(effect, outer_pointer);

        // Two references on *us*: the effect interface we just took, and the
        // one the outer holds. The identity reference went to the outer.
        assert_eq!(release(apo), 1);
        assert_eq!(release(apo), 0);
    }
}

#[test]
fn create_instance_hands_the_outer_a_reference_to_the_inner() {
    // The identity rule applies to calls *after* creation. `CreateInstance`
    // owes the outer a reference to the inner object itself, because that is
    // how the outer controls the inner's lifetime.
    unsafe {
        let mut outer = FakeOuter::new();
        let outer_pointer = outer.pointer();

        let mut factory: *mut c_void = null_mut();
        assert_eq!(
            sb_apo::DllGetClassObject(&CLSID_STREAM, &IID_ICLASS_FACTORY, &mut factory),
            S_OK
        );
        let factory_vtbl = *(factory as *mut *const IClassFactoryVtbl);

        let mut inner: *mut c_void = null_mut();
        assert_eq!(
            ((*factory_vtbl).create_instance)(
                factory,
                outer_pointer as *mut IUnknown,
                &IID_IUNKNOWN,
                &mut inner
            ),
            S_OK
        );
        assert_ne!(inner, outer_pointer, "the outer wants the inner itself");
        assert_eq!(
            outer.refs.load(Ordering::Relaxed),
            1,
            "the inner holds no reference on the outer"
        );

        // Once it is part of the aggregate, it answers with the outer.
        let inner_vtbl = *(inner as *mut *const IAudioProcessingObjectVtbl);
        let mut identity: *mut c_void = null_mut();
        assert_eq!(
            ((*inner_vtbl).query_interface)(inner, &IID_IUNKNOWN, &mut identity),
            S_OK
        );
        assert_eq!(identity, outer_pointer);
        assert_eq!(outer.refs.load(Ordering::Relaxed), 2);

        // Give everything back: the identity reference, then the inner.
        assert_eq!(outer_release(outer_pointer), 1);
        ((*factory_vtbl).release)(factory);
        assert_eq!(release(inner as *mut BoostApo), 0);
    }
}

// ---------------------------------------------------------------------------
// The audio thread under engine-like load
// ---------------------------------------------------------------------------

#[test]
fn an_in_place_connection_may_share_one_property_struct() {
    // The engine hands the *same* `APO_CONNECTION_PROPERTY` to both sides of an
    // in-place connection -- not merely the same buffer. Reading it through a
    // shared reference while writing through a mutable one is undefined
    // behaviour however harmless the reads look, so `APOProcess` copies the
    // property out first. This drives that exact shape: one property struct,
    // passed as both the input and the output.
    let _guard = control_lock();
    let control = publish(ControlSnapshot::from_ui(Preset::Music, 400, Some(0.0)));

    unsafe {
        let (instance, _media) = ready(CLSID_ENDPOINT);
        let mut buffer = sine(0.4, FRAMES);
        let pointer = buffer.as_mut_ptr();

        let vtbl = *(instance.rt as *mut *const IAudioProcessingObjectRTVtbl);
        let mut property =
            ApoConnectionProperty::new(pointer as usize, FRAMES as u32, APO_BUFFER_VALID);
        let mut shared: *mut ApoConnectionProperty = &mut property;
        let shared_pointer: *mut *mut ApoConnectionProperty = &mut shared;
        ((*vtbl).apo_process)(instance.rt, 1, shared_pointer, 1, shared_pointer);

        assert_eq!(property.valid_frame_count, FRAMES as u32);
        assert_eq!(property.buffer_flags, APO_BUFFER_VALID);
        assert!(
            peak(&buffer) > 0.4,
            "boost did not happen: {}",
            peak(&buffer)
        );
        assert!(peak(&buffer) <= 0.98, "ceiling broken: {}", peak(&buffer));

        destroy(&instance);
    }
    drop(control);
}

#[test]
fn a_long_stream_with_a_moving_slider_stays_finite_and_limited() {
    // What the engine does to a long stream: thousands of buffers, settings
    // changing underneath, quiet passages the leveler lifts and loud ones it
    // leaves alone, and a ceiling that has to hold throughout. Two properties
    // are asserted on every sample, because both are the kind of thing that
    // only shows up after the first few hundred buffers:
    //
    // * every sample is finite (a NaN or an infinity leaving an APO is a crash
    //   waiting to happen in whatever consumes the buffer next), and
    // * the ceiling holds, whichever preset is in force.
    let _guard = control_lock();
    let control = publish(ControlSnapshot::from_ui(Preset::Music, 300, Some(1.0)));

    unsafe {
        let (instance, _media) = ready(CLSID_ENDPOINT);
        let mut output = vec![0.0f32; FRAMES * CHANNELS as usize];
        let mut worst_peak = 0.0f32;

        for round in 0..1_200u32 {
            // A moving slider, a rotating preset and the occasional change of
            // leveling strength: three different ways into `set_params`.
            if round % 137 == 0 {
                let percent = 100 + (round * 37) % 401;
                let preset = Preset::ALL[(round / 137) as usize % Preset::ALL.len()];
                let strength = if (round / 137) % 2 == 0 { 1.0 } else { 0.0 };
                control
                    .block()
                    .write(&ControlSnapshot::from_ui(preset, percent, Some(strength)));
            }

            let amplitude = if (round / 50) % 2 == 0 { 0.02 } else { 0.7 };
            let mut buffer = sine(amplitude, FRAMES);
            process(&instance, &mut buffer, &mut output, FRAMES as u32);

            for sample in &output {
                assert!(
                    sample.is_finite(),
                    "non-finite sample in round {round}: {sample}"
                );
                worst_peak = worst_peak.max(sample.abs());
            }
            // -0.3 dBFS is the highest ceiling any preset asks for.
            assert!(
                worst_peak <= 0.99,
                "ceiling broken in round {round}: {worst_peak}"
            );
        }

        assert!(worst_peak > 0.5, "nothing was ever boosted: {worst_peak}");
        destroy(&instance);
    }
    drop(control);
}
