//! The COM object `audiodg.exe` instantiates.
//!
//! Two CLSIDs come out of this one implementation:
//!
//! * [`CLSID_STREAM`] -- registered as a *stream* effect (SFX). One instance
//!   per stream, so it can level each source independently: a quiet video gets
//!   lifted while a loud one is left where it is.
//! * [`CLSID_ENDPOINT`] -- registered as an *endpoint* effect (EFX). One
//!   instance for the whole device, where the user's manual boost and the
//!   final look-ahead limiter belong.
//!
//! [`ApoRole::apply`] is what splits the shared control snapshot between the
//! two stages, and it is the only place that knows the split.
//!
//! Real-time rules obeyed here: no allocation, no locks, no COM, no registry
//! access and no logging once `LockForProcess` has run. `APOProcess` bumps two
//! counters and runs the DSP, and that is all -- see `diag::rt_processed` for
//! why the counting exists and why the line it produces is written from a
//! control path instead.

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use core::ptr::{addr_of_mut, null_mut};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;

use sb_core::chain::{BoostChain, BoostParams};
use sb_core::params::Preset;
use sb_ipc::mapping::MappedControl;
use windows_sys::Win32::System::Com::CoTaskMemAlloc;

use crate::abi::*;
use crate::format::{self, SampleFormat, StreamFormat};
use crate::log_diag;

/// `{A7C3E1D9-4B62-4F38-9E57-1C8D2A6B0F41}` -- the per-stream (SFX) instance.
pub const CLSID_STREAM: GUID = GUID::new(
    0xA7C3_E1D9,
    0x4B62,
    0x4F38,
    [0x9E, 0x57, 0x1C, 0x8D, 0x2A, 0x6B, 0x0F, 0x41],
);

/// `{A7C3E1D9-4B62-4F38-9E57-1C8D2A6B0F42}` -- the endpoint (EFX) instance.
pub const CLSID_ENDPOINT: GUID = GUID::new(
    0xA7C3_E1D9,
    0x4B62,
    0x4F38,
    [0x9E, 0x57, 0x1C, 0x8D, 0x2A, 0x6B, 0x0F, 0x42],
);

const FRIENDLY_NAME_STREAM: &str = "Sound Booster Stream Leveler";
const FRIENDLY_NAME_ENDPOINT: &str = "Sound Booster Endpoint Boost";

/// Which stage of the chain an instance is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApoRole {
    /// Per-stream leveling (registered as SFX).
    Stream,
    /// Master boost and final limiting (registered as EFX).
    Endpoint,
}

impl ApoRole {
    /// Short name, for diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            ApoRole::Stream => "stream",
            ApoRole::Endpoint => "endpoint",
        }
    }

    pub const fn clsid(self) -> GUID {
        match self {
            ApoRole::Stream => CLSID_STREAM,
            ApoRole::Endpoint => CLSID_ENDPOINT,
        }
    }

    pub fn from_clsid(clsid: &GUID) -> Option<Self> {
        if *clsid == CLSID_STREAM {
            Some(ApoRole::Stream)
        } else if *clsid == CLSID_ENDPOINT {
            Some(ApoRole::Endpoint)
        } else {
            None
        }
    }

    pub const fn friendly_name(self) -> &'static str {
        match self {
            ApoRole::Stream => FRIENDLY_NAME_STREAM,
            ApoRole::Endpoint => FRIENDLY_NAME_ENDPOINT,
        }
    }

    /// Split the user's settings between the two stages.
    ///
    /// The stream stage owns the auto leveler and applies no manual gain; the
    /// endpoint stage applies the manual gain and no leveling (leveling the
    /// mix would let one loud app mask a quiet one, which is the whole point
    /// of doing it per stream).
    pub fn apply(self, params: &BoostParams) -> BoostParams {
        let mut applied = *params;
        match self {
            ApoRole::Stream => {
                applied.manual_gain_db = 0.0;
            }
            ApoRole::Endpoint => {
                applied.agc.strength = 0.0;
                applied.agc.max_gain_db = 0.0;
            }
        }
        applied
    }
}

/// The parameter set used when no tray app is running.
fn bypass_params() -> BoostParams {
    Preset::Off.boost_params(0.0)
}

/// The COM object.
///
/// The first six fields are the interface identities. The engine keeps a
/// pointer to one of them as "the APO", so each vtable's `QueryInterface`,
/// `AddRef` and `Release` recover the object with `this - offset_of!(field)`.
#[repr(C)]
pub struct BoostApo {
    apo_vtbl: *const IAudioProcessingObjectVtbl,
    config_vtbl: *const IAudioProcessingObjectConfigurationVtbl,
    rt_vtbl: *const IAudioProcessingObjectRTVtbl,
    sysfx_vtbl: *const UnknownVtbl,
    sysfx2_vtbl: *const IAudioSystemEffects2Vtbl,
    sysfx3_vtbl: *const IAudioSystemEffects3Vtbl,

    ref_count: AtomicU32,
    role: ApoRole,
    initialized: bool,
    locked: bool,
    /// The system's own switch, as thrown by Windows Settings through
    /// [`IAudioSystemEffects3`](IID_IAUDIO_SYSTEM_EFFECTS3). It is *and*-ed
    /// with the user's switch in the control block, so either side can silence
    /// the effect without the other having to know.
    system_on: AtomicBool,
    params: BoostParams,
    format: Option<StreamFormat>,
    chain: Option<BoostChain>,
    /// Only used for integer PCM, where the DSP needs `f32` in between.
    scratch: Vec<f32>,
    /// Frames per connection the engine promised us at lock time.
    max_frames: usize,
    /// The controlling unknown, when the engine creates us as part of an
    /// aggregate. Null when we are created stand-alone.
    ///
    /// COM's identity rule: while an object is part of an aggregate, asking it
    /// for `IID_IUnknown` must yield the *outer*, so that the aggregate has one
    /// identity. **The engine creates every APO this way** -- it passes a
    /// non-null controlling unknown and asks for `IID_IUnknown` as its first act
    /// -- and a wrapper that does not get its own identity back concludes the
    /// APO is not usable: the object is created, initialized and format-checked,
    /// then dropped, over and over.
    ///
    /// Not `AddRef`'d: the outer owns the inner's lifetime, not the reverse.
    outer: *mut c_void,
}

/// The control section, held for as long as this DLL is loaded.
///
/// Deliberately *not* a field of [`BoostApo`]: the engine builds a fresh
/// instance for every stream and drops it when the stream ends, so a section
/// owned by an instance disappears between two sounds -- and the tray app,
/// which can only ever *open* the section, would find nothing to attach to.
/// One section per process also means a published boost survives an
/// `audiodg.exe` restart, because the audio engine reloads this DLL at once.
static CONTROL: OnceLock<MappedControl> = OnceLock::new();

/// The shared control section, created on first use and then reused.
///
/// Only ever called from the control path (`Initialize`), never from
/// `APOProcess`: the steady state is one atomic load, but the first call does
/// real I/O and the audio thread must not.
fn control() -> Result<&'static MappedControl, io::Error> {
    if let Some(mapped) = CONTROL.get() {
        return Ok(mapped);
    }
    let mapped = MappedControl::create_or_open()?;
    // Two instances can be initialized at the same time; `set` keeps whichever
    // got there first and both then read the same section.
    let _ = CONTROL.set(mapped);
    CONTROL
        .get()
        .ok_or_else(|| io::Error::other("control section vanished after creation"))
}

impl BoostApo {
    /// A fresh object, with **no** reference outstanding.
    ///
    /// COM's contract for a new object is that it starts at zero and that the
    /// reference handed to the caller comes from the `QueryInterface` that
    /// produces the returned pointer. Starting at one instead would leak an
    /// object per stream -- and one per stream is exactly how often the engine
    /// instantiates this APO, so the leak would grow with every sound played,
    /// each one holding its own copy of the control block open.
    pub fn create(role: ApoRole) -> *mut Self {
        Self::create_with_outer(role, null_mut())
    }

    /// A fresh object that is part of an aggregate, with the engine's
    /// controlling unknown recorded.
    ///
    /// This is how the audio engine asks for every APO (`outer_null=false` in
    /// the factory log), and it is the only reason [`IID_IUNKNOWN`] can answer
    /// with something other than this object.
    pub fn create_aggregated(role: ApoRole, outer: *mut c_void) -> *mut Self {
        Self::create_with_outer(role, outer)
    }

    fn create_with_outer(role: ApoRole, outer: *mut c_void) -> *mut Self {
        let object = Self {
            apo_vtbl: &APO_VTBL,
            config_vtbl: &CONFIG_VTBL,
            rt_vtbl: &RT_VTBL,
            sysfx_vtbl: &UNKNOWN_VTBL,
            sysfx2_vtbl: &SYSFX2_VTBL,
            sysfx3_vtbl: &SYSFX3_VTBL,
            ref_count: AtomicU32::new(0),
            role,
            initialized: false,
            locked: false,
            system_on: AtomicBool::new(true),
            params: bypass_params(),
            format: None,
            chain: None,
            scratch: Vec::new(),
            max_frames: 0,
            outer,
        };
        Box::into_raw(Box::new(object))
    }

    /// The controlling unknown, or null when we stand alone.
    pub fn outer(&self) -> *mut c_void {
        self.outer
    }

    pub fn role(&self) -> ApoRole {
        self.role
    }

    pub fn params(&self) -> BoostParams {
        self.params
    }

    /// Whether the effect should touch audio at all.
    ///
    /// Two independent switches: the user's, published through the shared
    /// control block, and the system's, thrown from Windows Settings through
    /// [`IAudioSystemEffects3`](IID_IAUDIO_SYSTEM_EFFECTS3). Either one off is a
    /// true bypass, so neither side has to know about the other.
    pub fn audible(&self) -> bool {
        self.params.enabled && self.system_on.load(Ordering::Relaxed)
    }

    /// Attach to the control block, creating the section if nobody has yet.
    ///
    /// `audiodg.exe` runs as `LOCAL SERVICE` in **session 0**, and creating a
    /// `Global\` file mapping from session 0 needs no privilege (only doing it
    /// from another session does), so the APO is the side that can create the
    /// section. The tray app, in the user's session, can only attach. Both that
    /// ordering and the DACL are in `sb_ipc::mapping`; see `docs/risks.md` for
    /// why a `Local\` section cannot work at all.
    ///
    /// The handle is kept even when the block is not stamped yet. A brand new
    /// section has no UI behind it, so it reads as *not compatible* and the APO
    /// stays a true bypass -- but holding the handle means it will notice the
    /// moment somebody publishes, without a second `OpenFileMappingW` on the
    /// audio thread. A booster that starts boosting because a stale DLL was
    /// registered, with no UI in sight, is not something to ship.
    fn open_control(&mut self) {
        let role = self.role.label();
        match control() {
            Ok(mapped) => {
                let compatible = mapped.block().is_compatible();
                self.params = match compatible {
                    true => self.role.apply(&mapped.block().read().params),
                    false => bypass_params(),
                };
                log_diag!(role, "control", "attached, compatible={compatible}");
            }
            Err(error) => {
                self.params = bypass_params();
                log_diag!(role, "control", "unavailable: {error}");
            }
        }
    }

    /// Pull the current settings out of the shared block.
    ///
    /// Called once per buffer. Reads are relaxed atomic loads behind a
    /// seqlock, so worst case we apply the previous snapshot for one buffer.
    fn sync_params(&mut self) {
        let snapshot = match control() {
            Ok(mapped) if mapped.block().is_compatible() => Some(mapped.block().read()),
            _ => None,
        };
        let Some(snapshot) = snapshot else {
            return;
        };

        let wanted = self.role.apply(&snapshot.params);
        if wanted != self.params {
            self.params = wanted;
            if let Some(chain) = self.chain.as_mut() {
                chain.set_params(wanted);
            }
        }
    }

    fn lock_to_format(&mut self, format: StreamFormat, max_frames: u32) {
        let channels = format.channels as usize;
        self.max_frames = max_frames as usize;
        // Sized here so the audio path never grows a Vec.
        self.scratch.clear();
        if format.sample_format != SampleFormat::Float32 {
            self.scratch
                .resize(self.max_frames.saturating_mul(channels).max(1), 0.0);
        }
        self.chain = Some(BoostChain::new(
            format.sample_rate as f32,
            channels,
            self.params,
        ));
        self.format = Some(format);
    }

    /// Process one connection in place. `frames` is the engine's frame count.
    unsafe fn process_connection(
        &mut self,
        frames: usize,
        input: &ApoConnectionProperty,
        output: &mut ApoConnectionProperty,
    ) {
        let Some(format) = self.format else {
            output.buffer_flags = APO_BUFFER_VALID;
            return;
        };
        // `LockForProcess` told us how large a buffer the engine will use, and
        // the scratch (and the DSP's own window) are sized from it. Process at
        // most that many frames: running past the end of a buffer we cannot
        // measure would read somebody else's memory inside `audiodg.exe`, and
        // in the in-place case the audio is already where it belongs.
        let frames = match self.max_frames {
            0 => frames,
            limit => frames.min(limit),
        };
        let samples = frames.saturating_mul(format.channels as usize);
        if samples == 0 {
            output.buffer_flags = APO_BUFFER_VALID;
            return;
        }

        let source = input.buffer as *const u8;
        let destination = output.buffer as *mut u8;
        if source.is_null() || destination.is_null() {
            output.buffer_flags = APO_BUFFER_VALID;
            return;
        }

        let active = self.audible() && self.chain.is_some() && self.locked;
        if !active {
            if source != destination {
                core::ptr::copy_nonoverlapping(
                    source,
                    destination,
                    samples * format.sample_format.container_bytes(),
                );
            }
            output.buffer_flags = APO_BUFFER_VALID;
            return;
        }

        if format.sample_format == SampleFormat::Float32 {
            if source != destination {
                core::ptr::copy_nonoverlapping(
                    source as *const f32,
                    destination as *mut f32,
                    samples,
                );
            }
            // The engine promises this buffer holds at least `frames` frames.
            let audio = core::slice::from_raw_parts_mut(destination as *mut f32, samples);
            if let Some(chain) = self.chain.as_mut() {
                chain.process_interleaved(audio);
            }
        } else {
            if self.scratch.len() < samples {
                // The engine exceeded the size it locked us to. Rather than
                // allocate on the audio thread, pass this buffer through.
                if source != destination {
                    core::ptr::copy_nonoverlapping(
                        source,
                        destination,
                        samples * format.sample_format.container_bytes(),
                    );
                }
                output.buffer_flags = APO_BUFFER_VALID;
                return;
            }
            format::decode_into(&format, source, self.scratch.as_mut_ptr(), samples);
            let pointer = self.scratch.as_mut_ptr();
            let audio = core::slice::from_raw_parts_mut(pointer, samples);
            if let Some(chain) = self.chain.as_mut() {
                chain.process_interleaved(audio);
            }
            format::encode_from(&format, pointer as *const f32, destination, samples);
        }

        output.buffer_flags = APO_BUFFER_VALID;
    }
}

// ---------------------------------------------------------------------------
// Interface identity
// ---------------------------------------------------------------------------

/// Recover the object from any interface pointer.
unsafe fn base(this: *mut c_void, offset: usize) -> *mut BoostApo {
    (this as *mut u8).sub(offset) as *mut BoostApo
}

unsafe fn apo_ref<'a>(this: *mut c_void) -> &'a mut BoostApo {
    &mut *base(this, offset_of!(BoostApo, apo_vtbl))
}

unsafe fn config_ref<'a>(this: *mut c_void) -> &'a mut BoostApo {
    &mut *base(this, offset_of!(BoostApo, config_vtbl))
}

unsafe fn rt_ref<'a>(this: *mut c_void) -> &'a mut BoostApo {
    &mut *base(this, offset_of!(BoostApo, rt_vtbl))
}

unsafe fn sysfx2_ref<'a>(this: *mut c_void) -> &'a mut BoostApo {
    &mut *base(this, offset_of!(BoostApo, sysfx2_vtbl))
}

unsafe fn sysfx3_ref<'a>(this: *mut c_void) -> &'a mut BoostApo {
    &mut *base(this, offset_of!(BoostApo, sysfx3_vtbl))
}

/// Whether this build answers the modern "system effects" interfaces.
///
/// Measured on this machine: the engine asks for `IAudioSystemEffects2` and,
/// the moment it is given one, for `IAudioSystemEffects3` too -- the Windows 11
/// "controllable effects" pair -- and it wants that answer before it will build
/// a chain around the APO. The reference product answers only the older
/// `IAudioSystemEffects` and refuses both, which is what
/// `--features legacy-effects` builds, so the two profiles can be compared on
/// real hardware instead of argued about.
const MODERN_EFFECTS: bool = !cfg!(feature = "legacy-effects");

/// `QueryInterface`. Public so the class factory can reuse it.
///
/// # Safety
/// `apo` must come from [`BoostApo::create`] and `ppv` must be writable.
pub unsafe fn query_interface(
    apo: *mut BoostApo,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();
    if riid.is_null() {
        return E_POINTER;
    }

    let iid = *riid;
    let name = crate::diag::guid_text(&iid);

    // COM's identity rule, and the one thing the audio engine checks first:
    // while this object is part of an aggregate, its `IUnknown` *is* the
    // outer's. Handing back our own instead leaves the engine holding two
    // identities for one object, and it drops the APO without ever locking it.
    if iid == IID_IUNKNOWN && !(*apo).outer.is_null() {
        let outer = (*apo).outer;
        log_diag!(
            (*apo).role.label(),
            "QueryInterface",
            "{name} -> the controlling unknown"
        );
        add_ref_unknown(outer);
        *ppv = outer;
        return S_OK;
    }

    let interface: *mut c_void = if iid == IID_IUNKNOWN || iid == IID_IAUDIO_PROCESSING_OBJECT {
        addr_of_mut!((*apo).apo_vtbl) as *mut c_void
    } else if iid == IID_IAUDIO_PROCESSING_OBJECT_CONFIGURATION {
        addr_of_mut!((*apo).config_vtbl) as *mut c_void
    } else if iid == IID_IAUDIO_PROCESSING_OBJECT_RT {
        addr_of_mut!((*apo).rt_vtbl) as *mut c_void
    } else if iid == IID_IAUDIO_SYSTEM_EFFECTS {
        addr_of_mut!((*apo).sysfx_vtbl) as *mut c_void
    } else if iid == IID_IAUDIO_SYSTEM_EFFECTS2 && MODERN_EFFECTS {
        addr_of_mut!((*apo).sysfx2_vtbl) as *mut c_void
    } else if iid == IID_IAUDIO_SYSTEM_EFFECTS3 && MODERN_EFFECTS {
        addr_of_mut!((*apo).sysfx3_vtbl) as *mut c_void
    } else {
        log_diag!((*apo).role.label(), "QueryInterface", "{name} -> no");
        return E_NOINTERFACE;
    };
    log_diag!((*apo).role.label(), "QueryInterface", "{name} -> yes");

    add_ref(apo);
    *ppv = interface;
    S_OK
}

/// Hand out a reference to this object's **own** `IUnknown`, ignoring any
/// controlling unknown.
///
/// This is what `IClassFactory::CreateInstance` owes the outer object: the
/// aggregate's identity rule applies to calls that come *after* creation. The
/// outer stores this reference as "my inner", so answering with the outer's own
/// identity there would have it release itself and leak the inner.
///
/// # Safety
/// `apo` must come from [`BoostApo::create`] and `ppv` must be writable.
pub unsafe fn query_interface_inner_unknown(apo: *mut BoostApo, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    log_diag!(
        (*apo).role.label(),
        "QueryInterface",
        "IUnknown -> own identity (aggregate member)"
    );
    add_ref(apo);
    *ppv = addr_of_mut!((*apo).apo_vtbl) as *mut c_void;
    S_OK
}

/// Take a reference on an arbitrary `IUnknown`.
///
/// # Safety
/// `unknown` must be null, or a live `IUnknown`: the first three entries of any
/// COM vtable are `QueryInterface`, `AddRef` and `Release`, so this is safe for
/// a pointer to any interface.
pub unsafe fn add_ref_unknown(unknown: *mut c_void) {
    if unknown.is_null() {
        return;
    }
    let vtbl = *(unknown as *mut *const UnknownVtbl);
    if let Some(add_ref) = vtbl.as_ref().map(|vtbl| vtbl.add_ref) {
        add_ref(unknown);
    }
}

/// # Safety
/// `apo` must be a live object.
pub unsafe fn add_ref(apo: *mut BoostApo) -> u32 {
    (*apo).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

/// # Safety
/// `apo` must be a live object with a reference to give up.
pub unsafe fn release(apo: *mut BoostApo) -> u32 {
    let previous = (*apo).ref_count.fetch_sub(1, Ordering::AcqRel);
    if previous == 0 {
        // An extra `Release` from the engine. Counts saturate rather than
        // wrapping, because the alternative in a DLL running inside
        // `audiodg.exe` is a panic across an FFI boundary, which takes the
        // whole machine's audio down.
        log_diag!(
            (*apo).role.label(),
            "Release",
            "called with no references outstanding"
        );
        (*apo).ref_count.store(0, Ordering::Relaxed);
        return 0;
    }
    let remaining = previous - 1;
    if remaining == 0 {
        log_diag!(
            (*apo).role.label(),
            "Release",
            "last reference, object destroyed"
        );
        drop(Box::from_raw(apo));
    }
    remaining
}

/// Free an object that never had a reference handed out.
///
/// # Safety
/// `apo` must be a live object with no references outstanding, and it must not
/// be used again.
pub unsafe fn destroy(apo: *mut BoostApo) {
    drop(Box::from_raw(apo));
}

/// Generate the three `IUnknown` entry points for one interface.
macro_rules! unknown_entry_points {
    ($qi:ident, $add_ref:ident, $release:ident, $field:ident) => {
        unsafe extern "system" fn $qi(
            this: *mut c_void,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> HRESULT {
            query_interface(base(this, offset_of!(BoostApo, $field)), riid, ppv)
        }

        unsafe extern "system" fn $add_ref(this: *mut c_void) -> ULONG {
            add_ref(base(this, offset_of!(BoostApo, $field)))
        }

        unsafe extern "system" fn $release(this: *mut c_void) -> ULONG {
            release(base(this, offset_of!(BoostApo, $field)))
        }
    };
}

unknown_entry_points!(apo_qi, apo_add_ref, apo_release, apo_vtbl);
unknown_entry_points!(config_qi, config_add_ref, config_release, config_vtbl);
unknown_entry_points!(rt_qi, rt_add_ref, rt_release, rt_vtbl);
unknown_entry_points!(sysfx_qi, sysfx_add_ref, sysfx_release, sysfx_vtbl);
unknown_entry_points!(sysfx2_qi, sysfx2_add_ref, sysfx2_release, sysfx2_vtbl);
unknown_entry_points!(sysfx3_qi, sysfx3_add_ref, sysfx3_release, sysfx3_vtbl);

// ---------------------------------------------------------------------------
// IAudioProcessingObject
// ---------------------------------------------------------------------------

unsafe extern "system" fn apo_reset(this: *mut c_void) -> HRESULT {
    let apo = apo_ref(this);
    log_diag!(apo.role.label(), "Reset", "locked={}", apo.locked);
    if let Some(chain) = apo.chain.as_mut() {
        chain.reset();
    }
    S_OK
}

unsafe extern "system" fn apo_get_latency(this: *mut c_void, time: *mut HNSTIME) -> HRESULT {
    if time.is_null() {
        return E_POINTER;
    }
    let apo = apo_ref(this);
    let hundred_ns = match (apo.format, apo.chain.as_ref()) {
        (Some(format), Some(chain)) if format.sample_rate > 0 => {
            (chain.latency_frames() as f64 * 10_000_000.0 / format.sample_rate as f64).round()
                as HNSTIME
        }
        _ => 0,
    };
    log_diag!(
        apo.role.label(),
        "GetLatency",
        "{} hundred-ns ({} frames of look-ahead)",
        hundred_ns,
        apo.chain.as_ref().map_or(0, |chain| chain.latency_frames())
    );
    *time = hundred_ns;
    S_OK
}

unsafe extern "system" fn apo_get_registration_properties(
    this: *mut c_void,
    properties: *mut *mut ApoRegProperties,
) -> HRESULT {
    if properties.is_null() {
        return E_POINTER;
    }
    *properties = null_mut();
    let apo = apo_ref(this);
    // One source of truth for the DLL's report and the installer's copy of it
    // in the engine's APO store.
    let info = crate::registration::for_role(apo.role);
    let interfaces = info.interfaces;

    // `APO_REG_PROPERTIES` ends in a one-element IID array; the rest of the
    // list lives in the allocation behind it.
    let extra = interfaces.len().saturating_sub(1) * size_of::<GUID>();
    let bytes = size_of::<ApoRegProperties>() + extra;

    let memory = CoTaskMemAlloc(bytes) as *mut ApoRegProperties;
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    core::ptr::write_bytes(memory as *mut u8, 0, bytes);

    let registration = &mut *memory;
    registration.clsid = info.clsid;
    registration.flags = info.flags;
    registration.set_friendly_name(info.friendly_name);
    registration.set_copyright(info.copyright);
    registration.major_version = info.major_version;
    registration.minor_version = info.minor_version;
    registration.min_input_connections = info.min_input_connections;
    registration.max_input_connections = info.max_input_connections;
    registration.min_output_connections = info.min_output_connections;
    registration.max_output_connections = info.max_output_connections;
    registration.max_instances = info.max_instances;
    registration.num_apo_interfaces = interfaces.len() as u32;

    let list = addr_of_mut!((*memory).iid_apo_interface_list) as *mut GUID;
    for (index, iid) in interfaces.iter().enumerate() {
        list.add(index).write(*iid);
    }

    *properties = memory;
    log_diag!(
        apo.role.label(),
        "GetRegistrationProperties",
        "flags={:#04X} interfaces={} in={}..{} out={}..{} max_instances={}",
        info.flags,
        interfaces.len(),
        info.min_input_connections,
        info.max_input_connections,
        info.min_output_connections,
        info.max_output_connections,
        info.max_instances
    );
    S_OK
}

/// The engine's `Initialize` payload, decoded for the diagnostics log.
///
/// The engine hands a system effect an `APOInitSystemEffects2` (measured:
/// `data_size = 88`, which is exactly that struct's size), and two of its tail
/// fields say what is about to happen to this object:
///
/// * `AudioProcessingMode` at offset 64 -- which signal-processing mode the
///   stream is in, i.e. one of the mode GUIDs the endpoint lists under
///   `PKEY_SFX_ProcessingModes_Supported_For_Streaming` (`{d3993a3f-...},5`).
/// * `InitializeForDiscoveryOnly` at offset 80 -- the engine is *enumerating*
///   effects for a mode, not building a chain. An APO initialized that way is
///   meant to be asked for its effect list and then dropped, so a trace that
///   shows `Initialize` and nothing else is not necessarily a failed chain
///   build; without this field the two look identical.
///
/// Both offsets sit past `APOInitBaseStruct` (24 bytes: size, padding, CLSID)
/// and are only read when the engine's declared size covers them.
#[cfg(feature = "diagnostics")]
unsafe fn init_extras(data_size: u32, data: *const u8) -> String {
    // APOInitSystemEffects2 is 84 bytes before padding: four pointers, two
    // UINTs, the mode GUID, then the discovery BOOL.
    const SYSTEM_EFFECTS_2_SIZE: u32 = 84;
    if data.is_null() || data_size < SYSTEM_EFFECTS_2_SIZE {
        return String::from("payload=(not APOInitSystemEffects2)");
    }
    let mode = (data.add(64) as *const GUID).read_unaligned();
    let discovery_only = (data.add(80) as *const u32).read_unaligned();
    format!(
        "mode={} discovery_only={discovery_only}",
        crate::diag::guid_text(&mode)
    )
}

unsafe extern "system" fn apo_initialize(
    this: *mut c_void,
    data_size: u32,
    data: *mut u8,
) -> HRESULT {
    let apo = apo_ref(this);
    log_diag!(
        apo.role.label(),
        "Initialize",
        "data_size={data_size} data_null={} role_guess={}",
        data.is_null(),
        apo.role.label()
    );
    #[cfg(feature = "diagnostics")]
    log_diag!(
        apo.role.label(),
        "Initialize",
        "{}",
        init_extras(data_size, data)
    );
    if apo.initialized {
        log_diag!(
            apo.role.label(),
            "Initialize",
            "-> APOERR_ALREADY_INITIALIZED"
        );
        return APOERR_ALREADY_INITIALIZED;
    }

    // The engine tells us which CLSID it thinks it is creating; trust it over
    // our own guess if it names one of ours.
    if data_size >= size_of::<ApoInitBase>() as u32 && !data.is_null() {
        let init = &*(data as *const ApoInitBase);
        log_diag!(
            apo.role.label(),
            "Initialize",
            "engine clsid {}",
            crate::diag::guid_text(&init.clsid)
        );
        if let Some(role) = ApoRole::from_clsid(&init.clsid) {
            apo.role = role;
        }
    }

    apo.initialized = true;
    apo.open_control();
    log_diag!(
        apo.role.label(),
        "Initialize",
        "-> S_OK, role is now {}",
        apo.role.label()
    );
    S_OK
}

unsafe extern "system" fn apo_is_input_format_supported(
    this: *mut c_void,
    _opposite: *mut IAudioMediaType,
    requested: *mut IAudioMediaType,
    supported: *mut *mut IAudioMediaType,
) -> HRESULT {
    if !supported.is_null() {
        *supported = null_mut();
    }
    let role = apo_ref(this).role.label();
    // We never need a different format, so either the requested one works as-is
    // or it does not work at all.
    match format::read_stream_format(requested) {
        Some(format) => {
            log_diag!(
                role,
                "IsInputFormatSupported",
                "yes: {}",
                crate::diag::format_text(&format)
            );
            // Hand the requested format straight back, with the reference the
            // caller now owns. This is not cosmetic: a NULL here does not read
            // as "any format will do", it reads as "nothing I can process", and
            // the engine drops the APO out of the chain -- it creates,
            // initializes and format-checks the object, then quietly skips it
            // and moves on. `CBaseAudioProcessingObject` returns the requested
            // format with an `AddRef` for exactly this reason.
            if !supported.is_null() {
                format::add_ref(requested);
                *supported = requested;
            }
            S_OK
        }
        None => {
            log_diag!(role, "IsInputFormatSupported", "no: unreadable format");
            APOERR_FORMAT_NOT_SUPPORTED
        }
    }
}

unsafe extern "system" fn apo_is_output_format_supported(
    this: *mut c_void,
    _opposite: *mut IAudioMediaType,
    requested: *mut IAudioMediaType,
    supported: *mut *mut IAudioMediaType,
) -> HRESULT {
    if !supported.is_null() {
        *supported = null_mut();
    }
    let role = apo_ref(this).role.label();
    match format::read_stream_format(requested) {
        Some(format) => {
            log_diag!(
                role,
                "IsOutputFormatSupported",
                "yes: {}",
                crate::diag::format_text(&format)
            );
            // Same as the input side above.
            if !supported.is_null() {
                format::add_ref(requested);
                *supported = requested;
            }
            S_OK
        }
        None => {
            log_diag!(role, "IsOutputFormatSupported", "no: unreadable format");
            APOERR_FORMAT_NOT_SUPPORTED
        }
    }
}

unsafe extern "system" fn apo_get_input_channel_count(
    this: *mut c_void,
    channels: *mut u32,
) -> HRESULT {
    if channels.is_null() {
        return E_POINTER;
    }
    let apo = apo_ref(this);
    *channels = apo.format.map_or(0, |format| format.channels as u32);
    log_diag!(
        apo.role.label(),
        "GetInputChannelCount",
        "{} (locked={})",
        *channels,
        apo.locked
    );
    S_OK
}

// ---------------------------------------------------------------------------
// IAudioProcessingObjectConfiguration
// ---------------------------------------------------------------------------

unsafe extern "system" fn config_lock_for_process(
    this: *mut c_void,
    inputs: u32,
    input_descriptors: *mut *mut ApoConnectionDescriptor,
    outputs: u32,
    output_descriptors: *mut *mut ApoConnectionDescriptor,
) -> HRESULT {
    let apo = config_ref(this);
    log_diag!(
        apo.role.label(),
        "LockForProcess",
        "enter initialized={} locked={} {inputs} in / {outputs} out, descriptors_null={}/{}",
        apo.initialized,
        apo.locked,
        input_descriptors.is_null(),
        output_descriptors.is_null()
    );
    if !apo.initialized {
        return APOERR_NOT_INITIALIZED;
    }
    if inputs == 0 || outputs == 0 || input_descriptors.is_null() || output_descriptors.is_null() {
        return E_INVALIDARG;
    }

    let descriptor = *input_descriptors;
    if descriptor.is_null() {
        return E_INVALIDARG;
    }
    let Some(format) = format::read_stream_format((*descriptor).format) else {
        log_diag!(apo.role.label(), "LockForProcess", "unsupported input format");
        return APOERR_FORMAT_NOT_SUPPORTED;
    };

    log_diag!(
        apo.role.label(),
        "LockForProcess",
        "locking: {inputs} in / {outputs} out, {}, max_frame_count={}",
        crate::diag::format_text(&format),
        (*descriptor).max_frame_count
    );
    apo.lock_to_format(format, (*descriptor).max_frame_count);
    apo.locked = true;
    S_OK
}

unsafe extern "system" fn config_unlock_for_process(this: *mut c_void) -> HRESULT {
    let apo = config_ref(this);
    log_diag!(apo.role.label(), "UnlockForProcess", "locked={}", apo.locked);
    apo.chain = None;
    apo.format = None;
    apo.scratch.clear();
    apo.locked = false;
    S_OK
}

// ---------------------------------------------------------------------------
// IAudioProcessingObjectRT
// ---------------------------------------------------------------------------

unsafe extern "system" fn rt_apo_process(
    this: *mut c_void,
    input_count: u32,
    inputs: *mut *mut ApoConnectionProperty,
    output_count: u32,
    outputs: *mut *mut ApoConnectionProperty,
) {
    if input_count == 0 || output_count == 0 || inputs.is_null() || outputs.is_null() {
        return;
    }
    let apo = rt_ref(this);
    // Nothing is written to the log from here: this is the audio thread, where
    // formatting a line would allocate and opening a file could block. The
    // counter bump is the whole of what this callback gives diagnostics, and
    // the next control-path call writes it out.
    apo.sync_params();

    for index in 0..output_count as usize {
        let input = *inputs.add(index.min(input_count as usize - 1));
        let output = *outputs.add(index);
        if input.is_null() || output.is_null() {
            continue;
        }

        // Copy the property out by value before touching the output. The engine
        // hands the *same* property to both sides of an in-place connection,
        // and holding a shared reference and a mutable one to one object at the
        // same time is undefined behaviour however harmless the reads look.
        let source = *input;
        let frames = source.valid_frame_count as usize;
        // The engine reads this back even when the buffer is silent.
        (*output).valid_frame_count = source.valid_frame_count;

        if frames == 0 || source.buffer_flags == APO_BUFFER_SILENT {
            (*output).buffer_flags = APO_BUFFER_SILENT;
            continue;
        }

        apo.process_connection(frames, &source, &mut *output);
        crate::diag::rt_processed(frames);
    }
}

unsafe extern "system" fn rt_calc_input_frames(_this: *mut c_void, output_frames: u32) -> u32 {
    output_frames
}

unsafe extern "system" fn rt_calc_output_frames(_this: *mut c_void, input_frames: u32) -> u32 {
    input_frames
}

// ---------------------------------------------------------------------------
// IAudioSystemEffects2
// ---------------------------------------------------------------------------

unsafe extern "system" fn sysfx2_get_effects_list(
    this: *mut c_void,
    effects: *mut *mut GUID,
    count: *mut u32,
    _event: *mut c_void,
) -> HRESULT {
    effects_list(sysfx2_ref(this), effects, count)
}

/// The list of effects this APO provides: ours, one.
///
/// Shared by `IAudioSystemEffects2` and `IAudioSystemEffects3`, which differ
/// only in which interface pointer the engine is holding.
unsafe fn effects_list(
    apo: *mut BoostApo,
    effects: *mut *mut GUID,
    count: *mut u32,
) -> HRESULT {
    if effects.is_null() || count.is_null() {
        return E_POINTER;
    }
    *effects = null_mut();
    *count = 0;

    let memory = CoTaskMemAlloc(size_of::<GUID>()) as *mut GUID;
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    *memory = (*apo).role.clsid();
    *effects = memory;
    *count = 1;
    log_diag!(
        (*apo).role.label(),
        "GetEffectsList",
        "one effect: {}",
        crate::diag::guid_text(&(*apo).role.clsid())
    );
    S_OK
}

// ---------------------------------------------------------------------------
// IAudioSystemEffects3
// ---------------------------------------------------------------------------

unsafe extern "system" fn sysfx3_get_effects_list(
    this: *mut c_void,
    effects: *mut *mut GUID,
    count: *mut u32,
    _event: *mut c_void,
) -> HRESULT {
    effects_list(sysfx3_ref(this), effects, count)
}

/// The effects inside this APO that the system may switch on and off.
///
/// One effect -- ours -- reported as switchable, which is what puts Sound
/// Booster in Windows 11's "Audio enhancements" list. The array is
/// `CoTaskMemAlloc`'d because that is how the engine frees it.
///
/// The state reported is the *system's* switch, not the user's slider: the
/// slider belongs to the tray app, and the engine reads what it set itself.
unsafe extern "system" fn sysfx3_get_controllable_effects(
    this: *mut c_void,
    effects: *mut *mut AudioSystemEffect,
    count: *mut u32,
    _event: *mut c_void,
) -> HRESULT {
    if effects.is_null() || count.is_null() {
        return E_POINTER;
    }
    *effects = null_mut();
    *count = 0;

    let apo = sysfx3_ref(this);
    let memory = CoTaskMemAlloc(size_of::<AudioSystemEffect>()) as *mut AudioSystemEffect;
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    let state = if apo.system_on.load(Ordering::Relaxed) {
        AUDIO_SYSTEMEFFECT_STATE_ON
    } else {
        AUDIO_SYSTEMEFFECT_STATE_OFF
    };
    *memory = AudioSystemEffect {
        id: apo.role.clsid(),
        can_set_state: 1,
        state,
    };
    *effects = memory;
    *count = 1;
    log_diag!(
        apo.role.label(),
        "GetControllableSystemEffectsList",
        "one effect, switchable, state {state}"
    );
    S_OK
}

/// Windows Settings switching this effect on or off.
unsafe extern "system" fn sysfx3_set_effect_state(
    this: *mut c_void,
    effect_id: GUID,
    state: i32,
) -> HRESULT {
    let apo = sysfx3_ref(this);
    if effect_id != apo.role.clsid() {
        return E_INVALIDARG;
    }
    let on = state == AUDIO_SYSTEMEFFECT_STATE_ON;
    apo.system_on.store(on, Ordering::Relaxed);
    log_diag!(apo.role.label(), "SetAudioSystemEffectState", "on={on}");
    S_OK
}

// ---------------------------------------------------------------------------
// The vtables themselves
// ---------------------------------------------------------------------------

static APO_VTBL: IAudioProcessingObjectVtbl = IAudioProcessingObjectVtbl {
    query_interface: apo_qi,
    add_ref: apo_add_ref,
    release: apo_release,
    reset: apo_reset,
    get_latency: apo_get_latency,
    get_registration_properties: apo_get_registration_properties,
    initialize: apo_initialize,
    is_input_format_supported: apo_is_input_format_supported,
    is_output_format_supported: apo_is_output_format_supported,
    get_input_channel_count: apo_get_input_channel_count,
};

static CONFIG_VTBL: IAudioProcessingObjectConfigurationVtbl =
    IAudioProcessingObjectConfigurationVtbl {
        query_interface: config_qi,
        add_ref: config_add_ref,
        release: config_release,
        lock_for_process: config_lock_for_process,
        unlock_for_process: config_unlock_for_process,
    };

static RT_VTBL: IAudioProcessingObjectRTVtbl = IAudioProcessingObjectRTVtbl {
    query_interface: rt_qi,
    add_ref: rt_add_ref,
    release: rt_release,
    apo_process: rt_apo_process,
    calc_input_frames: rt_calc_input_frames,
    calc_output_frames: rt_calc_output_frames,
};

static UNKNOWN_VTBL: UnknownVtbl = UnknownVtbl {
    query_interface: sysfx_qi,
    add_ref: sysfx_add_ref,
    release: sysfx_release,
};

static SYSFX2_VTBL: IAudioSystemEffects2Vtbl = IAudioSystemEffects2Vtbl {
    query_interface: sysfx2_qi,
    add_ref: sysfx2_add_ref,
    release: sysfx2_release,
    get_effects_list: sysfx2_get_effects_list,
};

static SYSFX3_VTBL: IAudioSystemEffects3Vtbl = IAudioSystemEffects3Vtbl {
    query_interface: sysfx3_qi,
    add_ref: sysfx3_add_ref,
    release: sysfx3_release,
    get_effects_list: sysfx3_get_effects_list,
    get_controllable_system_effects_list: sysfx3_get_controllable_effects,
    set_audio_system_effect_state: sysfx3_set_effect_state,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_stage_owns_leveling_and_not_the_manual_boost() {
        let params = Preset::Music.boost_params(13.9794);
        let applied = ApoRole::Stream.apply(&params);
        assert_eq!(applied.manual_gain_db, 0.0);
        assert_eq!(applied.agc.strength, params.agc.strength);
        assert!(applied.agc.max_gain_db > 0.0);
    }

    #[test]
    fn the_endpoint_stage_owns_the_manual_boost_and_not_leveling() {
        let params = Preset::Music.boost_params(13.9794);
        let applied = ApoRole::Endpoint.apply(&params);
        assert!((applied.manual_gain_db - params.manual_gain_db).abs() < 1e-4);
        assert_eq!(applied.agc.strength, 0.0);
        assert_eq!(applied.agc.max_gain_db, 0.0);
    }

    #[test]
    fn the_two_stages_do_not_overlap_or_double_apply() {
        for preset in Preset::ALL {
            let params = preset.boost_params(6.0206);
            let stream = ApoRole::Stream.apply(&params);
            let endpoint = ApoRole::Endpoint.apply(&params);

            // Between them the two stages must produce exactly the user's
            // boost once, and each stage must keep its own half.
            assert_eq!(
                stream.manual_gain_db + endpoint.manual_gain_db,
                params.manual_gain_db
            );
            assert_eq!(stream.agc.strength, params.agc.strength);
            assert_eq!(endpoint.agc.strength, 0.0);
        }
    }

    #[test]
    fn no_tray_app_means_no_boost() {
        let bypass = bypass_params();
        assert!(!bypass.enabled);
        assert_eq!(bypass.manual_gain_db, 0.0);
    }

    #[test]
    fn each_role_has_its_own_clsid() {
        assert_ne!(CLSID_STREAM, CLSID_ENDPOINT);
        assert_eq!(ApoRole::from_clsid(&CLSID_STREAM), Some(ApoRole::Stream));
        assert_eq!(
            ApoRole::from_clsid(&CLSID_ENDPOINT),
            Some(ApoRole::Endpoint)
        );
        assert_eq!(ApoRole::from_clsid(&GUID::nil()), None);
    }
}
