//! Control block shared between the tray app (writer) and the APO (reader).
//!
//! The APO lives inside `audiodg.exe` and must never block, allocate or call
//! COM while audio is flowing. So the UI does not "call" the APO at all: the
//! tray app writes plain numbers into a named shared-memory section and the
//! APO reads them once per buffer. Parameters are therefore always POD.
//!
//! Consistency uses a seqlock (a writer counter plus a retry loop on the
//! reader). The writer is a single process, the reader may be several APO
//! instances -- one per stream -- so the block is read-only for them.

use std::sync::atomic::{AtomicU32, Ordering};

use sb_core::agc::AgcParams;
use sb_core::chain::BoostParams;
use sb_core::limiter::LimiterParams;
use sb_core::params::{percent_to_gain_db, Preset, MAX_PERCENT, MIN_PERCENT};

/// Machine-wide, and it has to be: `audiodg.exe` runs in **session 0** while the
/// tray app runs in the user's session, and a `Local\` section is per-session,
/// so each side would silently get its own private copy and the slider would do
/// nothing (measured on this machine; see `docs/risks.md`).
///
/// Creating an object in `Global\` needs `SeCreateGlobalPrivilege`, which the
/// interactive user does not have but `LOCAL SERVICE` -- and therefore
/// `audiodg.exe` -- does. So the **APO creates the section** and the tray app
/// only opens it, retrying until the audio engine has started. The DACL on the
/// section has to let the user in, which is why the creator passes
/// [`mapping::CONTROL_SDDL`].
pub const CONTROL_NAME: &str = r"Global\SoundBoosterControl_v1";

/// Session-local fallback, used only when the caller has no
/// `SeCreateGlobalPrivilege` -- that is, a non-elevated test or developer
/// process. Both sides of a test run in one session, so they find each other.
pub const CONTROL_NAME_LOCAL: &str = r"Local\SoundBoosterControl_v1";

/// `"SBOO"` read as a little-endian u32.
pub const MAGIC: u32 = u32::from_le_bytes(*b"SBOO");

/// Bumped whenever the layout changes; readers refuse other versions.
pub const VERSION: u32 = 1;

/// The complete parameter set the audio path acts on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControlSnapshot {
    pub enabled: bool,
    pub preset: Preset,
    /// Slider value as a percentage, 100..=500.
    pub manual_percent: u32,
    pub params: BoostParams,
}

impl Default for ControlSnapshot {
    fn default() -> Self {
        Self::from_ui(Preset::Music, 250, None)
    }
}

impl ControlSnapshot {
    /// Build a snapshot the way the UI does: pick a preset, place the
    /// slider, optionally override the leveling strength.
    pub fn from_ui(preset: Preset, manual_percent: u32, strength: Option<f32>) -> Self {
        let percent = manual_percent.clamp(MIN_PERCENT as u32, MAX_PERCENT as u32);
        let mut params = preset.boost_params(percent_to_gain_db(percent as f32));
        if let Some(strength) = strength {
            params.agc.strength = strength.clamp(0.0, 1.0);
        }
        Self {
            enabled: preset != Preset::Off,
            preset,
            manual_percent: percent,
            params,
        }
    }
}

/// Fixed-layout shared memory. All fields are atomics so the reader never
/// observes a torn value even without the seqlock.
#[repr(C)]
pub struct ControlBlock {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    /// Seqlock counter: odd while [`ControlBlock::write`] is in progress.
    pub seq: AtomicU32,
    pub enabled: AtomicU32,
    pub preset: AtomicU32,
    pub manual_percent: AtomicU32,
    pub strength_milli: AtomicU32,
    pub target_mdb: AtomicU32,
    pub max_gain_mdb: AtomicU32,
    pub min_gain_mdb: AtomicU32,
    pub gate_mdb: AtomicU32,
    pub attack_ms: AtomicU32,
    pub release_ms: AtomicU32,
    pub control_block: AtomicU32,
    pub ceiling_mdb: AtomicU32,
    pub lookahead_us: AtomicU32,
    pub limiter_release_ms: AtomicU32,
    /// Kept for future fields; also pads the block to a cache-line multiple.
    pub reserved: [AtomicU32; 6],
}

impl ControlBlock {
    /// Const constructor for tests and for the tray app's own staging block.
    #[allow(clippy::declare_interior_mutable_const)]
    pub const fn new() -> Self {
        const Z: AtomicU32 = AtomicU32::new(0);
        Self {
            magic: AtomicU32::new(MAGIC),
            version: AtomicU32::new(VERSION),
            seq: AtomicU32::new(0),
            enabled: Z,
            preset: Z,
            manual_percent: Z,
            strength_milli: Z,
            target_mdb: Z,
            max_gain_mdb: Z,
            min_gain_mdb: Z,
            gate_mdb: Z,
            attack_ms: Z,
            release_ms: Z,
            control_block: Z,
            ceiling_mdb: Z,
            lookahead_us: Z,
            limiter_release_ms: Z,
            reserved: [Z; 6],
        }
    }

    /// Publish the first snapshot and make the block readable.
    ///
    /// **Only the UI calls this.** The APO creates the section if it is missing
    /// but never stamps it, so a section nobody has published to reads as
    /// incompatible and the APO stays a true bypass. That is what keeps a
    /// registered-but-UI-less install from boosting anything.
    pub fn initialize(&self) {
        self.write(&ControlSnapshot::default());
        self.stamp();
    }

    /// Make this block readable by the APO.
    ///
    /// Magic and version go last, on purpose: a publisher that wants to avoid
    /// even one buffer of the *default* snapshot writes the real one first and
    /// stamps afterwards. Until the stamp lands, readers treat the block as
    /// "no UI yet" and pass audio through, so audio is never boosted by a
    /// snapshot the user did not choose.
    pub fn stamp(&self) {
        self.magic.store(MAGIC, Ordering::Relaxed);
        self.version.store(VERSION, Ordering::Relaxed);
    }

    /// True if this block was created by a compatible build.
    pub fn is_compatible(&self) -> bool {
        self.magic.load(Ordering::Relaxed) == MAGIC
            && self.version.load(Ordering::Relaxed) == VERSION
    }

    /// Publish a snapshot. Single writer only.
    pub fn write(&self, snapshot: &ControlSnapshot) {
        let base = self.seq.load(Ordering::Relaxed) & !1;
        self.seq.store(base.wrapping_add(1), Ordering::Release); // odd: writing

        let params = snapshot.params;
        store_bool(&self.enabled, snapshot.enabled);
        self.preset
            .store(snapshot.preset.as_u32(), Ordering::Relaxed);
        self.manual_percent
            .store(snapshot.manual_percent, Ordering::Relaxed);
        self.strength_milli.store(
            (params.agc.strength.clamp(0.0, 1.0) * 1000.0).round() as u32,
            Ordering::Relaxed,
        );
        self.target_mdb
            .store(to_mdb(params.agc.target_db), Ordering::Relaxed);
        self.max_gain_mdb
            .store(to_mdb(params.agc.max_gain_db), Ordering::Relaxed);
        self.min_gain_mdb
            .store(to_mdb(params.agc.min_gain_db), Ordering::Relaxed);
        self.gate_mdb
            .store(to_mdb(params.agc.gate_db), Ordering::Relaxed);
        self.attack_ms
            .store(to_millis(params.agc.attack_ms), Ordering::Relaxed);
        self.release_ms
            .store(to_millis(params.agc.release_ms), Ordering::Relaxed);
        self.control_block
            .store(params.agc.control_block as u32, Ordering::Relaxed);
        self.ceiling_mdb
            .store(to_mdb(params.limiter.ceiling_db), Ordering::Relaxed);
        self.lookahead_us.store(
            (params.limiter.lookahead_ms * 1000.0).round().max(0.0) as u32,
            Ordering::Relaxed,
        );
        self.limiter_release_ms
            .store(to_millis(params.limiter.release_ms), Ordering::Relaxed);

        self.seq.store(base.wrapping_add(2), Ordering::Release); // even: done
    }

    /// Read a consistent snapshot. Safe to call from the audio thread: it
    /// only performs relaxed/acquire loads and a spin loop.
    pub fn read(&self) -> ControlSnapshot {
        for _ in 0..128 {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let snapshot = self.read_fields(before);
            if self.seq.load(Ordering::Acquire) == before {
                return snapshot;
            }
        }
        // The writer is stuck or extremely busy; take the last consistent-ish
        // view rather than spinning inside audiodg.exe.
        self.read_fields(self.seq.load(Ordering::Acquire))
    }

    fn read_fields(&self, seq: u32) -> ControlSnapshot {
        let _ = seq;
        let preset = Preset::from_u32(self.preset.load(Ordering::Relaxed));
        ControlSnapshot {
            enabled: self.enabled.load(Ordering::Relaxed) != 0,
            preset,
            manual_percent: self.manual_percent.load(Ordering::Relaxed),
            params: BoostParams {
                enabled: self.enabled.load(Ordering::Relaxed) != 0,
                manual_gain_db: percent_to_gain_db(
                    self.manual_percent.load(Ordering::Relaxed) as f32
                ),
                agc: AgcParams {
                    target_db: from_mdb(self.target_mdb.load(Ordering::Relaxed)),
                    max_gain_db: from_mdb(self.max_gain_mdb.load(Ordering::Relaxed)),
                    min_gain_db: from_mdb(self.min_gain_mdb.load(Ordering::Relaxed)),
                    strength: self.strength_milli.load(Ordering::Relaxed) as f32 / 1000.0,
                    gate_db: from_mdb(self.gate_mdb.load(Ordering::Relaxed)),
                    attack_ms: self.attack_ms.load(Ordering::Relaxed) as f32,
                    release_ms: self.release_ms.load(Ordering::Relaxed) as f32,
                    control_block: (self.control_block.load(Ordering::Relaxed) as usize).max(1),
                },
                limiter: LimiterParams {
                    ceiling_db: from_mdb(self.ceiling_mdb.load(Ordering::Relaxed)),
                    lookahead_ms: self.lookahead_us.load(Ordering::Relaxed) as f32 / 1000.0,
                    release_ms: self.limiter_release_ms.load(Ordering::Relaxed) as f32,
                },
            },
        }
    }
}

impl Default for ControlBlock {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn store_bool(target: &AtomicU32, value: bool) {
    target.store(u32::from(value), Ordering::Relaxed);
}

#[inline]
fn to_mdb(db: f32) -> u32 {
    (db * 1000.0).round() as i32 as u32
}

#[inline]
fn from_mdb(value: u32) -> f32 {
    (value as i32) as f32 / 1000.0
}

#[inline]
fn to_millis(value: f32) -> u32 {
    value.clamp(0.0, 60_000.0).round() as u32
}

/// The named section as seen by each side.
#[cfg(windows)]
pub mod mapping {
    use std::io;
    use std::mem::size_of;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, ERROR_ACCESS_DENIED, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
        MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    };

    use super::{ControlBlock, CONTROL_NAME, CONTROL_NAME_LOCAL};

    /// Who may attach to the control section.
    ///
    /// `SY` local system, `LS` local service (the APO, and the audio service
    /// that starts it), `AU` authenticated users (the tray app, in whichever
    /// session it happens to run). There is nothing secret in the block -- it is
    /// a volume knob -- and `AU` is used rather than the interactive-user alias
    /// so this still works over Remote Desktop, where the interactive SID is
    /// not always what you would expect.
    pub const CONTROL_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;AU)";

    /// Owns the file mapping and the mapped view.
    pub struct MappedControl {
        mapping: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    // The view is only ever accessed through atomics.
    unsafe impl Send for MappedControl {}
    unsafe impl Sync for MappedControl {}

    impl MappedControl {
        /// Create the section, or attach to it if it already exists.
        ///
        /// This is the **APO's** entry point: `audiodg.exe` runs as
        /// `LOCAL SERVICE` and can create a `Global\` object, so whoever gets
        /// there first creates it and everyone else attaches. The section
        /// survives an `audiodg.exe` restart for as long as any other process
        /// (the tray app) still holds a handle.
        ///
        /// A freshly created section is zero-filled, i.e. *not* stamped as
        /// compatible, so creating it does not make the APO do anything. Only
        /// [`super::ControlBlock::initialize`] does, and only the UI calls that.
        pub fn create_or_open() -> io::Result<Self> {
            match Self::create() {
                Ok(mapped) => Ok(mapped),
                Err(_) => Self::open(),
            }
        }

        /// Create a fresh section, or attach to an existing one with the same
        /// name (which is what keeps repeated instantiations cheap).
        ///
        /// Falls back to the session-local name when this process lacks
        /// `SeCreateGlobalPrivilege`, which is the normal case for a test or a
        /// developer process, and never the case inside `audiodg.exe`.
        pub fn create() -> io::Result<Self> {
            match Self::create_named(CONTROL_NAME) {
                Ok(mapped) => Ok(mapped),
                Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                    Self::create_named(CONTROL_NAME_LOCAL)
                }
                Err(error) => Err(error),
            }
        }

        /// Attach to a section that already exists.
        ///
        /// This is the **tray app's** entry point. It deliberately cannot
        /// create anything: a UI process that silently made its own private
        /// section would look like it worked while the audio engine saw
        /// nothing. It should report the failure and retry instead.
        pub fn open() -> io::Result<Self> {
            match Self::open_named(CONTROL_NAME) {
                Ok(mapped) => Ok(mapped),
                // Only reachable for a same-session test or dev process that
                // could not create the machine-wide one either.
                Err(_) => Self::open_named(CONTROL_NAME_LOCAL),
            }
        }

        fn create_named(name: &str) -> io::Result<Self> {
            let security = SecurityAttributes::for_control_section();
            let wide_name = wide(name);
            unsafe {
                let mapping = CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    security
                        .as_ref()
                        .map_or(null(), |security| &security.attributes),
                    PAGE_READWRITE,
                    0,
                    size_of::<ControlBlock>() as u32,
                    wide_name.as_ptr(),
                );
                if mapping.is_null() {
                    return Err(io::Error::last_os_error());
                }
                Self::map(mapping)
            }
        }

        fn open_named(name: &str) -> io::Result<Self> {
            let wide_name = wide(name);
            unsafe {
                let mapping = OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr());
                if mapping.is_null() {
                    return Err(io::Error::last_os_error());
                }
                Self::map(mapping)
            }
        }

        unsafe fn map(mapping: HANDLE) -> io::Result<Self> {
            let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if view.Value.is_null() {
                let error = io::Error::last_os_error();
                CloseHandle(mapping);
                return Err(error);
            }
            Ok(Self { mapping, view })
        }

        pub fn block(&self) -> &ControlBlock {
            unsafe { &*(self.view.Value as *const ControlBlock) }
        }
    }

    impl Drop for MappedControl {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.mapping);
            }
        }
    }

    /// A security descriptor parsed from SDDL, alive only long enough to create
    /// the section.
    struct SecurityAttributes {
        attributes: SECURITY_ATTRIBUTES,
        descriptor: PSECURITY_DESCRIPTOR,
    }

    impl SecurityAttributes {
        /// `None` if the SDDL is malformed, in which case the section is created
        /// with the caller's default DACL -- which is the right thing to do
        /// anyway, because failing to create the section would be worse.
        fn for_control_section() -> Option<Self> {
            let sddl = wide(CONTROL_SDDL);
            let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
            let converted = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            };
            if converted == 0 || descriptor.is_null() {
                return None;
            }
            Some(Self {
                attributes: SECURITY_ATTRIBUTES {
                    nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                },
                descriptor,
            })
        }
    }

    impl Drop for SecurityAttributes {
        fn drop(&mut self) {
            // `ConvertStringSecurityDescriptorToSecurityDescriptorW` allocates
            // with LocalAlloc; LocalFree is the matching deallocator.
            unsafe { LocalFree(self.descriptor) };
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The control section is machine-wide, so the tests that create it must
    /// not overlap each other.
    ///
    /// These run without `SeCreateGlobalPrivilege` (unless you happen to run
    /// `cargo test` from an elevated prompt), so they exercise the fallback to
    /// the session-local name. That is deliberate: the fallback is the only
    /// reason both sides can meet inside one test process.
    static SECTION: Mutex<()> = Mutex::new(());

    fn section_lock() -> std::sync::MutexGuard<'static, ()> {
        SECTION.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn layout_is_stable_and_word_aligned() {
        assert_eq!(std::mem::align_of::<ControlBlock>(), 4);
        assert_eq!(std::mem::size_of::<ControlBlock>() % 4, 0);
        assert!(std::mem::size_of::<ControlBlock>() <= 256);
    }

    #[test]
    fn fresh_block_is_compatible_and_has_a_default_snapshot() {
        let block = ControlBlock::new();
        assert!(block.is_compatible());
        block.initialize();
        let snapshot = block.read();
        assert!(snapshot.enabled);
        assert_eq!(snapshot.preset, Preset::Music);
        assert_eq!(snapshot.manual_percent, 250);
    }

    #[test]
    fn snapshots_round_trip_through_the_block() {
        for preset in Preset::ALL {
            for percent in [100u32, 175, 300, 500] {
                let wanted = ControlSnapshot::from_ui(preset, percent, Some(0.75));
                let block = ControlBlock::new();
                block.write(&wanted);
                let got = block.read();

                assert_eq!(got.enabled, wanted.enabled);
                assert_eq!(got.preset, preset);
                assert_eq!(got.manual_percent, percent);
                assert!((got.params.manual_gain_db - wanted.params.manual_gain_db).abs() < 1e-3);
                assert!((got.params.agc.strength - 0.75).abs() < 1e-3);
                assert!((got.params.agc.target_db - wanted.params.agc.target_db).abs() < 1e-3);
                assert!((got.params.agc.max_gain_db - wanted.params.agc.max_gain_db).abs() < 1e-3);
                assert!((got.params.agc.gate_db - wanted.params.agc.gate_db).abs() < 1e-3);
                assert!((got.params.agc.attack_ms - wanted.params.agc.attack_ms).abs() < 1e-3);
                assert!(
                    (got.params.limiter.ceiling_db - wanted.params.limiter.ceiling_db).abs() < 1e-3
                );
                assert!(
                    (got.params.limiter.lookahead_ms - wanted.params.limiter.lookahead_ms).abs()
                        < 1e-3
                );
                assert_eq!(
                    got.params.agc.control_block,
                    wanted.params.agc.control_block
                );
            }
        }
    }

    #[test]
    fn slider_is_clamped_to_the_supported_range() {
        let low = ControlSnapshot::from_ui(Preset::Music, 10, None);
        assert_eq!(low.manual_percent, MIN_PERCENT as u32);
        let high = ControlSnapshot::from_ui(Preset::Music, 9_999, None);
        assert_eq!(high.manual_percent, MAX_PERCENT as u32);
    }

    #[test]
    fn off_preset_disables_the_chain() {
        let snapshot = ControlSnapshot::from_ui(Preset::Off, 300, None);
        assert!(!snapshot.enabled);
        assert!(!snapshot.params.enabled);
    }

    #[cfg(windows)]
    #[test]
    fn a_second_view_of_the_section_sees_what_the_publisher_wrote() {
        // The APO only ever calls `open`; the UI is the side that stamps and
        // writes. This is that handshake, over two independent views.
        let _guard = section_lock();
        let published = mapping::MappedControl::create().expect("create section");
        published.block().initialize();
        let wanted = ControlSnapshot::from_ui(Preset::Night, 420, Some(0.5));
        published.block().write(&wanted);

        let attached = mapping::MappedControl::open().expect("attach to section");
        assert!(attached.block().is_compatible());
        let got = attached.block().read();
        assert_eq!(got.preset, Preset::Night);
        assert_eq!(got.manual_percent, 420);
        assert!((got.params.agc.strength - 0.5).abs() < 1e-3);
    }

    #[cfg(windows)]
    #[test]
    fn a_section_nobody_published_to_is_not_compatible() {
        // The APO creates the section when it gets there first, so a freshly
        // created one has to read as "no UI yet" and leave the APO bypassed.
        // Only `initialize` -- which only the UI calls -- changes that.
        let _guard = section_lock();
        let created = mapping::MappedControl::create().expect("create section");
        assert!(!created.block().is_compatible());
        assert!(!created.block().read().enabled || created.block().read().manual_percent == 0);
    }
}
