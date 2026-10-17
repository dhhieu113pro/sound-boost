//! Where the tray app keeps its settings: the current user's registry.
//!
//! `HKCU`, not `HKLM`: these are one person's preferences and the tray app is
//! not elevated. The installer's own bookkeeping lives under
//! `HKLM\SOFTWARE\Sound Booster`, so the two never touch.

use std::io;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

use crate::settings::Settings;

/// The tray app's settings.
pub const SETTINGS_KEY: &str = r"Software\Sound Booster\Tray";

/// Per-user autostart.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Sound Booster";

/// The argument autostart passes, so a login starts quietly in the tray
/// instead of opening the panel.
pub const BACKGROUND_ARG: &str = "--background";

/// Load the saved settings, or the defaults if there are none.
pub fn load() -> Settings {
    Settings::from_values(|name| read_dword(SETTINGS_KEY, name))
}

/// Save the settings. Creates the key on first use.
pub fn save(settings: &Settings) -> io::Result<()> {
    for (name, value) in settings.to_values() {
        write_dword(SETTINGS_KEY, name, value)?;
    }
    Ok(())
}

/// The command line the Run key holds for this executable.
pub fn autostart_command(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_ARG}", exe.display())
}

/// True if the Run key starts *this* executable. An entry left behind by a
/// copy in another folder counts as off, so ticking the box fixes it.
pub fn autostart_enabled(exe: &Path) -> bool {
    read_string(RUN_KEY, RUN_VALUE)
        .is_some_and(|command| command.eq_ignore_ascii_case(&autostart_command(exe)))
}

pub fn set_autostart(exe: &Path, enabled: bool) -> io::Result<()> {
    if enabled {
        write_string(RUN_KEY, RUN_VALUE, &autostart_command(exe))
    } else {
        let key = wide(RUN_KEY);
        let value = wide(RUN_VALUE);
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) };
        match status {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
            error => Err(os_error(error)),
        }
    }
}

fn read_dword(key: &str, name: &str) -> Option<u32> {
    let key = wide(key);
    let name = wide(name);
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(value)
}

fn write_dword(key: &str, name: &str, value: u32) -> io::Result<()> {
    let key = wide(key);
    let name = wide(name);
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            REG_DWORD,
            (&value as *const u32).cast(),
            size_of::<u32>() as u32,
        )
    };
    check(status)
}

fn read_string(key: &str, name: &str) -> Option<String> {
    let key = wide(key);
    let name = wide(name);
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let length = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..length]))
}

fn write_string(key: &str, name: &str, value: &str) -> io::Result<()> {
    let key = wide(key);
    let name = wide(name);
    let data = wide(value);
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    check(status)
}

fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(os_error(status))
    }
}

fn os_error(status: WIN32_ERROR) -> io::Error {
    io::Error::from_raw_os_error(status as i32)
}

/// NUL-terminated UTF-16.
pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs against the real `HKCU` under a throwaway key, so it never
    /// disturbs a user's settings.
    #[test]
    fn dwords_and_strings_round_trip() {
        let key = r"Software\Sound Booster\TestScratch";
        write_dword(key, "Number", 0xDEAD_BEEF).unwrap();
        assert_eq!(read_dword(key, "Number"), Some(0xDEAD_BEEF));
        assert_eq!(read_dword(key, "Missing"), None);

        write_string(key, "Text", "\"C:\\Program Files\\x.exe\" --background").unwrap();
        assert_eq!(
            read_string(key, "Text").as_deref(),
            Some("\"C:\\Program Files\\x.exe\" --background")
        );

        let scratch = wide(key);
        let status = unsafe {
            windows_sys::Win32::System::Registry::RegDeleteTreeW(
                HKEY_CURRENT_USER,
                scratch.as_ptr(),
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
        assert_eq!(read_dword(key, "Number"), None);
    }

    #[test]
    fn autostart_command_quotes_the_path() {
        let command = autostart_command(Path::new(r"C:\Program Files\Sound Booster\sb-tray.exe"));
        assert_eq!(
            command,
            "\"C:\\Program Files\\Sound Booster\\sb-tray.exe\" --background"
        );
    }
}
