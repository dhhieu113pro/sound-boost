//! Render endpoints, read straight out of the registry.
//!
//! `MMDevices\Audio\Render` has one subkey per endpoint GUID, which is exactly
//! the GUID used in that endpoint's `FxProperties` path — so the installer can
//! find, inspect and patch endpoints without instantiating any COM interface.
//! `ApoControl.dll` in the reference product builds the same paths itself,
//! which is good evidence that this is a supported way to do it.

use std::io;

use windows_sys::Win32::System::Registry::{KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE};

use crate::plan::Value;
use crate::win::{self, Key};

/// `PKEY_Device_FriendlyName`, the name shown in the Sound control panel.
const DEVICE_FRIENDLY_NAME: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";

/// `PKEY_AudioEndpoint_Disable_SysFx` (`mmdeviceapi.idl`): the "Disable all
/// enhancements" checkbox. When it is set, the engine does not load any APO on
/// the endpoint, no matter how well we registered, so the installer reports it.
const DISABLE_SYSFX: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";

/// `DeviceState`: 1 means active. The rest are the states a stale endpoint can
/// be left in by a driver update or by unplugging hardware.
const DEVICE_STATE_ACTIVE: u32 = 1;

pub struct RenderEndpoint {
    pub id: String,
    pub name: Option<String>,
    pub state: Option<u32>,
    pub sysfx_disabled: Option<bool>,
    /// Whether the endpoint has an `FxProperties` key at all.
    pub has_fx: bool,
}

impl RenderEndpoint {
    /// Active means the low nibble is 1. Endpoints that were removed keep
    /// higher bits set (`0x22000004`), so an exact comparison would miss a
    /// live endpoint and include a dead one.
    pub fn is_active(&self) -> bool {
        matches!(self.state, Some(state) if state & 0xF == DEVICE_STATE_ACTIVE)
    }

    pub fn state_text(&self) -> String {
        let Some(state) = self.state else {
            return "unknown".to_string();
        };
        let named = match state & 0xF {
            1 => "active",
            2 => "disabled",
            4 => "not present",
            8 => "unplugged",
            _ => "unknown",
        };
        if state == state & 0xF {
            named.to_string()
        } else {
            format!("{named} ({state:#010x})")
        }
    }

    pub fn fx_path(&self) -> String {
        format!("{}\\{}\\FxProperties", win::RENDER_KEY, self.id)
    }

    /// The effect values this endpoint currently has, or an empty map.
    pub fn fx_values(&self) -> io::Result<std::collections::BTreeMap<String, Value>> {
        Ok(win::read_values(&self.fx_path())?.unwrap_or_default())
    }

    /// How this endpoint looks in a status line.
    pub fn label(&self) -> String {
        let name = self.name.as_deref().unwrap_or("(unnamed)");
        match self.is_active() {
            true => name.to_string(),
            false => format!("{name} [{}]", self.state_text()),
        }
    }
}

/// Every render endpoint on the machine, active or not.
pub fn render_endpoints() -> io::Result<Vec<RenderEndpoint>> {
    let root = Key::open(win::RENDER_KEY, KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS)?;
    let mut endpoints = Vec::new();
    for id in root.subkeys()? {
        let endpoint_path = format!("{}\\{}", win::RENDER_KEY, id);
        // `DeviceState` is a value on the endpoint key itself; the friendly
        // name and the "disable enhancements" flag live in its `Properties`
        // subkey.
        let state = match Key::open_optional(&endpoint_path, KEY_QUERY_VALUE)? {
            Some(key) => key.get(Some("DeviceState"))?.and_then(|value| match value {
                Value::Dword(state) => Some(state),
                _ => None,
            }),
            None => None,
        };
        let properties =
            win::read_values(&format!("{endpoint_path}\\Properties"))?.unwrap_or_default();
        let fx = format!("{endpoint_path}\\FxProperties");
        endpoints.push(RenderEndpoint {
            name: properties
                .get(DEVICE_FRIENDLY_NAME)
                .and_then(text)
                .filter(|name| !name.is_empty()),
            state,
            sysfx_disabled: properties.get(DISABLE_SYSFX).and_then(|value| match value {
                Value::Dword(flag) => Some(*flag != 0),
                _ => None,
            }),
            has_fx: Key::open_optional(&fx, KEY_QUERY_VALUE)?.is_some(),
            id,
        });
    }
    endpoints.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(endpoints)
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Sz(text) | Value::ExpandSz(text) => Some(text.clone()),
        // Some device properties are stored as one-entry lists.
        Value::MultiSz(entries) => entries.first().cloned(),
        _ => None,
    }
}
