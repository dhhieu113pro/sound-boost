//! Everything the engine is told about this APO, in one place.
//!
//! Two copies of the same facts have to agree:
//!
//! * the `APO_REG_PROPERTIES` block this DLL hands back from
//!   `IAudioProcessingObject::GetRegistrationProperties`, and
//! * the mirror of it in the engine's APO store,
//!   `HKCR\AudioEngine\AudioProcessingObjects\{clsid}`, which the installer
//!   writes.
//!
//! A mismatch between them is one of the documented reasons the engine drops
//! an effect (and silently bumps a failure counter that eventually disables
//! system effects on the endpoint), so both sides read these values instead of
//! writing them out twice.
//!
//! The values were checked against the reference product with
//! `cargo run -p sb-refprobe`, which loads an installed APO and prints what it
//! reports. It reports one interface, flags `0x0D`, one in and one out, and
//! `-1` for the instance cap.

use crate::abi::{
    APO_FLAG_BITSPERSAMPLE_MUST_MATCH, APO_FLAG_FRAMESPERSECOND_MUST_MATCH, APO_FLAG_INPLACE, GUID,
    IID_IAUDIO_PROCESSING_OBJECT,
};
use crate::object::ApoRole;

/// The engine's APO store, relative to `HKEY_CLASSES_ROOT`.
pub const APO_STORE_KEY: &str = r"AudioEngine\AudioProcessingObjects";

/// Frame rate and sample size must match the neighbours, and we work in place.
///
/// `APO_FLAG_SAMPLESPERFRAME_MUST_MATCH` is deliberately absent (it is part of
/// the SDK's `APO_FLAG_DEFAULT`): the chain is rebuilt per locked format and
/// handles any channel count, so the constraint would only narrow where the
/// engine is allowed to place us. The reference product omits it too.
pub const APO_FLAGS: u32 =
    APO_FLAG_INPLACE | APO_FLAG_FRAMESPERSECOND_MUST_MATCH | APO_FLAG_BITSPERSAMPLE_MUST_MATCH;

/// The interfaces advertised in `APO_REG_PROPERTIES`.
///
/// `QueryInterface` answers for the configuration, real-time, and system
/// effects interfaces as well; the engine asks for those by IID rather than
/// reading this list, and advertising the minimum keeps the store and the DLL
/// trivially consistent.
pub const APO_INTERFACES: &[GUID] = &[IID_IAUDIO_PROCESSING_OBJECT];

/// One registered effect: the DLL's report and the store's contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApoRegistration {
    pub clsid: GUID,
    pub flags: u32,
    pub friendly_name: &'static str,
    pub copyright: &'static str,
    pub major_version: u32,
    pub minor_version: u32,
    pub min_input_connections: u32,
    pub max_input_connections: u32,
    pub min_output_connections: u32,
    pub max_output_connections: u32,
    /// `u32::MAX` means no limit; the store writes it as `-1`.
    pub max_instances: u32,
    pub interfaces: &'static [GUID],
}

/// The registration of one stage.
pub const fn for_role(role: ApoRole) -> ApoRegistration {
    ApoRegistration {
        clsid: role.clsid(),
        flags: APO_FLAGS,
        friendly_name: role.friendly_name(),
        copyright: COPYRIGHT,
        major_version: 1,
        minor_version: 0,
        min_input_connections: 1,
        max_input_connections: 1,
        min_output_connections: 1,
        max_output_connections: 1,
        // A hint for hardware-backed APOs; software APOs are unlimited.
        max_instances: u32::MAX,
        interfaces: APO_INTERFACES,
    }
}

pub const COPYRIGHT: &str = "Sound Booster, original implementation";

/// Both stages, for the installer to walk.
pub const ALL: [ApoRegistration; 2] = [for_role(ApoRole::Stream), for_role(ApoRole::Endpoint)];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{CLSID_ENDPOINT, CLSID_STREAM};

    #[test]
    fn the_two_stages_register_as_different_effects() {
        assert_eq!(ALL[0].clsid, CLSID_STREAM);
        assert_eq!(ALL[1].clsid, CLSID_ENDPOINT);
        assert_ne!(ALL[0].friendly_name, ALL[1].friendly_name);
    }

    #[test]
    fn the_flags_are_the_ones_the_reference_reports() {
        // INPLACE | FRAMESPERSECOND_MUST_MATCH | BITSPERSAMPLE_MUST_MATCH.
        // Pinned because sb-refprobe read exactly this back from the reference
        // product, and a drifting flag set changes where the engine places us.
        assert_eq!(APO_FLAGS, 0x0D);
        assert_eq!(ALL[0].flags, ALL[1].flags);
    }

    #[test]
    fn the_advertised_interface_list_is_the_reference_one() {
        assert_eq!(APO_INTERFACES, &[IID_IAUDIO_PROCESSING_OBJECT]);
        assert_eq!(for_role(ApoRole::Stream).interfaces.len(), 1);
    }

    #[test]
    fn the_instance_cap_is_unlimited_and_written_as_minus_one() {
        assert_eq!(for_role(ApoRole::Endpoint).max_instances, u32::MAX);
        assert_eq!(for_role(ApoRole::Endpoint).max_instances as i32, -1);
    }
}
