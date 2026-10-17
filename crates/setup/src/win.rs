//! The parts that touch Windows: registry, process/module lookup and the audio
//! service.
//!
//! Everything here is a thin, fallible wrapper. The decisions live in
//! [`crate::plan`]; this module only reads and writes what that plan says.

use std::collections::BTreeMap;
use std::io;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS,
    INVALID_HANDLE_VALUE, WIN32_ERROR,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW,
    MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY,
};

use crate::plan::Value;

/// Machine-wide audio settings, including `DisableProtectedAudioDG`.
pub const AUDIO_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Audio";

/// Where the COM registrations live when written machine-wide. Writing here
/// rather than to the merged `HKEY_CLASSES_ROOT` view keeps the effect of the
/// installer deterministic.
pub const CLASSES_KEY: &str = r"SOFTWARE\Classes";

/// Render endpoints, one subkey per endpoint GUID.
pub const RENDER_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";

/// Our own backup and bookkeeping root.
pub const BACKUP_KEY: &str = r"SOFTWARE\Sound Booster";

/// `HKEY_LOCAL_MACHINE` with the 64-bit view forced, so a 32-bit build of this
/// tool still edits the keys the (64-bit) audio engine reads.
const MACHINE: HKEY = HKEY_LOCAL_MACHINE;
const VIEW: u32 = KEY_WOW64_64KEY;

#[derive(Debug)]
pub struct RegistryError {
    pub code: WIN32_ERROR,
    pub path: String,
    pub what: String,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = io::Error::from_raw_os_error(self.code as i32);
        write!(
            formatter,
            "{} on {} failed: {message} ({}), error {:#x}",
            self.what,
            if self.path.is_empty() {
                "HKLM"
            } else {
                &self.path
            },
            self.description(),
            self.code
        )
    }
}

impl RegistryError {
    fn description(&self) -> &'static str {
        match self.code {
            ERROR_ACCESS_DENIED => "access denied; run elevated",
            ERROR_FILE_NOT_FOUND => "not found",
            _ => "registry error",
        }
    }
}

impl std::error::Error for RegistryError {}

/// One open registry key, closed on drop.
pub struct Key {
    handle: HKEY,
    path: String,
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.handle) };
    }
}

impl Key {
    pub fn open(path: &str, access: u32) -> io::Result<Self> {
        Self::open_optional(path, access)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("key HKLM\\{path} does not exist"),
            )
        })
    }

    pub fn open_optional(path: &str, access: u32) -> io::Result<Option<Self>> {
        let subkey = wide(path);
        let mut handle: HKEY = null_mut();
        let code =
            unsafe { RegOpenKeyExW(MACHINE, subkey.as_ptr(), 0, access | VIEW, &mut handle) };
        match code {
            ERROR_SUCCESS => Ok(Some(Key {
                handle,
                path: path.to_string(),
            })),
            ERROR_FILE_NOT_FOUND => Ok(None),
            other => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                RegistryError {
                    code: other,
                    path: path.to_string(),
                    what: "open".to_string(),
                }
                .to_string(),
            )),
        }
    }

    /// Create a key and any missing parents.
    pub fn create(path: &str) -> io::Result<Self> {
        let subkey = wide(path);
        let mut handle: HKEY = null_mut();
        let mut disposition = 0u32;
        let code = unsafe {
            RegCreateKeyExW(
                MACHINE,
                subkey.as_ptr(),
                0,
                null(),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_ENUMERATE_SUB_KEYS | VIEW,
                null(),
                &mut handle,
                &mut disposition,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                RegistryError {
                    code,
                    path: path.to_string(),
                    what: "create".to_string(),
                }
                .to_string(),
            ));
        }
        Ok(Key {
            handle,
            path: path.to_string(),
        })
    }

    /// Read one value; `Ok(None)` means it is not there.
    pub fn get(&self, name: Option<&str>) -> io::Result<Option<Value>> {
        let mut buffer: Vec<u16> = Vec::new();
        let pointer = name_pointer(name, &mut buffer);
        let mut kind = 0u32;
        let mut size = 0u32;
        let code = unsafe {
            RegQueryValueExW(
                self.handle,
                pointer,
                null(),
                &mut kind,
                null_mut(),
                &mut size,
            )
        };
        match code {
            ERROR_SUCCESS => {}
            ERROR_FILE_NOT_FOUND => return Ok(None),
            other => {
                return Err(io::Error::other(RegistryError {
                    code: other,
                    path: self.path.clone(),
                    what: format!("query size of {name:?}"),
                }))
            }
        }

        let mut bytes = vec![0u8; size as usize];
        let code = unsafe {
            RegQueryValueExW(
                self.handle,
                pointer,
                null(),
                &mut kind,
                bytes.as_mut_ptr(),
                &mut size,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(io::Error::other(RegistryError {
                code,
                path: self.path.clone(),
                what: format!("query {name:?}"),
            }));
        }
        bytes.truncate(size as usize);
        Ok(Some(Value::decode(kind, &bytes)))
    }

    pub fn set(&self, name: Option<&str>, value: &Value) -> io::Result<()> {
        let mut buffer: Vec<u16> = Vec::new();
        let pointer = name_pointer(name, &mut buffer);
        let (kind, bytes) = value.encode();
        let code = unsafe {
            RegSetValueExW(
                self.handle,
                pointer,
                0,
                kind,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                RegistryError {
                    code,
                    path: self.path.clone(),
                    what: format!("set {name:?} = {}", value.render()),
                }
                .to_string(),
            ));
        }
        Ok(())
    }

    pub fn delete_value(&self, name: Option<&str>) -> io::Result<()> {
        let mut buffer: Vec<u16> = Vec::new();
        let pointer = name_pointer(name, &mut buffer);
        let code = unsafe { RegDeleteValueW(self.handle, pointer) };
        if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                RegistryError {
                    code,
                    path: self.path.clone(),
                    what: format!("delete {name:?}"),
                }
                .to_string(),
            ));
        }
        Ok(())
    }

    /// All values of this key.
    pub fn values(&self) -> io::Result<BTreeMap<String, Value>> {
        let mut values = BTreeMap::new();
        let mut index = 0u32;
        loop {
            let mut name = vec![0u16; 512];
            let mut name_length = name.len() as u32;
            let mut kind = 0u32;
            let mut size = 0u32;
            let code = unsafe {
                RegEnumValueW(
                    self.handle,
                    index,
                    name.as_mut_ptr(),
                    &mut name_length,
                    null(),
                    &mut kind,
                    null_mut(),
                    &mut size,
                )
            };
            match code {
                ERROR_SUCCESS => {}
                ERROR_NO_MORE_ITEMS => break,
                other => {
                    return Err(io::Error::other(RegistryError {
                        code: other,
                        path: self.path.clone(),
                        what: format!("enumerate value {index}"),
                    }))
                }
            }

            let mut bytes = vec![0u8; size as usize];
            let mut read_size = size;
            let code = unsafe {
                RegEnumValueW(
                    self.handle,
                    index,
                    name.as_mut_ptr(),
                    &mut name_length,
                    null(),
                    &mut kind,
                    bytes.as_mut_ptr(),
                    &mut read_size,
                )
            };
            if code != ERROR_SUCCESS {
                // The value shrank or grew between the two calls; read it the
                // ordinary way instead.
                let name_text = String::from_utf16_lossy(&name[..name_length as usize]);
                if let Some(value) = self.get(Some(&name_text))? {
                    values.insert(name_text, value);
                }
                index += 1;
                continue;
            }

            bytes.truncate(read_size as usize);
            let name_text = String::from_utf16_lossy(&name[..name_length as usize]);
            values.insert(name_text, Value::decode(kind, &bytes));
            index += 1;
        }
        Ok(values)
    }

    /// Immediate subkey names.
    pub fn subkeys(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        let mut index = 0u32;
        loop {
            let mut name = vec![0u16; 512];
            let mut length = name.len() as u32;
            let code = unsafe {
                RegEnumKeyExW(
                    self.handle,
                    index,
                    name.as_mut_ptr(),
                    &mut length,
                    null(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            match code {
                ERROR_SUCCESS => {}
                ERROR_NO_MORE_ITEMS => break,
                other => {
                    return Err(io::Error::other(RegistryError {
                        code: other,
                        path: self.path.clone(),
                        what: format!("enumerate subkey {index}"),
                    }))
                }
            }
            names.push(String::from_utf16_lossy(&name[..length as usize]));
            index += 1;
        }
        Ok(names)
    }
}

/// Read every value of a key, or `None` if the key is not there.
pub fn read_values(path: &str) -> io::Result<Option<BTreeMap<String, Value>>> {
    match Key::open_optional(path, KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS)? {
        Some(key) => Ok(Some(key.values()?)),
        None => Ok(None),
    }
}

/// Remove a key and everything under it.
pub fn delete_tree(path: &str) -> io::Result<()> {
    let subkey = wide(path);
    let code = unsafe { RegDeleteTreeW(MACHINE, subkey.as_ptr()) };
    if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            RegistryError {
                code,
                path: path.to_string(),
                what: "delete tree".to_string(),
            }
            .to_string(),
        ));
    }
    Ok(())
}

/// Whether this process can write machine-wide audio settings.
///
/// This is the capability the installer actually needs, so it is a better test
/// than asking the token whether it is elevated.
pub fn can_write_audio_settings() -> bool {
    Key::open(AUDIO_KEY, KEY_SET_VALUE).is_ok()
}

/// Find a process by executable name, e.g. `audiodg.exe`.
pub fn find_process(executable: &str) -> io::Result<Option<u32>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    let mut found = None;
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        let name = utf16z(&entry.szExeFile);
        if name.eq_ignore_ascii_case(executable) {
            found = Some(entry.th32ProcessID);
            break;
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    Ok(found)
}

/// The DLLs a process has loaded. Needs permission to read that process, which
/// `audiodg.exe` may refuse even to an administrator.
pub fn modules_of(pid: u32) -> io::Result<Vec<String>> {
    let snapshot =
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;

    let mut names = Vec::new();
    let mut ok = unsafe { Module32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        names.push(utf16z(&entry.szModule));
        ok = unsafe { Module32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    Ok(names)
}

/// Bounce the audio stack, so `audiodg.exe` picks up our DLL.
///
/// `audiodg.exe` holds the APO DLL loaded, so the file cannot be replaced and
/// the effect chain is not re-read until it restarts. It is created from the
/// endpoint stack, which `AudioEndpointBuilder` owns: stopping `Audiosrv` on its
/// own is refused while anything is streaming through it (error 1051) and
/// starting it again fails (1056). So the whole stack is bounced instead --
/// `net stop ... /y` takes the dependent audio service down with it, and the two
/// starts bring it back -- and then we report the pid we ended up with.
pub fn restart_audio_service() -> Vec<String> {
    let mut report = Vec::new();
    let before = find_process("audiodg.exe").unwrap_or(None);

    let steps: [&[&str]; 3] = [
        &["stop", "AudioEndpointBuilder", "/y"],
        &["start", "AudioEndpointBuilder"],
        &["start", "Audiosrv"],
    ];
    for arguments in steps {
        report.push(net_step(arguments));
    }

    // `audiodg.exe` is created on demand, so it may well be absent right after
    // the bounce: that is a success, not a problem.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let after = find_process("audiodg.exe").unwrap_or(None);
    report.push(match (before, after) {
        (Some(before), Some(after)) if before != after => {
            format!("audiodg.exe: pid {before} -> {after}")
        }
        (Some(before), Some(after)) => format!("audiodg.exe: still pid {after} (was {before})"),
        (_, Some(after)) => format!("audiodg.exe: pid {after}"),
        (_, None) => "audiodg.exe: not running (it starts again with the next stream)".to_string(),
    });
    report
}

/// Stop the audio stack, so that a mapped `sb_apo.dll` can be replaced.
///
/// The engine maps our DLL into `audiodg.exe` for as long as it is running, and
/// a mapped image cannot be overwritten: the copy fails with "being used by
/// another process". That failure has to be taken seriously rather than
/// printed, because it leaves the *previous* build registered and every later
/// observation then describes the wrong DLL -- which is exactly what happened
/// on the first four-configuration sweep. The caller starts the stack again
/// with [`restart_audio_service`].
pub fn stop_audio_service() -> Vec<String> {
    vec![net_step(&["stop", "AudioEndpointBuilder", "/y"])]
}

/// Run one `net` step and describe what happened, success or not.
fn net_step(arguments: &[&str]) -> String {
    let (action, name) = (arguments[0], arguments[1]);
    match std::process::Command::new("net.exe").args(arguments).output() {
        Ok(output) if output.status.success() => format!("net {action} {name}: ok"),
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            let first = text.lines().map(str::trim).find(|line| !line.is_empty());
            format!(
                "net {action} {name}: exit {} ({})",
                output.status.code().unwrap_or(-1),
                first.unwrap_or("no output")
            )
        }
        Err(error) => format!("net {action} {name}: {error}"),
    }
}

fn name_pointer(name: Option<&str>, buffer: &mut Vec<u16>) -> *const u16 {
    match name {
        Some(text) => {
            *buffer = wide(text);
            buffer.as_ptr()
        }
        // A null name addresses the key's default value.
        None => null(),
    }
}

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn utf16z(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        let text = wide("abc");
        assert_eq!(text, vec![0x61, 0x62, 0x63, 0]);
    }

    #[test]
    fn the_render_key_exists_on_any_windows_machine() {
        let key = Key::open(RENDER_KEY, KEY_ENUMERATE_SUB_KEYS).expect("render key");
        assert!(
            !key.subkeys().expect("subkeys").is_empty(),
            "no render endpoints"
        );
    }

    #[test]
    fn reading_a_missing_value_is_not_an_error() {
        let key = Key::open(RENDER_KEY, KEY_QUERY_VALUE).expect("render key");
        assert_eq!(key.get(Some("no such value")).expect("query"), None);
    }
}
