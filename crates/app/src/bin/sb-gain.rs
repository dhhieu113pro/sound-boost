//! Set or report the Sound Booster level from a command line.
//!
//! This is the diagnostic twin of the tray app: it attaches to the same control
//! section and publishes the same kind of snapshot. That is what makes it useful
//! before the UI exists -- if `sb-gain 500` changes what you hear, then the APO
//! is registered, loaded into `audiodg.exe`, reading the shared block and
//! processing audio, which is the whole chain in one command.

use std::process::ExitCode;
use std::time::Duration;

use sb_app::{attach, publish, summarize};
use sb_core::params::{Preset, MAX_PERCENT, MIN_PERCENT};
use sb_ipc::ControlSnapshot;

/// Where the slider sits if we have to publish but nobody has said yet.
const DEFAULT_PERCENT: u32 = 250;

const USAGE: &str = "\
sb-gain sets the Sound Booster level through the same control block the tray app
uses, which makes it the tool for checking that the APO is really loaded.

USAGE:
  sb-gain [LEVEL] [options]

ARGS:
  LEVEL              100..=500, the slider position

OPTIONS:
  --preset <name>    off | speech | music | night (default: keep the current one)
  --strength <0..1>  leveling strength (default: keep the current one)
  --off              stop processing (same as --preset off)
  --wait <seconds>   how long to wait for the audio engine (default: 5)
  -h, --help         this text

With neither a LEVEL nor an option it only reports what the block says.";

struct Args {
    percent: Option<u32>,
    preset: Option<Preset>,
    strength: Option<f32>,
    wait: Duration,
    publish: bool,
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let control = match attach(args.wait) {
        Ok(control) => control,
        Err(error) => {
            eprintln!("{error}");
            eprintln!(
                "if that is unexpected, `sb-setup status` says whether the APO is registered"
            );
            return ExitCode::FAILURE;
        }
    };

    let block = control.block();
    let current = block.is_compatible().then(|| block.read());
    match current {
        Some(snapshot) => println!("current  : {}", summarize(&snapshot)),
        None => println!("current  : the block exists but nobody has published to it"),
    }

    if !args.publish {
        return ExitCode::SUCCESS;
    }

    // Anything the user did not name keeps its current value, so changing only
    // the level does not silently reset the preset.
    let percent = args
        .percent
        .or_else(|| current.map(|snapshot| snapshot.manual_percent))
        .unwrap_or(DEFAULT_PERCENT);
    let preset = args
        .preset
        .or_else(|| current.map(|snapshot| snapshot.preset))
        .unwrap_or(Preset::Music);
    let strength = args
        .strength
        .or_else(|| current.map(|snapshot| snapshot.params.agc.strength));

    let snapshot = ControlSnapshot::from_ui(preset, percent, strength);
    publish(&control, &snapshot);
    println!("published: {}", summarize(&snapshot));
    ExitCode::SUCCESS
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args {
        percent: None,
        preset: None,
        strength: None,
        wait: Duration::from_secs(5),
        publish: false,
    };

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--off" => {
                parsed.preset = Some(Preset::Off);
                parsed.publish = true;
            }
            "--preset" => {
                let name = args.next().ok_or("--preset needs a name")?;
                parsed.preset = Some(parse_preset(&name)?);
                parsed.publish = true;
            }
            "--strength" => {
                let value = args.next().ok_or("--strength needs a number")?;
                let value: f32 = value
                    .parse()
                    .map_err(|_| format!("{value:?} is not a number"))?;
                if !(0.0..=1.0).contains(&value) {
                    return Err(format!("--strength {value} is outside 0..=1"));
                }
                parsed.strength = Some(value);
                parsed.publish = true;
            }
            "--wait" => {
                let value = args.next().ok_or("--wait needs a number of seconds")?;
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| format!("{value:?} is not a number of seconds"))?;
                parsed.wait = Duration::from_secs(seconds);
            }
            other => {
                let percent: u32 = other
                    .parse()
                    .map_err(|_| format!("unexpected argument {other:?}"))?;
                if !((MIN_PERCENT as u32)..=(MAX_PERCENT as u32)).contains(&percent) {
                    return Err(format!(
                        "{percent} % is outside {MIN_PERCENT}..={MAX_PERCENT} %"
                    ));
                }
                parsed.percent = Some(percent);
                parsed.publish = true;
            }
        }
    }

    Ok(Some(parsed))
}

fn parse_preset(name: &str) -> Result<Preset, String> {
    match name.to_ascii_lowercase().as_str() {
        "off" => Ok(Preset::Off),
        "speech" => Ok(Preset::Speech),
        "music" => Ok(Preset::Music),
        "night" => Ok(Preset::Night),
        other => Err(format!(
            "{other:?} is not a preset (off, speech, music, night)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(values: &[&str]) -> impl Iterator<Item = String> + use<> {
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn a_bare_level_publishes() {
        let args = parse(argv(&["350"])).unwrap().unwrap();
        assert_eq!(args.percent, Some(350));
        assert!(args.publish);
        assert_eq!(args.preset, None);
    }

    #[test]
    fn no_arguments_only_reports() {
        let args = parse(argv(&[])).unwrap().unwrap();
        assert!(!args.publish);
        assert_eq!(args.percent, None);
    }

    #[test]
    fn presets_are_case_insensitive() {
        let args = parse(argv(&["--preset", "Night"])).unwrap().unwrap();
        assert_eq!(args.preset, Some(Preset::Night));
        assert!(args.publish);
    }

    #[test]
    fn help_is_not_an_error() {
        assert!(parse(argv(&["--help"])).unwrap().is_none());
    }

    #[test]
    fn a_level_outside_the_slider_is_refused() {
        assert!(parse(argv(&["99"])).is_err());
        assert!(parse(argv(&["501"])).is_err());
    }

    #[test]
    fn unknown_options_are_refused() {
        assert!(parse(argv(&["--louder"])).is_err());
        assert!(parse(argv(&["--preset"])).is_err());
        assert!(parse(argv(&["--strength", "2"])).is_err());
    }
}
