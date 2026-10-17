//! What `install` changes and `uninstall` puts back, as pure data.
//!
//! None of this touches the registry. That is deliberate: the FX key is a
//! hostile place to experiment in (administrators may set values there but may
//! not create or delete the subkey), so every decision is made here first, as a
//! plain function over the values that are already there, and only then
//! applied. It also means the rules are testable without an audio device.
//!
//! The important rule is that these values are **chains** (`REG_MULTI_SZ`):
//! index 14 on a real machine holds two vendor APOs, and the same shape applies
//! to the slots we use. So we append our CLSID to what is already in the slot
//! and remove exactly our own entry on the way out, rather than replacing the
//! chain the way the reference product does.

use std::collections::BTreeMap;

use sb_apo::registration::ApoRegistration;
use windows_sys::Win32::System::Registry::{
    REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_NONE, REG_SZ,
};

/// The property set the effect chain lives in
/// (`PKEY_FX_*`, `audioenginebaseapo.idl`).
pub const FX_PROPERTY_SET: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}";

/// Composite FX stream slot (13), then classic stream slot (5).
///
/// Composite first because that is the pipeline a modern endpoint actually
/// uses: on this machine 13 and 14 are populated and 5/6/7 are absent, so an
/// installer that only wrote the classic slots would appear to do nothing.
/// Index 14 (MFX) is deliberately absent from both lists: leveling the
/// post-mix bus would let one loud app mask a quiet one.
const STREAM_SLOTS: [u32; 2] = [13, 5];

/// Composite FX endpoint slot (15), then classic endpoint slot (7).
const ENDPOINT_SLOTS: [u32; 2] = [15, 7];

/// A registry value, in the shapes the FX key is known to use.
///
/// Unknown types are kept byte-exact so that a restore writes back precisely
/// what was there, including things we have no interpretation for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Sz(String),
    ExpandSz(String),
    MultiSz(Vec<String>),
    Dword(u32),
    Binary(Vec<u8>),
    None,
    Raw { kind: u32, bytes: Vec<u8> },
}

impl Value {
    pub fn decode(kind: u32, bytes: &[u8]) -> Self {
        match kind {
            REG_SZ => Value::Sz(decode_string(bytes)),
            REG_EXPAND_SZ => Value::ExpandSz(decode_string(bytes)),
            REG_MULTI_SZ => Value::MultiSz(decode_multi_string(bytes)),
            REG_DWORD if bytes.len() >= 4 => {
                Value::Dword(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            REG_BINARY => Value::Binary(bytes.to_vec()),
            REG_NONE => Value::None,
            other => Value::Raw {
                kind: other,
                bytes: bytes.to_vec(),
            },
        }
    }

    /// The wire form: type tag plus raw bytes.
    pub fn encode(&self) -> (u32, Vec<u8>) {
        match self {
            Value::Sz(text) => (REG_SZ, encode_string(text)),
            Value::ExpandSz(text) => (REG_EXPAND_SZ, encode_string(text)),
            Value::MultiSz(entries) => (REG_MULTI_SZ, encode_multi_string(entries)),
            Value::Dword(number) => (REG_DWORD, number.to_le_bytes().to_vec()),
            Value::Binary(bytes) => (REG_BINARY, bytes.clone()),
            Value::None => (REG_NONE, Vec::new()),
            Value::Raw { kind, bytes } => (*kind, bytes.clone()),
        }
    }

    /// The entries of a chain value, whatever shape it arrived in.
    pub fn chain(&self) -> Vec<String> {
        match self {
            Value::MultiSz(entries) => entries.clone(),
            Value::Sz(text) | Value::ExpandSz(text) if !text.is_empty() => vec![text.clone()],
            _ => Vec::new(),
        }
    }

    pub fn render(&self) -> String {
        match self {
            Value::Sz(text) => format!("REG_SZ {text}"),
            Value::ExpandSz(text) => format!("REG_EXPAND_SZ {text}"),
            Value::MultiSz(entries) => format!("REG_MULTI_SZ {}", entries.join(" | ")),
            Value::Dword(number) => format!("REG_DWORD {number}"),
            Value::Binary(bytes) => format!("REG_BINARY {} bytes", bytes.len()),
            Value::None => "REG_NONE".to_string(),
            Value::Raw { kind, bytes } => format!("type {kind}, {} bytes", bytes.len()),
        }
    }
}

/// The registry value name for one FX slot: `{d04e05a6-...},13`.
pub fn slot_name(index: u32) -> String {
    format!("{FX_PROPERTY_SET},{index}")
}

/// How `install` treats a slot that already holds somebody else's CLSID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Put our CLSID at the end of the existing chain, so the device's own
    /// effects keep running. This is the polite default: we are a guest in a
    /// chain the driver built.
    Append,
    /// Replace the chain with our CLSID alone, remembering the old value for
    /// uninstall. This is what the reference product does (`OldAPOs`), and on
    /// this machine it turned out to be what the engine actually honours for
    /// the endpoint slot: with our CLSID appended after DTS's and Realtek's,
    /// the engine built its chain without ever asking for ours.
    Replace,
}

/// One set of changes to one endpoint.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub endpoint: String,
    /// Values to write, in order.
    pub writes: Vec<(String, Value)>,
    /// Values to remove.
    pub deletes: Vec<String>,
    /// Original contents of every value we change, for the backup key. A name
    /// that is absent here did not exist before us, so uninstall deletes it.
    pub backup: BTreeMap<String, Value>,
    /// Human-readable explanation, printed by `--dry-run` and by `install`.
    pub notes: Vec<String>,
}

impl Plan {
    fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.deletes.is_empty()
    }
}

/// The chain we want a slot to hold, given what is there now.
fn with_clsid(current: Option<&Value>, clsid: &str, policy: Policy) -> Value {
    let mut chain = match policy {
        Policy::Append => current.map(Value::chain).unwrap_or_default(),
        Policy::Replace => Vec::new(),
    };
    if !contains(&chain, clsid) {
        chain.push(clsid.to_string());
    }
    Value::MultiSz(chain)
}

/// What an `Append` install of `clsid` over `current` would look like, for
/// `uninstall` to recognise its own work.
fn appended(current: &Value, clsid: &str) -> Value {
    with_clsid(Some(current), clsid, Policy::Append)
}

fn without_clsid(current: &Value, clsid: &str) -> Vec<String> {
    current
        .chain()
        .into_iter()
        .filter(|entry| !entry.eq_ignore_ascii_case(clsid))
        .collect()
}

fn contains(chain: &[String], clsid: &str) -> bool {
    chain.iter().any(|entry| entry.eq_ignore_ascii_case(clsid))
}

/// The slot this endpoint is using for one stage, creating the composite one if
/// the endpoint has no FX configuration at all.
fn choose_slot(existing: &BTreeMap<String, Value>, slots: [u32; 2]) -> String {
    for index in slots {
        let name = slot_name(index);
        if existing.contains_key(&name) {
            return name;
        }
    }
    slot_name(slots[0])
}

/// What `install` writes for one endpoint.
pub fn plan_install(
    endpoint: &str,
    existing: &BTreeMap<String, Value>,
    stages: &[ApoRegistration; 2],
    policy: Policy,
) -> Plan {
    let mut plan = Plan::new(endpoint);
    for (registration, slots) in stages.iter().zip([STREAM_SLOTS, ENDPOINT_SLOTS]) {
        let name = choose_slot(existing, slots);
        let clsid = registration.clsid.to_string();
        let current = existing.get(&name);
        let holds_ours = current.is_some_and(|value| contains(&value.chain(), &clsid));
        let nothing_to_do = match policy {
            // Appending to a chain that already has us would be a no-op.
            Policy::Append => holds_ours,
            // Replacing only has nothing to do if ours is all there is.
            Policy::Replace => current.is_some_and(|value| value.chain().len() == 1) && holds_ours,
        };

        if nothing_to_do {
            plan.notes
                .push(format!("{name}: already contains {clsid}, left as is"));
            continue;
        }

        plan.notes.push(format!(
            "{name}: {} -> {} {clsid}",
            current.map_or("absent".to_string(), Value::render),
            match policy {
                Policy::Append => "append",
                Policy::Replace => "replace with",
            }
        ));
        // Never back up our own value over a real one: a second `install` over a
        // live install must not make uninstall restore our CLSID.
        if !holds_ours {
            if let Some(previous) = current {
                plan.backup.insert(name.clone(), previous.clone());
            }
        }
        plan.writes
            .push((name, with_clsid(current, &clsid, policy)));
    }
    plan
}

/// What `uninstall` writes for one endpoint, given the backup that was taken
/// when it was installed.
pub fn plan_uninstall(
    endpoint: &str,
    existing: &BTreeMap<String, Value>,
    backup: &BTreeMap<String, Value>,
    stages: &[ApoRegistration; 2],
) -> Plan {
    let mut plan = Plan::new(endpoint);
    for registration in stages {
        let clsid = registration.clsid.to_string();
        // Look for our CLSID anywhere it ended up rather than only where we
        // would put it today: the endpoint may have switched pipelines (or the
        // vendor driver may have been reinstalled) since we installed.
        for (name, value) in existing {
            if !contains(&value.chain(), &clsid) {
                continue;
            }
            match backup.get(name) {
                // Exactly what we wrote and nothing else: put the original back.
                Some(previous) if *value == appended(previous, &clsid) => {
                    plan.notes
                        .push(format!("{name}: restore {}", previous.render()));
                    plan.writes.push((name.clone(), previous.clone()));
                }
                // Someone edited the chain after us. Take out our entry only.
                _ => {
                    let remainder = without_clsid(value, &clsid);
                    if remainder.is_empty() {
                        match backup.get(name) {
                            Some(previous) => {
                                plan.notes
                                    .push(format!("{name}: restore {}", previous.render()));
                                plan.writes.push((name.clone(), previous.clone()));
                            }
                            None => {
                                plan.notes.push(format!("{name}: remove our entry"));
                                plan.deletes.push(name.clone());
                            }
                        }
                    } else {
                        plan.notes.push(format!(
                            "{name}: remove our entry, keep {}",
                            remainder.len()
                        ));
                        plan.writes.push((name.clone(), Value::MultiSz(remainder)));
                    }
                }
            }
        }
    }
    plan
}

fn decode_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn decode_multi_string(bytes: &[u8]) -> Vec<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let mut entries = Vec::new();
    let mut current = Vec::new();
    for unit in units {
        if unit == 0 {
            if current.is_empty() {
                break;
            }
            entries.push(String::from_utf16_lossy(&current));
            current.clear();
        } else {
            current.push(unit);
        }
    }
    entries
}

fn encode_string(text: &str) -> Vec<u8> {
    let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

fn encode_multi_string(entries: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in entries {
        bytes.extend(entry.encode_utf16().flat_map(u16::to_le_bytes));
        bytes.extend_from_slice(&[0, 0]);
    }
    // A MULTI_SZ list is terminated by an extra NUL, which for an empty list is
    // the only NUL there is.
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_apo::APO_REGISTRATIONS;

    fn chain(entries: &[&str]) -> Value {
        Value::MultiSz(entries.iter().map(|entry| entry.to_string()).collect())
    }

    fn endpoint(values: &[(&str, Value)]) -> BTreeMap<String, Value> {
        values
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    const OURS: [&str; 2] = [
        "{A7C3E1D9-4B62-4F38-9E57-1C8D2A6B0F41}",
        "{A7C3E1D9-4B62-4F38-9E57-1C8D2A6B0F42}",
    ];
    const REALTEK: &str = "{905069CC-CF0D-4EAD-B7D7-FBC5A9E38BD5}";

    #[test]
    fn install_appends_to_the_composite_slots_the_endpoint_already_uses() {
        let existing = endpoint(&[
            (&slot_name(13), chain(&[REALTEK])),
            (
                &slot_name(14),
                chain(&[
                    "{FCDD3010-BB3F-4B24-8074-A18FC7D558F7}",
                    "{90609662-991C-4873-BA68-8FF2D57B230B}",
                ]),
            ),
            (
                &slot_name(15),
                chain(&["{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}"]),
            ),
        ]);
        let plan = plan_install("{endpoint}", &existing, &APO_REGISTRATIONS, Policy::Append);

        // The vendor's own stream effect is still first in the chain.
        assert_eq!(
            plan.writes,
            vec![
                (slot_name(13), chain(&[REALTEK, OURS[0]])),
                (
                    slot_name(15),
                    chain(&["{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}", OURS[1]])
                ),
            ]
        );
        // MFX (14) is not ours to touch.
        assert!(!plan.writes.iter().any(|(name, _)| *name == slot_name(14)));
        // Both slots are backed up so uninstall can put the chains back.
        assert_eq!(plan.backup.len(), 2);
        assert_eq!(plan.backup[&slot_name(13)], existing[&slot_name(13)]);
    }

    #[test]
    fn install_uses_the_classic_slots_when_that_is_what_the_endpoint_has() {
        let existing = endpoint(&[
            (&slot_name(5), chain(&[REALTEK])),
            (&slot_name(7), chain(&[])),
        ]);
        let plan = plan_install("{endpoint}", &existing, &APO_REGISTRATIONS, Policy::Append);
        assert_eq!(
            plan.writes,
            vec![
                (slot_name(5), chain(&[REALTEK, OURS[0]])),
                (slot_name(7), chain(&[OURS[1]])),
            ]
        );
    }

    #[test]
    fn install_creates_the_composite_slots_on_an_endpoint_with_no_effects() {
        let plan = plan_install("{endpoint}", &BTreeMap::new(), &APO_REGISTRATIONS, Policy::Append);
        assert_eq!(
            plan.writes,
            vec![
                (slot_name(13), chain(&[OURS[0]])),
                (slot_name(15), chain(&[OURS[1]])),
            ]
        );
        // Nothing existed, so uninstall will delete rather than restore.
        assert!(plan.backup.is_empty());
    }

    #[test]
    fn replace_takes_sole_ownership_of_the_slot_and_still_backs_it_up() {
        let existing = endpoint(&[
            (&slot_name(13), chain(&[REALTEK])),
            (
                &slot_name(15),
                chain(&["{599C2638-CC15-4FFE-B16A-9246260D2DF7}", REALTEK]),
            ),
        ]);
        let plan = plan_install(
            "{endpoint}",
            &existing,
            &APO_REGISTRATIONS,
            Policy::Replace,
        );

        assert_eq!(
            plan.writes,
            vec![
                (slot_name(13), chain(&[OURS[0]])),
                (slot_name(15), chain(&[OURS[1]])),
            ]
        );
        // The vendor chains are remembered, so uninstall can put them back.
        assert_eq!(plan.backup[&slot_name(13)], existing[&slot_name(13)]);
        assert_eq!(plan.backup[&slot_name(15)], existing[&slot_name(15)]);

        // And uninstall does put them back, even though the value on disk no
        // longer looks like "the original plus ours".
        let installed = endpoint(&[
            (&slot_name(13), chain(&[OURS[0]])),
            (&slot_name(15), chain(&[OURS[1]])),
        ]);
        let undo = plan_uninstall("{endpoint}", &installed, &plan.backup, &APO_REGISTRATIONS);
        assert_eq!(
            undo.writes,
            vec![
                (slot_name(13), existing[&slot_name(13)].clone()),
                (slot_name(15), existing[&slot_name(15)].clone()),
            ]
        );
    }

    #[test]
    fn a_second_replace_is_a_no_op() {
        let installed = endpoint(&[
            (&slot_name(13), chain(&[OURS[0]])),
            (&slot_name(15), chain(&[OURS[1]])),
        ]);
        let plan = plan_install(
            "{endpoint}",
            &installed,
            &APO_REGISTRATIONS,
            Policy::Replace,
        );
        assert!(plan.is_empty(), "second replace wrote {:?}", plan.writes);
        assert!(plan.backup.is_empty());
    }

    #[test]
    fn replace_rewrites_a_slot_where_ours_is_not_alone() {
        // Half-installed by an earlier append: ours is there, but so is the
        // vendor's APO. Replacing must take the slot over, not skip it.
        let existing = endpoint(&[(&slot_name(13), chain(&[REALTEK, OURS[0]]))]);
        let plan = plan_install(
            "{endpoint}",
            &existing,
            &APO_REGISTRATIONS,
            Policy::Replace,
        );
        assert_eq!(plan.writes[0], (slot_name(13), chain(&[OURS[0]])));
        // Our own value is not backed up as if it were the vendor's.
        assert!(!plan.backup.contains_key(&slot_name(13)));
    }

    #[test]
    fn a_second_install_does_not_back_up_our_own_values() {
        let existing = endpoint(&[
            (&slot_name(13), chain(&[REALTEK])),
            (&slot_name(15), chain(&[])),
        ]);
        let first = plan_install("{endpoint}", &existing, &APO_REGISTRATIONS, Policy::Append);
        let installed = endpoint(&[
            (&slot_name(13), first.writes[0].1.clone()),
            (&slot_name(15), first.writes[1].1.clone()),
        ]);

        let second = plan_install("{endpoint}", &installed, &APO_REGISTRATIONS, Policy::Append);
        assert!(
            second.is_empty(),
            "second install wrote {:?}",
            second.writes
        );
        assert!(
            second.backup.is_empty(),
            "second install would have overwritten the real backup"
        );
    }

    #[test]
    fn uninstall_restores_the_original_chains() {
        let original = endpoint(&[
            (&slot_name(13), chain(&[REALTEK])),
            (
                &slot_name(15),
                chain(&["{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}"]),
            ),
        ]);
        let installed = endpoint(&[
            (&slot_name(13), chain(&[REALTEK, OURS[0]])),
            (
                &slot_name(15),
                chain(&["{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}", OURS[1]]),
            ),
        ]);

        let plan = plan_uninstall("{endpoint}", &installed, &original, &APO_REGISTRATIONS);
        assert_eq!(
            plan.writes,
            vec![
                (slot_name(13), chain(&[REALTEK])),
                (
                    slot_name(15),
                    chain(&["{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}"])
                ),
            ]
        );
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn uninstall_deletes_the_slots_we_created() {
        let existing = endpoint(&[
            (&slot_name(13), chain(&[OURS[0]])),
            (&slot_name(15), chain(&[OURS[1]])),
        ]);
        let plan = plan_uninstall(
            "{endpoint}",
            &existing,
            &BTreeMap::new(),
            &APO_REGISTRATIONS,
        );
        assert!(plan.writes.is_empty());
        assert_eq!(plan.deletes, vec![slot_name(13), slot_name(15)]);
    }

    #[test]
    fn uninstall_keeps_a_chain_that_changed_after_we_installed() {
        // Something else added an effect behind us.
        let existing = endpoint(&[(
            &slot_name(13),
            chain(&[REALTEK, OURS[0], "{11111111-2222-3333-4444-555555555555}"]),
        )]);
        let plan = plan_uninstall(
            "{endpoint}",
            &existing,
            &BTreeMap::new(),
            &APO_REGISTRATIONS,
        );
        assert_eq!(
            plan.writes,
            vec![(
                slot_name(13),
                chain(&[REALTEK, "{11111111-2222-3333-4444-555555555555}"])
            )]
        );
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn uninstall_ignores_an_endpoint_we_never_installed_on() {
        let existing = endpoint(&[(&slot_name(13), chain(&[REALTEK]))]);
        let plan = plan_uninstall(
            "{endpoint}",
            &existing,
            &BTreeMap::new(),
            &APO_REGISTRATIONS,
        );
        assert!(plan.is_empty());
    }

    #[test]
    fn a_single_sz_slot_becomes_a_chain_without_losing_its_entry() {
        // Some endpoints keep a single CLSID in a REG_SZ; appending still works.
        let existing = endpoint(&[(&slot_name(15), Value::Sz(REALTEK.to_string()))]);
        let plan = plan_install("{endpoint}", &existing, &APO_REGISTRATIONS, Policy::Append);
        assert_eq!(plan.writes[1], (slot_name(15), chain(&[REALTEK, OURS[1]])));
    }

    #[test]
    fn chains_round_trip_through_the_wire_format() {
        for value in [
            chain(&["{A}", "{B}"]),
            chain(&[]),
            Value::Sz("hello".to_string()),
            Value::Dword(0x400),
            Value::Binary(vec![1, 2, 3]),
            Value::None,
            Value::Raw {
                kind: 0x1234,
                bytes: vec![9, 9],
            },
        ] {
            let (kind, bytes) = value.encode();
            assert_eq!(Value::decode(kind, &bytes), value, "{value:?}");
        }
    }

    #[test]
    fn a_multi_sz_list_ends_with_an_extra_nul() {
        let (_, bytes) = chain(&["{A}"]).encode();
        // "{A}" is three UTF-16 units, plus its own terminator, plus the extra
        // NUL that ends the list.
        assert_eq!(bytes.len(), 10);
        assert_eq!(&bytes[6..], &[0, 0, 0, 0]);
        assert_eq!(
            Value::decode(REG_MULTI_SZ, &bytes).chain(),
            vec!["{A}".to_string()]
        );
    }
}
