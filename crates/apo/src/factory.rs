//! `IClassFactory`, so the engine can ask for either APO CLSID.

use core::ffi::c_void;
use core::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::abi::*;
use crate::log_diag;
use crate::object::{self, ApoRole, BoostApo};

#[repr(C)]
pub struct ClassFactory {
    // First field, so the object's address *is* its `IClassFactory` pointer.
    vtbl: *const IClassFactoryVtbl,
    ref_count: AtomicU32,
    role: ApoRole,
}

impl ClassFactory {
    /// A fresh factory, with **no** reference outstanding.
    ///
    /// Like [`BoostApo::create`], the caller's reference comes from the
    /// `QueryInterface` in `DllGetClassObject`; see the comment there.
    pub fn create(role: ApoRole) -> *mut Self {
        Box::into_raw(Box::new(Self {
            vtbl: &FACTORY_VTBL,
            ref_count: AtomicU32::new(0),
            role,
        }))
    }
}

/// Free a factory that never had a reference handed out.
///
/// # Safety
/// `factory` must be live with no references outstanding.
pub unsafe fn destroy(factory: *mut ClassFactory) {
    drop(Box::from_raw(factory));
}

/// # Safety
/// `this` must be a live factory.
pub unsafe fn query_interface(
    factory: *mut ClassFactory,
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
    if iid == IID_IUNKNOWN || iid == IID_ICLASS_FACTORY {
        (*factory).ref_count.fetch_add(1, Ordering::Relaxed);
        *ppv = factory as *mut c_void;
        S_OK
    } else {
        E_NOINTERFACE
    }
}

/// # Safety
/// `this` must be a live factory.
pub unsafe fn release(factory: *mut ClassFactory) -> u32 {
    let previous = (*factory).ref_count.fetch_sub(1, Ordering::AcqRel);
    if previous == 0 {
        (*factory).ref_count.store(0, Ordering::Relaxed);
        return 0;
    }
    let remaining = previous - 1;
    if remaining == 0 {
        drop(Box::from_raw(factory));
    }
    remaining
}

unsafe extern "system" fn factory_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let result = query_interface(this as *mut ClassFactory, riid, ppv);
    log_diag!(
        "factory",
        "QueryInterface",
        "iid {} -> {result:#010X}",
        crate::diag::guid_text(&*riid)
    );
    result
}

unsafe extern "system" fn factory_add_ref(this: *mut c_void) -> ULONG {
    let factory = this as *mut ClassFactory;
    (*factory).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn factory_release(this: *mut c_void) -> ULONG {
    release(this as *mut ClassFactory)
}

unsafe extern "system" fn factory_create_instance(
    this: *mut c_void,
    outer: *mut IUnknown,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    log_diag!(
        "factory",
        "CreateInstance",
        "enter outer_null={} iid {}",
        outer.is_null(),
        crate::diag::guid_text(&*riid)
    );
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();

    // **The engine aggregates APOs.** It passes a non-null controlling unknown
    // and asks for `IID_IUnknown` (measured on this machine: five streams, five
    // of these calls, `outer_null=false`). Refusing that with
    // `CLASS_E_NOAGGREGATION`, which is what a non-aggregatable object must do,
    // made the engine drop the APO in silence -- the DLL stayed loaded, but no
    // instance was ever initialized, so the shared control block was never
    // created and the effect never ran.
    //
    // So we take part in the aggregate properly: the object records the
    // controlling unknown and answers *later* `IID_IUnknown` questions with it,
    // which is COM's identity rule and what the engine's wrapper checks. The one
    // exception is this very call: `CreateInstance` owes the outer a reference to
    // the *inner* object, because that reference is how the outer controls the
    // inner's lifetime.
    let role = (*(this as *mut ClassFactory)).role;
    let apo = if outer.is_null() {
        BoostApo::create(role)
    } else {
        BoostApo::create_aggregated(role, outer as *mut c_void)
    };
    if apo.is_null() {
        return E_OUTOFMEMORY;
    }

    let result = match (!outer.is_null(), riid.is_null()) {
        (true, false) if *riid == IID_IUNKNOWN => object::query_interface_inner_unknown(apo, ppv),
        _ => object::query_interface(apo, riid, ppv),
    };
    if !succeeded(result) {
        // Nothing holds a reference, so this cannot go through `release`.
        object::destroy(apo);
    }
    log_diag!(
        role.label(),
        "CreateInstance",
        "iid {} -> {result:#010X} (outer_null={})",
        crate::diag::guid_text(&*riid),
        outer.is_null()
    );
    result
}

unsafe extern "system" fn factory_lock_server(_this: *mut c_void, _lock: BOOL) -> HRESULT {
    log_diag!("factory", "LockServer", "lock={_lock}");
    S_OK
}

static FACTORY_VTBL: IClassFactoryVtbl = IClassFactoryVtbl {
    query_interface: factory_query_interface,
    add_ref: factory_add_ref,
    release: factory_release,
    create_instance: factory_create_instance,
    lock_server: factory_lock_server,
};
