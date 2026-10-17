//! The Sound Booster Audio Processing Object.
//!
//! Built as `sb_apo.dll` and loaded by `audiodg.exe` once the CLSIDs in
//! [`object`] are registered against an audio endpoint. See
//! `docs/reference-implementation.md` for the registry contract and
//! `docs/risks.md` for what still has to be proven on real hardware.
//!
//! Everything here is original code; the audio engine ABI in [`abi`] is
//! transcribed from the Windows SDK headers, which is the only way to write an
//! APO in a language whose toolchain does not ship those bindings.

#![cfg(windows)]

pub mod abi;
pub mod diag;
pub mod factory;
pub mod format;
pub mod object;
pub mod registration;

/// Write a diagnostic line, but only when the `diagnostics` feature is on.
///
/// A macro rather than a function so that a normal build does not build the
/// string at all -- the call is not made. The arguments are still type-checked,
/// which is also what keeps a variable that only appears in a log line from
/// looking unused.
macro_rules! log_diag {
    ($role:expr, $event:expr, $($arg:tt)*) => {{
        #[cfg(feature = "diagnostics")]
        {
            $crate::diag::log($role, $event, &::std::format!($($arg)*));
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            if false {
                $crate::diag::log($role, $event, &::std::format!($($arg)*));
            }
        }
    }};
}
pub(crate) use log_diag;

pub use abi::{GUID, HRESULT};
pub use object::{ApoRole, BoostApo, CLSID_ENDPOINT, CLSID_STREAM};
pub use registration::{ApoRegistration, ALL as APO_REGISTRATIONS, APO_STORE_KEY};

use core::ffi::c_void;
use core::ptr::null_mut;

use abi::{succeeded, CLASS_E_CLASSNOTAVAILABLE, E_OUTOFMEMORY, E_POINTER, S_FALSE};

/// COM entry point. `audiodg.exe` (via `CoCreateInstance`) is the only caller.
///
/// # Safety
/// Standard `IClassFactory` contract: all three pointers must be valid.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();

    log_diag!(
        "dll",
        "DllGetClassObject",
        "clsid {} iid {}",
        crate::diag::guid_text(&*rclsid),
        crate::diag::guid_text(&*riid)
    );

    let Some(role) = ApoRole::from_clsid(&*rclsid) else {
        log_diag!("dll", "DllGetClassObject", "unknown CLSID, refusing");
        return CLASS_E_CLASSNOTAVAILABLE;
    };

    let factory = factory::ClassFactory::create(role);
    if factory.is_null() {
        return E_OUTOFMEMORY;
    }

    let result = factory::query_interface(factory, riid, ppv);
    if !succeeded(result) {
        // The factory was created with no references outstanding, so the failed
        // `QueryInterface` left it with nothing holding it.
        factory::destroy(factory);
    }
    log_diag!(
        role.label(),
        "DllGetClassObject",
        "factory handed out, result {result:#010X}"
    );
    result
}

/// Always `S_FALSE`: this module stays loaded.
///
/// A booster DLL that unloads itself while the engine still holds interface
/// pointers is a crash waiting to happen, and there is nothing to gain.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
