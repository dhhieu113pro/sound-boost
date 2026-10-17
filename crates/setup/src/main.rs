//! Installer for the Sound Booster APO.
//!
//! It does three things, all of them reversible and all of them recorded:
//!
//! 1. registers the two CLSIDs (COM + the engine's APO store),
//! 2. appends them to the effect chain of each render endpoint,
//! 3. sets `DisableProtectedAudioDG`, without which a registry-only APO is not
//!    loaded at all (see `docs/reference-implementation.md`).
//!
//! Every value it overwrites is copied into
//! `HKLM\SOFTWARE\Sound Booster\Backup` first, so `uninstall` restores the
//! machine rather than approximating it. `--dry-run` prints the whole plan and
//! writes nothing.
//!
//! Nothing here needs the audio engine to be running; that is what `status` and
//! the reload at the end of `install` are for.

mod endpoints;
mod plan;
mod win;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sb_apo::registration::ApoRegistration;
use sb_apo::APO_REGISTRATIONS;
use windows_sys::Win32::System::Registry::{KEY_QUERY_VALUE, KEY_SET_VALUE};

use endpoints::RenderEndpoint;
use plan::{Plan, Value};
use win::Key;

/// Where the APO lives once installed. A registered CLSID names an absolute
/// path, so the DLL has to sit somewhere stable rather than in `target\`.
const INSTALL_DIR: &str = r"C:\Program Files\Sound Booster";
const DLL_NAME: &str = "sb_apo.dll";

/// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio\DisableProtectedAudioDG`.
const PROTECTION_VALUE: &str = "DisableProtectedAudioDG";

/// Bookkeeping values in our own backup key.
const MARKER_INSTALLED: &str = "Installed";
const MARKER_DLL_PATH: &str = "DllPath";
const MARKER_PROTECTION_ABSENT: &str = "DisableProtectedAudioDG.wasAbsent";

const USAGE: &str = "\
Sound Booster APO installer

USAGE:
  sb-setup status                 what is registered and where it is loaded
  sb-setup install [options]      register the APO and attach it to endpoints
  sb-setup uninstall [options]    restore the machine

OPTIONS:
  --dll <path>       the sb_apo.dll to install (default: next to this program)
  --endpoint <guid>  only this endpoint (repeatable)
  --all              every endpoint, not just the active ones
  --replace          take over the effect slots instead of appending to them
  --dry-run          print the plan and change nothing
  --no-restart       do not restart the audio service
  -h, --help         this text

By default the installer appends our CLSID to the chains the device driver
built, so the vendor's own effects keep running. `--replace` writes our CLSID
alone instead -- what the reference product does -- which is the only shape the
engine has been observed to honour in the endpoint slot. Either way the original
values are backed up and `uninstall` puts them back.

An install must run elevated: the effect chain and the APO store are
machine-wide keys that administrators may write but not create.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Status,
    Install,
    Uninstall,
}

struct Options {
    command: Command,
    dll: Option<PathBuf>,
    endpoints: Vec<String>,
    all: bool,
    replace: bool,
    dry_run: bool,
    restart: bool,
}

fn main() -> ExitCode {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        // `--help`
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("error: {error}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let result = match options.command {
        Command::Status => status(),
        Command::Install => install(&options),
        Command::Uninstall => uninstall(&options),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\nfailed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut args = args.peekable();
    let command = match args.next().as_deref() {
        Some("status") => Command::Status,
        Some("install") => Command::Install,
        Some("uninstall") => Command::Uninstall,
        Some("-h" | "--help" | "help") | None => return Ok(None),
        Some(other) => return Err(format!("unknown command {other:?}")),
    };

    let mut options = Options {
        command,
        dll: None,
        endpoints: Vec::new(),
        all: false,
        replace: false,
        dry_run: false,
        restart: true,
    };

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--dll" => {
                let value = args.next().ok_or("--dll needs a path")?;
                options.dll = Some(PathBuf::from(value));
            }
            "--endpoint" => {
                let value = args.next().ok_or("--endpoint needs a GUID")?;
                options.endpoints.push(value);
            }
            "--all" => options.all = true,
            "--replace" => options.replace = true,
            "--dry-run" => options.dry_run = true,
            "--no-restart" => options.restart = false,
            "-h" | "--help" => return Ok(None),
            other => return Err(format!("unknown option {other:?}")),
        }
    }

    if command == Command::Status
        && (options.all || options.dry_run || !options.endpoints.is_empty())
    {
        return Err("status takes no options".to_string());
    }

    Ok(Some(options))
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn status() -> io::Result<()> {
    println!(
        "write access to HKLM\\{}: {}",
        win::AUDIO_KEY,
        yes_no(win::can_write_audio_settings())
    );
    match win::read_values(win::AUDIO_KEY)? {
        Some(values) => match values.get(PROTECTION_VALUE) {
            Some(value) => println!("  {PROTECTION_VALUE} = {}", value.render()),
            None => println!("  {PROTECTION_VALUE} is absent (a registry-only APO will not load)"),
        },
        None => println!("  the audio key does not exist"),
    }

    println!("\nregistrations");
    for registration in APO_REGISTRATIONS.iter() {
        let clsid = &registration.clsid;
        let inproc = win::read_values(&format!(
            r"{}\CLSID\{}\InprocServer32",
            win::CLASSES_KEY,
            clsid
        ))?;
        match inproc {
            Some(values) => {
                let path = values.get("").and_then(as_text).unwrap_or_default();
                let present = Path::new(&path).is_file();
                println!("  {clsid}  {}", registration.friendly_name);
                println!(
                    "    server        : {path}{}",
                    if present { "" } else { "  (missing!)" }
                );
            }
            None => println!("  {clsid}  not registered"),
        }

        let store = win::read_values(&store_path(registration))?;
        match store {
            Some(values) => {
                let flags = values.get("Flags").and_then(as_u32).unwrap_or_default();
                let interfaces = values
                    .get("NumAPOInterfaces")
                    .and_then(as_u32)
                    .unwrap_or_default();
                let name = values
                    .get("FriendlyName")
                    .and_then(as_text)
                    .unwrap_or_default();
                let agrees = name == registration.friendly_name
                    && flags == registration.flags
                    && interfaces == registration.interfaces.len() as u32;
                println!(
                    "    store       : {name:?} flags {flags:#04x} interfaces {interfaces}{}",
                    if agrees {
                        ""
                    } else {
                        "  (does not match this build!)"
                    }
                );
            }
            None => println!("    store       : absent (the engine will not load it)"),
        }
    }

    println!("\nrender endpoints");
    let endpoints = endpoints::render_endpoints()?;
    for endpoint in &endpoints {
        report_endpoint(endpoint)?;
    }

    println!("\naudiodg.exe");
    match win::find_process("audiodg.exe")? {
        Some(pid) => match win::modules_of(pid) {
            Ok(modules) => {
                let loaded = modules
                    .iter()
                    .find(|name| name.eq_ignore_ascii_case(DLL_NAME));
                println!("  pid {pid}, {} modules", modules.len());
                match loaded {
                    Some(name) => println!("  {name} IS loaded: the APO is running"),
                    None => {
                        println!("  {DLL_NAME} is NOT loaded (play something, then look again)")
                    }
                }
            }
            Err(error) => println!(
                "  pid {pid}, but its modules cannot be listed ({error}); \
                 audiodg may be a protected process"
            ),
        },
        None => println!("  not running (it starts when audio plays)"),
    }
    Ok(())
}

fn report_endpoint(endpoint: &RenderEndpoint) -> io::Result<()> {
    let fx = endpoint.fx_values()?;
    let mut slots = Vec::new();
    for registration in APO_REGISTRATIONS.iter() {
        let ours = registration.clsid.to_string();
        for (name, value) in &fx {
            if value
                .chain()
                .iter()
                .any(|entry| entry.eq_ignore_ascii_case(&ours))
            {
                slots.push(name.clone());
            }
        }
    }
    let backup = backup_state(&endpoint.id)?;
    println!("  {}  [{}]", endpoint.label(), endpoint.id);
    println!(
        "    effects     : {}{}",
        if slots.is_empty() {
            "ours not present".to_string()
        } else {
            format!("ours in {}", slots.join(", "))
        },
        if endpoint.has_fx {
            ""
        } else {
            "  (no FxProperties key)"
        }
    );
    if let Some(disabled) = endpoint.sysfx_disabled {
        if disabled {
            println!("    all enhancements are DISABLED on this endpoint, so no APO loads");
        }
    }
    if let Some(state) = backup {
        match &state.installed_from {
            Some(dll) => println!(
                "    backup      : {} slot value(s) recorded, installed from {}",
                state.slots.len(),
                if dll.is_empty() { "(unknown)" } else { dll }
            ),
            None => println!("    backup      : present but without our marker"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

/// Read both files back and compare them.
///
/// "The DLL is deployed" should be a fact rather than an assumption: the one
/// time this went wrong, the copy had been refused because the engine had the
/// file mapped, nothing said so, and a whole sweep was read off the build that
/// was already there.
fn files_match(left: &Path, right: &Path) -> io::Result<bool> {
    Ok(std::fs::read(left)? == std::fs::read(right)?)
}

fn install(options: &Options) -> io::Result<()> {
    let source = match &options.dll {
        Some(path) => path.clone(),
        None => default_dll()?,
    };
    if !source.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} does not exist (build it, or pass --dll)",
                source.display()
            ),
        ));
    }

    let elevated = win::can_write_audio_settings();
    println!(
        "write access to HKLM\\{}: {}",
        win::AUDIO_KEY,
        yes_no(elevated)
    );
    if !elevated && !options.dry_run {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the effect chain and the APO store are machine-wide; run this from an elevated prompt",
        ));
    }

    let target = Path::new(INSTALL_DIR).join(DLL_NAME);
    let target_text = target.display().to_string();

    println!("\nfiles");
    if source == target {
        println!("  {target_text} is already in place");
    } else {
        println!("  copy {} -> {target_text}", source.display());
        if !options.dry_run {
            std::fs::create_dir_all(INSTALL_DIR)?;
            if let Err(error) = std::fs::copy(&source, &target) {
                // A file the engine has mapped cannot be replaced. Retrying
                // after a bounce is the difference between deploying this build
                // and silently leaving the previous one registered, which is how
                // an entire experiment got analysed against the wrong DLL.
                println!("    {error}");
                println!("    the audio engine maps the DLL while it runs; stopping the stack and retrying");
                for line in win::stop_audio_service() {
                    println!("    {line}");
                }
                std::fs::copy(&source, &target).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "could not copy {0} to {1}: {error}\n\
                             the file is in use; restart the audio service first \
                             (or run with --no-restart anyway and reboot)",
                            source.display(),
                            target_text
                        ),
                    )
                })?;
                // The chains and registrations below have not been written yet,
                // so bringing the stack back now costs nothing.
                for line in win::restart_audio_service() {
                    println!("    {line}");
                }
            }
            if files_match(&source, &target)? {
                println!(
                    "    {} bytes in place, verified identical to the source",
                    std::fs::metadata(&target)?.len()
                );
            } else {
                return Err(io::Error::other(format!(
                    "{target_text} does not match {} after copying",
                    source.display()
                )));
            }
        }
    }

    println!("\nregistrations");
    for registration in APO_REGISTRATIONS.iter() {
        println!("  {}  {}", registration.clsid, registration.friendly_name);
        println!(
            "    server      : HKLM\\{}\\CLSID\\{}",
            win::CLASSES_KEY,
            registration.clsid
        );
        println!(
            "    store       : HKLM\\{}\\{} flags {:#04x} interfaces {}",
            win::CLASSES_KEY,
            sb_apo::APO_STORE_KEY,
            registration.flags,
            registration.interfaces.len()
        );
        if !options.dry_run {
            register(registration, &target_text)?;
        }
    }

    println!("\nprocess protection");
    set_protection(options)?;

    println!("\neffect chains");
    let targets = select_endpoints(&options.endpoints, options.all, true)?;
    if targets.is_empty() {
        println!("  no endpoints selected");
    }
    for endpoint in targets {
        println!("  {}  [{}]", endpoint.label(), endpoint.id);
        if !endpoint.has_fx {
            println!(
                "    skipped: no FxProperties key (this endpoint has no effects chain to extend)"
            );
            continue;
        }
        let existing = endpoint.fx_values()?;
        let plan = plan::plan_install(
            &endpoint.id,
            &existing,
            &APO_REGISTRATIONS,
            if options.replace {
                plan::Policy::Replace
            } else {
                plan::Policy::Append
            },
        );
        for note in &plan.notes {
            println!("    {note}");
        }
        if plan.is_empty() {
            println!("    nothing to do");
            continue;
        }
        if !options.dry_run {
            apply(&plan)?;
            save_backup(&endpoint.id, &plan.backup, &target_text)?;
        }
    }

    restart(options);

    println!(
        "\n{}",
        if options.dry_run {
            "dry run: nothing was written"
        } else {
            "installed"
        }
    );
    if !options.dry_run {
        println!(
            "verify with `sb-setup status` while something is playing, or with \
             `sb-setup status` after the effect has been idle"
        );
    }
    Ok(())
}

fn register(registration: &ApoRegistration, dll_path: &str) -> io::Result<()> {
    let clsid_key = Key::create(&clsid_path(registration))?;
    clsid_key.set(None, &Value::Sz(registration.friendly_name.to_string()))?;

    let inproc = Key::create(&format!(r"{}\InprocServer32", clsid_path(registration)))?;
    inproc.set(None, &Value::Sz(dll_path.to_string()))?;
    // Both: the engine creates and calls us from several threads.
    inproc.set(Some("ThreadingModel"), &Value::Sz("Both".to_string()))?;

    let store = Key::create(&store_path(registration))?;
    store.set(
        Some("FriendlyName"),
        &Value::Sz(registration.friendly_name.to_string()),
    )?;
    store.set(
        Some("Copyright"),
        &Value::Sz(registration.copyright.to_string()),
    )?;
    store.set(
        Some("MajorVersion"),
        &Value::Dword(registration.major_version),
    )?;
    store.set(
        Some("MinorVersion"),
        &Value::Dword(registration.minor_version),
    )?;
    store.set(Some("Flags"), &Value::Dword(registration.flags))?;
    store.set(
        Some("MinInputConnections"),
        &Value::Dword(registration.min_input_connections),
    )?;
    store.set(
        Some("MaxInputConnections"),
        &Value::Dword(registration.max_input_connections),
    )?;
    store.set(
        Some("MinOutputConnections"),
        &Value::Dword(registration.min_output_connections),
    )?;
    store.set(
        Some("MaxOutputConnections"),
        &Value::Dword(registration.max_output_connections),
    )?;
    // `u32::MAX` in a REG_DWORD is what the reference product stores as -1.
    store.set(
        Some("MaxInstances"),
        &Value::Dword(registration.max_instances),
    )?;
    store.set(
        Some("NumAPOInterfaces"),
        &Value::Dword(registration.interfaces.len() as u32),
    )?;
    for (index, iid) in registration.interfaces.iter().enumerate() {
        store.set(
            Some(&format!("APOInterface{index}")),
            &Value::Sz(iid.to_string()),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

fn uninstall(options: &Options) -> io::Result<()> {
    let elevated = win::can_write_audio_settings();
    println!(
        "write access to HKLM\\{}: {}",
        win::AUDIO_KEY,
        yes_no(elevated)
    );
    if !elevated && !options.dry_run {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "run this from an elevated prompt",
        ));
    }

    println!("\neffect chains");
    let targets = select_endpoints(&options.endpoints, options.all, false)?;
    for endpoint in targets {
        let existing = endpoint.fx_values()?;
        let backup = backup_values(&endpoint.id)?;
        let plan = plan::plan_uninstall(
            &endpoint.id,
            &existing,
            backup.as_ref().unwrap_or(&BTreeMap::new()),
            &APO_REGISTRATIONS,
        );
        if plan.is_empty() {
            continue;
        }
        println!("  {}  [{}]", endpoint.label(), endpoint.id);
        for note in &plan.notes {
            println!("    {note}");
        }
        if !options.dry_run {
            apply(&plan)?;
            if backup.is_some() {
                win::delete_tree(&backup_path(&endpoint.id))?;
            }
        }
    }

    println!("\nregistrations");
    for registration in APO_REGISTRATIONS.iter() {
        println!("  remove {}", registration.clsid);
        if !options.dry_run {
            win::delete_tree(&clsid_path(registration))?;
            win::delete_tree(&store_path(registration))?;
        }
    }

    println!("\nprocess protection");
    restore_protection(options)?;

    restart(options);

    println!("\nfiles");
    let target = Path::new(INSTALL_DIR).join(DLL_NAME);
    if !options.dry_run {
        match std::fs::remove_file(&target) {
            Ok(()) => println!("  removed {}", target.display()),
            Err(error) => println!(
                "  could not remove {} ({error}); reboot and run uninstall again",
                target.display()
            ),
        }
        let _ = std::fs::remove_dir(INSTALL_DIR);
    } else {
        println!("  would remove {}", target.display());
    }

    println!(
        "\n{}",
        if options.dry_run {
            "dry run: nothing was changed"
        } else {
            "uninstalled"
        }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// pieces
// ---------------------------------------------------------------------------

fn set_protection(options: &Options) -> io::Result<()> {
    // A dry run must not need write access to the key it is describing.
    let access = match options.dry_run {
        true => KEY_QUERY_VALUE,
        false => KEY_QUERY_VALUE | KEY_SET_VALUE,
    };
    let key = Key::open(win::AUDIO_KEY, access)?;
    let current = key.get(Some(PROTECTION_VALUE))?;
    if current == Some(Value::Dword(1)) {
        println!("  {PROTECTION_VALUE} is already 1");
        return Ok(());
    }

    println!(
        "  {PROTECTION_VALUE}: {} -> 1",
        current.as_ref().map_or("absent".to_string(), Value::render)
    );
    println!("    required for an APO that is not installed as a driver component");
    if options.dry_run {
        return Ok(());
    }

    // Record the original exactly once, so a previous install cannot overwrite
    // the real original with our own 1.
    let backup = Key::create(win::BACKUP_KEY)?;
    if backup.get(Some(PROTECTION_VALUE))?.is_none()
        && backup.get(Some(MARKER_PROTECTION_ABSENT))?.is_none()
    {
        match &current {
            Some(value) => backup.set(Some(PROTECTION_VALUE), value)?,
            None => backup.set(Some(MARKER_PROTECTION_ABSENT), &Value::Dword(1))?,
        }
    }

    key.set(Some(PROTECTION_VALUE), &Value::Dword(1))?;
    Ok(())
}

fn restore_protection(options: &Options) -> io::Result<()> {
    let Some(backup) = win::read_values(win::BACKUP_KEY)? else {
        println!("  nothing recorded, left alone");
        return Ok(());
    };

    if backup.contains_key(MARKER_PROTECTION_ABSENT) {
        println!("  {PROTECTION_VALUE} was absent before; removing it");
        if !options.dry_run {
            let key = Key::open(win::AUDIO_KEY, KEY_QUERY_VALUE | KEY_SET_VALUE)?;
            key.delete_value(Some(PROTECTION_VALUE))?;
        }
    } else if let Some(original) = backup.get(PROTECTION_VALUE) {
        println!("  {PROTECTION_VALUE}: restore {}", original.render());
        if !options.dry_run {
            let key = Key::open(win::AUDIO_KEY, KEY_QUERY_VALUE | KEY_SET_VALUE)?;
            key.set(Some(PROTECTION_VALUE), original)?;
        }
    } else {
        println!("  no original value recorded, left alone");
    }

    if !options.dry_run {
        let key = Key::create(win::BACKUP_KEY)?;
        key.delete_value(Some(PROTECTION_VALUE))?;
        key.delete_value(Some(MARKER_PROTECTION_ABSENT))?;
    }
    Ok(())
}

/// Write a plan to the endpoint's `FxProperties` key.
///
/// Administrators have `SetValue` but not `CreateSubKey` there, which is why
/// this opens rather than creates: if the key is missing the endpoint simply
/// has no effect chain to extend, and the caller reports that.
fn apply(plan: &Plan) -> io::Result<()> {
    let key = Key::open(
        &format!(r"{}\{}\FxProperties", win::RENDER_KEY, plan.endpoint),
        KEY_QUERY_VALUE | KEY_SET_VALUE,
    )?;
    for (name, value) in &plan.writes {
        key.set(Some(name), value)?;
    }
    for name in &plan.deletes {
        key.delete_value(Some(name))?;
    }
    Ok(())
}

fn save_backup(endpoint: &str, backup: &BTreeMap<String, Value>, dll_path: &str) -> io::Result<()> {
    let key = Key::create(&backup_path(endpoint))?;
    for (name, value) in backup {
        key.set(Some(name), value)?;
    }
    key.set(Some(MARKER_INSTALLED), &Value::Dword(1))?;
    key.set(Some(MARKER_DLL_PATH), &Value::Sz(dll_path.to_string()))?;
    Ok(())
}

fn backup_values(endpoint: &str) -> io::Result<Option<BTreeMap<String, Value>>> {
    Ok(backup_state(endpoint)?.map(|state| state.slots))
}

/// What our backup key says about one endpoint.
struct BackupState {
    /// The slot values we would restore, with our own bookkeeping removed.
    /// `Installed` and `DllPath` must never reach the FX key.
    slots: BTreeMap<String, Value>,
    /// Set when the marker is there: `Some(dll)` as recorded at install time.
    installed_from: Option<String>,
}

fn backup_state(endpoint: &str) -> io::Result<Option<BackupState>> {
    let Some(values) = win::read_values(&backup_path(endpoint))? else {
        return Ok(None);
    };
    let dll = values.get(MARKER_DLL_PATH).and_then(as_text);
    let installed = values.contains_key(MARKER_INSTALLED);
    Ok(Some(BackupState {
        slots: values
            .into_iter()
            .filter(|(name, _)| name != MARKER_INSTALLED && name != MARKER_DLL_PATH)
            .collect(),
        installed_from: installed.then(|| dll.unwrap_or_default()),
    }))
}

/// Which endpoints to work on: the named ones, or (by default) the active ones
/// that already have our CLSID or a backup.
fn select_endpoints(
    named: &[String],
    all: bool,
    installing: bool,
) -> io::Result<Vec<RenderEndpoint>> {
    let endpoints = endpoints::render_endpoints()?;
    let mut selected = Vec::new();
    for endpoint in endpoints {
        if !named.is_empty() {
            if named.iter().any(|id| id.eq_ignore_ascii_case(&endpoint.id)) {
                selected.push(endpoint);
            }
            continue;
        }
        if installing {
            if all || endpoint.is_active() {
                selected.push(endpoint);
            }
            continue;
        }
        // Uninstalling: anything we may have touched, active or not.
        let ours_or_backed_up = backup_values(&endpoint.id)?.is_some()
            || endpoint.fx_values()?.values().any(|value| {
                value.chain().iter().any(|entry| {
                    APO_REGISTRATIONS.iter().any(|registration| {
                        entry.eq_ignore_ascii_case(&registration.clsid.to_string())
                    })
                })
            });
        if all || ours_or_backed_up {
            selected.push(endpoint);
        }
    }
    Ok(selected)
}

fn restart(options: &Options) {
    println!("\naudio service");
    if !options.restart {
        println!("  skipped (--no-restart); reboot or restart the audio service to apply");
        return;
    }
    if options.dry_run {
        println!("  would restart Audiosrv");
        return;
    }
    for line in win::restart_audio_service() {
        println!("  {line}");
    }
}

fn default_dll() -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(DLL_NAME))
}

fn clsid_path(registration: &ApoRegistration) -> String {
    format!(r"{}\CLSID\{}", win::CLASSES_KEY, registration.clsid)
}

fn store_path(registration: &ApoRegistration) -> String {
    format!(
        r"{}\{}\{}",
        win::CLASSES_KEY,
        sb_apo::APO_STORE_KEY,
        registration.clsid
    )
}

fn backup_path(endpoint: &str) -> String {
    format!(r"{}\Backup\FxProperties\{endpoint}", win::BACKUP_KEY)
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::Sz(text) | Value::ExpandSz(text) => Some(text.clone()),
        Value::MultiSz(entries) => entries.first().cloned(),
        _ => None,
    }
}

fn as_u32(value: &Value) -> Option<u32> {
    match value {
        Value::Dword(number) => Some(*number),
        _ => None,
    }
}

fn yes_no(flag: bool) -> &'static str {
    if flag {
        "yes"
    } else {
        "no"
    }
}
