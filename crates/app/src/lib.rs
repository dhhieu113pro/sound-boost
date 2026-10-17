//! The parts of the user interface that are not the user interface.
//!
//! The UI never calls into the APO. It attaches to a shared-memory section that
//! the **APO** creates -- `audiodg.exe` runs in session 0, where creating a
//! `Global\` file mapping needs no privilege, while a UI in the user's session
//! could not create one at all -- and publishes plain numbers into it. See
//! `sb_ipc` and `docs/risks.md` for the reasoning, and `sb-gain` in this crate
//! for the command-line version of the same two calls.

use std::io;
use std::thread::sleep;
use std::time::{Duration, Instant};

use sb_ipc::mapping::MappedControl;
use sb_ipc::{ControlSnapshot, CONTROL_NAME};

/// How long to wait between attempts to attach.
const RETRY: Duration = Duration::from_millis(250);

/// Attach to the control section, waiting for the audio engine to create it.
///
/// A UI that starts before any audio is playing has nothing to attach to: the
/// section only appears once `audiodg.exe` has instantiated an APO, which takes
/// a stream. Waiting is the honest behaviour here -- creating our own section
/// would look like it worked while the engine read a different one, and a
/// non-elevated process cannot create the machine-wide section anyway.
pub fn attach(timeout: Duration) -> io::Result<MappedControl> {
    let deadline = Instant::now() + timeout;
    loop {
        match MappedControl::open() {
            Ok(mapped) => return Ok(mapped),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "{CONTROL_NAME}: {error}\n\
                             the audio engine creates this when it loads the APO, \
                             so play something and try again"
                        ),
                    ));
                }
                sleep(RETRY);
            }
        }
    }
}

/// Publish a snapshot, making the block readable if nobody has yet.
///
/// Write first, stamp second: the APO ignores an unstamped block, so it can
/// never act on the default snapshot that a stamp-first order would expose for
/// one buffer.
pub fn publish(control: &MappedControl, snapshot: &ControlSnapshot) {
    control.block().write(snapshot);
    control.block().stamp();
}

/// One line describing what the APO has been told to do.
pub fn summarize(snapshot: &ControlSnapshot) -> String {
    let params = snapshot.params;
    if !snapshot.enabled {
        return format!(
            "off ({} preset, slider at {} %)",
            snapshot.preset.label(),
            snapshot.manual_percent
        );
    }
    format!(
        "{} preset, slider {} % -> +{:.2} dB manual gain; \
         leveling {:.2} over {:.1}..{:.1} dB; limiter ceiling {:.1} dBFS, {:.1} ms look-ahead",
        snapshot.preset.label(),
        snapshot.manual_percent,
        params.manual_gain_db,
        params.agc.strength,
        params.agc.min_gain_db,
        params.agc.max_gain_db,
        params.limiter.ceiling_db,
        params.limiter.lookahead_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::params::Preset;

    #[test]
    fn a_stopped_chain_says_so() {
        let off = ControlSnapshot::from_ui(Preset::Off, 300, None);
        let text = summarize(&off);
        assert!(text.starts_with("off"), "{text}");
    }

    #[test]
    fn a_running_chain_reports_its_numbers() {
        let on = ControlSnapshot::from_ui(Preset::Music, 400, Some(0.5));
        let text = summarize(&on);
        assert!(text.contains("Music"), "{text}");
        assert!(text.contains("400 %"), "{text}");
        assert!(text.contains("0.50"), "{text}");
    }
}
