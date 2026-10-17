//! Reads what an installed APO reports to the Windows audio engine.
//!
//! The engine's loader is undocumented enough that guessing is expensive: it
//! reads the APO store under `HKCR\AudioEngine\AudioProcessingObjects\{clsid}`,
//! then `CoCreateInstance`s the APO and asks it for `APO_REG_PROPERTIES`. If
//! the two disagree in a way the engine dislikes, the effect is silently
//! dropped and a failure counter is bumped. So this tool asks the reference
//! product the same question and prints the answer in exactly the types our
//! own APO writes, letting the two be diffed.
//!
//! Read-only reference analysis: it loads a DLL, asks COM for its registration
//! data and unloads it. Nothing is written and no third-party code is reused.
//!
//! ```text
//! cargo run -p sb-refprobe                       # the reference product
//! cargo run -p sb-refprobe -- <dll> <clsid>      # anything else
//! ```

use core::ffi::c_void;
use core::ptr::{addr_of, null_mut};
use std::process::ExitCode;

use sb_apo::abi::*;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// The reference implementation, as found in the registry on this machine.
const DEFAULT_DLL: &str = r"C:\Program Files (x86)\Letasoft Sound Booster\Sbapo.dll";
const DEFAULT_CLSID: &str = "{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}";

/// Our own endpoint stage, for diffing against the reference.
const OUR_ENDPOINT_CLSID: &str = "{A7C3E1D9-4B62-4F38-9E57-1C8D2A6B0F42}";

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let dll = args
        .next()
        .unwrap_or_else(|| std::env::var("SB_APO_DLL").unwrap_or_else(|_| DEFAULT_DLL.to_string()));
    let clsid_text = args.next().unwrap_or_else(|| {
        std::env::var("SB_APO_CLSID").unwrap_or_else(|_| DEFAULT_CLSID.to_string())
    });

    let clsid = match parse_clsid(&clsid_text) {
        Ok(clsid) => clsid,
        Err(error) => {
            eprintln!("bad CLSID {clsid_text:?}: {error}");
            eprintln!("usage: sb-refprobe [dll-path] [{{CLSID}}]");
            return ExitCode::FAILURE;
        }
    };

    println!("dll    : {dll}");
    println!("clsid  : {clsid}");
    println!(
        "ours?  : {}",
        if clsid_text.eq_ignore_ascii_case(OUR_ENDPOINT_CLSID) {
            "our endpoint stage"
        } else {
            "third party"
        }
    );
    println!();

    match unsafe { probe(&dll, clsid) } {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Load `dll`, instantiate `clsid` through its own `DllGetClassObject`, and
/// print everything the engine would learn about it.
///
/// # Safety
/// The DLL must be an in-process COM server implementing the APO interfaces;
/// arbitrary code is executed in this process by design.
unsafe fn probe(dll: &str, clsid: GUID) -> Result<(), String> {
    let path: Vec<u16> = dll.encode_utf16().chain(core::iter::once(0)).collect();
    let module = LoadLibraryW(path.as_ptr());
    if module.is_null() {
        return Err(format!(
            "LoadLibraryW({dll}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Everything below runs with the module loaded, so the cleanup at the end
    // is not optional: the loader owns a reference to the file until then.
    let result = probe_loaded(module, clsid);
    FreeLibrary(module);
    result
}

unsafe fn probe_loaded(module: HMODULE, clsid: GUID) -> Result<(), String> {
    let address = GetProcAddress(module, c"DllGetClassObject".as_ptr().cast());
    let Some(address) = address else {
        return Err("no DllGetClassObject export: not a COM in-proc server".to_string());
    };
    let get_class_object: DllGetClassObjectFn = core::mem::transmute(address);

    let mut factory: *mut c_void = null_mut();
    let hr = get_class_object(&clsid, &IID_ICLASS_FACTORY, &mut factory);
    if !succeeded(hr) || factory.is_null() {
        return Err(format!(
            "DllGetClassObject -> 0x{hr:08X}, factory {factory:p}"
        ));
    }
    let factory_vtbl = *(factory as *mut *const IClassFactoryVtbl);

    let mut apo: *mut c_void = null_mut();
    let hr = ((*factory_vtbl).create_instance)(
        factory,
        null_mut(),
        &IID_IAUDIO_PROCESSING_OBJECT,
        &mut apo,
    );
    if !succeeded(hr) || apo.is_null() {
        ((*factory_vtbl).release)(factory);
        return Err(format!("CreateInstance -> 0x{hr:08X}, apo {apo:p}"));
    }

    let vtbl = *(apo as *mut *const IAudioProcessingObjectVtbl);
    report_registration_properties(vtbl, apo);
    report_interface_support(vtbl, apo);
    report_effects_list(vtbl, apo);

    // The registration block is CoTaskMemAlloc'd by the APO, as the SDK
    // requires; the engine frees it the same way.
    let mut properties: *mut ApoRegProperties = null_mut();
    let hr = ((*vtbl).get_registration_properties)(apo, &mut properties);
    if succeeded(hr) && !properties.is_null() {
        CoTaskMemFree(properties as *const c_void);
    }

    ((*vtbl).release)(apo);
    ((*factory_vtbl).release)(factory);
    Ok(())
}

unsafe fn report_registration_properties(
    vtbl: *const IAudioProcessingObjectVtbl,
    apo: *mut c_void,
) {
    let mut properties: *mut ApoRegProperties = null_mut();
    let hr = ((*vtbl).get_registration_properties)(apo, &mut properties);
    println!("GetRegistrationProperties -> 0x{hr:08X}");
    if !succeeded(hr) || properties.is_null() {
        println!("  (no properties)");
        return;
    }

    let registration = &*properties;
    println!("  clsid             : {}", registration.clsid);
    println!(
        "  flags             : 0x{:02X}{}",
        registration.flags,
        describe_flags(registration.flags)
    );
    println!(
        "  friendly name     : {:?}",
        utf16(&registration.friendly_name)
    );
    println!(
        "  copyright         : {:?}",
        utf16(&registration.copyright_info)
    );
    println!(
        "  version           : {}.{}",
        registration.major_version, registration.minor_version
    );
    println!(
        "  connections       : in {}..{} out {}..{}",
        registration.min_input_connections,
        registration.max_input_connections,
        registration.min_output_connections,
        registration.max_output_connections
    );
    println!(
        "  max instances     : {}",
        registration.max_instances as i32
    );
    println!("  interfaces        : {}", registration.num_apo_interfaces);

    // The struct ends in a one-element array: the APO allocates
    // size_of + (n - 1) * size_of::<GUID>() behind it.
    let list = addr_of!((*properties).iid_apo_interface_list) as *const GUID;
    for index in 0..registration.num_apo_interfaces {
        let iid = *list.add(index as usize);
        println!("    [{index}] {iid}{}", describe_iid(&iid));
    }
}

unsafe fn report_interface_support(vtbl: *const IAudioProcessingObjectVtbl, apo: *mut c_void) {
    println!();
    println!("QueryInterface:");
    let probes: [(&str, GUID); 8] = [
        ("IUnknown", IID_IUNKNOWN),
        ("IAudioProcessingObject", IID_IAUDIO_PROCESSING_OBJECT),
        (
            "IAudioProcessingObjectConfiguration",
            IID_IAUDIO_PROCESSING_OBJECT_CONFIGURATION,
        ),
        ("IAudioProcessingObjectRT", IID_IAUDIO_PROCESSING_OBJECT_RT),
        ("IAudioSystemEffects", IID_IAUDIO_SYSTEM_EFFECTS),
        ("IAudioSystemEffects2", IID_IAUDIO_SYSTEM_EFFECTS2),
        ("IAudioMediaType", IID_IAUDIO_MEDIA_TYPE),
        ("IID_IAudioSystemEffects3?", GUID::nil()),
    ];
    for (name, iid) in probes {
        if iid == GUID::nil() {
            continue;
        }
        let mut pointer: *mut c_void = null_mut();
        let hr = ((*vtbl).query_interface)(apo, &iid, &mut pointer);
        println!("  {name:<36} 0x{hr:08X}");
        if succeeded(hr) && !pointer.is_null() {
            let released = *(pointer as *mut *const UnknownVtbl);
            ((*released).release)(pointer);
        }
    }
}

unsafe fn report_effects_list(vtbl: *const IAudioProcessingObjectVtbl, apo: *mut c_void) {
    let mut effects: *mut c_void = null_mut();
    let hr = ((*vtbl).query_interface)(apo, &IID_IAUDIO_SYSTEM_EFFECTS2, &mut effects);
    if !succeeded(hr) || effects.is_null() {
        return;
    }
    let effects_vtbl = *(effects as *mut *const IAudioSystemEffects2Vtbl);
    let mut list: *mut GUID = null_mut();
    let mut count: u32 = 0;
    let hr = ((*effects_vtbl).get_effects_list)(effects, &mut list, &mut count, null_mut());
    println!();
    println!("GetEffectsList -> 0x{hr:08X}, {count} effect(s)");
    if succeeded(hr) && !list.is_null() {
        for index in 0..count {
            println!("  [{index}] {}", *list.add(index as usize));
        }
        CoTaskMemFree(list as *const c_void);
    }
    ((*effects_vtbl).release)(effects);
}

fn describe_flags(flags: u32) -> String {
    let mut names = Vec::new();
    for (bit, name) in [
        (APO_FLAG_INPLACE, "INPLACE"),
        (
            APO_FLAG_SAMPLESPERFRAME_MUST_MATCH,
            "SAMPLESPERFRAME_MUST_MATCH",
        ),
        (
            APO_FLAG_FRAMESPERSECOND_MUST_MATCH,
            "FRAMESPERSECOND_MUST_MATCH",
        ),
        (
            APO_FLAG_BITSPERSAMPLE_MUST_MATCH,
            "BITSPERSAMPLE_MUST_MATCH",
        ),
        (APO_FLAG_MIXER, "MIXER"),
    ] {
        if flags & bit != 0 {
            names.push(name);
        }
    }
    if names.is_empty() {
        " (none)".to_string()
    } else {
        format!(" ({})", names.join(" | "))
    }
}

fn describe_iid(iid: &GUID) -> &'static str {
    match *iid {
        IID_IUNKNOWN => " = IUnknown",
        IID_IAUDIO_PROCESSING_OBJECT => " = IAudioProcessingObject",
        IID_IAUDIO_PROCESSING_OBJECT_CONFIGURATION => " = IAudioProcessingObjectConfiguration",
        IID_IAUDIO_PROCESSING_OBJECT_RT => " = IAudioProcessingObjectRT",
        IID_IAUDIO_SYSTEM_EFFECTS => " = IAudioSystemEffects",
        IID_IAUDIO_SYSTEM_EFFECTS2 => " = IAudioSystemEffects2",
        _ => "",
    }
}

fn utf16(text: &[u16]) -> String {
    text.iter()
        .take_while(|unit| **unit != 0)
        .map(|unit| char::from_u32(*unit as u32).unwrap_or('?'))
        .collect()
}

/// Parse `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, the form the registry uses.
fn parse_clsid(text: &str) -> Result<GUID, String> {
    let trimmed = text.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() != 5 {
        return Err("expected 5 dash-separated groups".to_string());
    }
    let hex = |value: &str, digits: usize| -> Result<u64, String> {
        if value.len() != digits {
            return Err(format!("{value:?} should be {digits} hex digits"));
        }
        u64::from_str_radix(value, 16).map_err(|error| format!("{value:?}: {error}"))
    };

    let data1 = hex(parts[0], 8)? as u32;
    let data2 = hex(parts[1], 4)? as u16;
    let data3 = hex(parts[2], 4)? as u16;
    let tail = hex(parts[3], 4)? as u16;
    let rest = hex(parts[4], 12)?;
    let mut data4 = [0u8; 8];
    data4[0] = (tail >> 8) as u8;
    data4[1] = tail as u8;
    for index in 0..6 {
        data4[2 + index] = ((rest >> (8 * (5 - index))) & 0xFF) as u8;
    }
    Ok(GUID::new(data1, data2, data3, data4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_text_round_trips_through_the_parser() {
        let parsed = parse_clsid("{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}").unwrap();
        assert_eq!(parsed.data1, 0xBABAE75F);
        assert_eq!(parsed.data2, 0xA7AA);
        assert_eq!(parsed.data3, 0x4A13);
        assert_eq!(
            parsed.data4,
            [0xB1, 0xF7, 0xBB, 0x27, 0x16, 0xFD, 0x00, 0x3E]
        );
        assert_eq!(parsed.to_string(), "{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}");
        assert_eq!(
            parse_clsid(&IID_IAUDIO_SYSTEM_EFFECTS2.to_string()).unwrap(),
            IID_IAUDIO_SYSTEM_EFFECTS2
        );
    }

    #[test]
    fn malformed_clsids_are_rejected() {
        for bad in [
            "",
            "{not-a-guid}",
            "{BABAE75F-A7AA-4A13-B1F7}",
            "{BABAE75F-A7AA-4A13-B1F7-BB2716FD003}",
        ] {
            assert!(parse_clsid(bad).is_err(), "{bad:?} was accepted");
        }
    }
}
