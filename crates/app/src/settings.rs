//! What the user has chosen, independent of any window or registry.
//!
//! The tray app keeps one [`Settings`] value, turns it into a
//! [`ControlSnapshot`] to publish, and persists it as four plain numbers. The
//! interesting part is that "off" is a *state*, not a preset: turning the
//! booster off publishes [`Preset::Off`], but the preset the user had picked is
//! kept so turning it back on restores it.

use sb_core::params::{percent_to_gain_db, Preset, MAX_PERCENT, MIN_PERCENT};
use sb_ipc::ControlSnapshot;

/// Where the slider starts on a fresh install.
pub const DEFAULT_PERCENT: u32 = 250;

/// Full leveling strength, in thousandths.
pub const FULL_STRENGTH: u32 = 1000;

/// The presets a user can pick. `Off` is not one of them; it is the
/// [`Settings::enabled`] switch.
pub const PRESETS: [Preset; 3] = [Preset::Speech, Preset::Music, Preset::Night];

/// The lowest and highest slider positions, as integers.
pub const MIN: u32 = MIN_PERCENT as u32;
pub const MAX: u32 = MAX_PERCENT as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// False publishes [`Preset::Off`]: audio passes through untouched.
    pub enabled: bool,
    /// Never [`Preset::Off`]; see the module comment.
    pub preset: Preset,
    /// Slider position, `MIN..=MAX`.
    pub percent: u32,
    /// Leveling strength in thousandths, `0..=FULL_STRENGTH`. Integers, so a
    /// value read back from the control block compares equal to what was
    /// written.
    pub strength_milli: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            preset: Preset::Music,
            percent: DEFAULT_PERCENT,
            strength_milli: FULL_STRENGTH,
        }
    }
}

impl Settings {
    /// Bring any combination of values into range. Everything that builds a
    /// `Settings` from outside data (the registry, the control block) goes
    /// through here.
    pub fn sanitized(self) -> Self {
        let (enabled, preset) = match self.preset {
            Preset::Off => (false, Preset::Music),
            preset => (self.enabled, preset),
        };
        Self {
            enabled,
            preset,
            percent: self.percent.clamp(MIN, MAX),
            strength_milli: self.strength_milli.min(FULL_STRENGTH),
        }
    }

    /// What the APO should be told.
    pub fn snapshot(&self) -> ControlSnapshot {
        let preset = if self.enabled {
            self.preset
        } else {
            Preset::Off
        };
        ControlSnapshot::from_ui(
            preset,
            self.percent,
            Some(self.strength_milli as f32 / FULL_STRENGTH as f32),
        )
    }

    /// The settings a control block describes, keeping this value's preset if
    /// the block says "off" (the block cannot remember what was on before).
    ///
    /// This is how the tray app follows a change made by someone else, such as
    /// `sb-gain 400` from a terminal while the tray is running.
    pub fn adopt(&self, snapshot: &ControlSnapshot) -> Self {
        let preset = match snapshot.preset {
            Preset::Off => self.preset,
            preset => preset,
        };
        Self {
            enabled: snapshot.enabled && snapshot.preset != Preset::Off,
            preset,
            percent: snapshot.manual_percent,
            strength_milli: (snapshot.params.agc.strength.clamp(0.0, 1.0) * FULL_STRENGTH as f32)
                .round() as u32,
        }
        .sanitized()
    }

    pub fn gain_db(&self) -> f32 {
        percent_to_gain_db(self.percent as f32)
    }

    /// `"250 %  (+7.96 dB)"`, for the label beside the slider.
    pub fn level_text(&self) -> String {
        format!("{} %  (+{:.2} dB)", self.percent, self.gain_db())
    }

    /// `"80 %"`, for the label beside the strength slider.
    pub fn strength_text(&self) -> String {
        format!("{} %", (self.strength_milli + 5) / 10)
    }

    /// One line for the tray tooltip.
    pub fn describe(&self) -> String {
        if self.enabled {
            format!("{} % \u{00b7} {}", self.percent, self.preset.label())
        } else {
            "off".to_string()
        }
    }

    /// Index of [`Self::preset`] in [`PRESETS`].
    pub fn preset_index(&self) -> usize {
        PRESETS
            .iter()
            .position(|preset| *preset == self.preset)
            .unwrap_or(1)
    }

    /// The value names and numbers this is stored as.
    pub fn to_values(&self) -> [(&'static str, u32); 4] {
        [
            ("Enabled", u32::from(self.enabled)),
            ("Preset", self.preset.as_u32()),
            ("Percent", self.percent),
            ("StrengthMilli", self.strength_milli),
        ]
    }

    /// The inverse of [`Self::to_values`]. Missing values keep their default,
    /// so a store written by an older build still loads.
    pub fn from_values(lookup: impl Fn(&str) -> Option<u32>) -> Self {
        let defaults = Self::default();
        Self {
            enabled: lookup("Enabled").map_or(defaults.enabled, |value| value != 0),
            preset: lookup("Preset").map_or(defaults.preset, Preset::from_u32),
            percent: lookup("Percent").unwrap_or(defaults.percent),
            strength_milli: lookup("StrengthMilli").unwrap_or(defaults.strength_milli),
        }
        .sanitized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn off_is_a_switch_not_a_preset() {
        let settings = Settings {
            enabled: false,
            preset: Preset::Night,
            ..Settings::default()
        };
        let snapshot = settings.snapshot();
        assert!(!snapshot.enabled);
        assert_eq!(snapshot.preset, Preset::Off);

        // Reading that back keeps the preset that was chosen before.
        let back = settings.adopt(&snapshot);
        assert!(!back.enabled);
        assert_eq!(back.preset, Preset::Night);
    }

    #[test]
    fn an_off_preset_from_outside_becomes_disabled() {
        let odd = Settings {
            enabled: true,
            preset: Preset::Off,
            ..Settings::default()
        }
        .sanitized();
        assert!(!odd.enabled);
        assert_ne!(odd.preset, Preset::Off);
    }

    #[test]
    fn what_is_published_reads_back_unchanged() {
        for preset in PRESETS {
            for percent in [MIN, 137, 250, 333, MAX] {
                for strength_milli in [0, 1, 500, 999, FULL_STRENGTH] {
                    for enabled in [true, false] {
                        let settings = Settings {
                            enabled,
                            preset,
                            percent,
                            strength_milli,
                        };
                        let block = sb_ipc::ControlBlock::new();
                        block.write(&settings.snapshot());
                        assert_eq!(settings.adopt(&block.read()), settings);
                    }
                }
            }
        }
    }

    #[test]
    fn someone_elses_change_is_followed() {
        let mine = Settings::default();
        let theirs = ControlSnapshot::from_ui(Preset::Speech, 400, Some(0.5));
        let followed = mine.adopt(&theirs);
        assert_eq!(followed.percent, 400);
        assert_eq!(followed.preset, Preset::Speech);
        assert_eq!(followed.strength_milli, 500);
        assert!(followed.enabled);
    }

    #[test]
    fn values_round_trip_and_tolerate_garbage() {
        let settings = Settings {
            enabled: false,
            preset: Preset::Speech,
            percent: 420,
            strength_milli: 650,
        };
        let stored: HashMap<_, _> = settings.to_values().into_iter().collect();
        assert_eq!(
            Settings::from_values(|name| stored.get(name).copied()),
            settings
        );

        assert_eq!(Settings::from_values(|_| None), Settings::default());

        let garbage = Settings::from_values(|name| match name {
            "Percent" => Some(9_000),
            "StrengthMilli" => Some(u32::MAX),
            "Preset" => Some(0),
            _ => None,
        });
        assert_eq!(garbage.percent, MAX);
        assert_eq!(garbage.strength_milli, FULL_STRENGTH);
        assert!(!garbage.enabled, "a stored Off preset means disabled");
    }

    #[test]
    fn labels_read_the_way_the_slider_does() {
        let settings = Settings {
            percent: 500,
            strength_milli: 795,
            ..Settings::default()
        };
        assert_eq!(settings.level_text(), "500 %  (+13.98 dB)");
        assert_eq!(settings.strength_text(), "80 %");
        assert_eq!(settings.describe(), "500 % \u{00b7} Music");
        assert_eq!(
            Settings {
                enabled: false,
                ..settings
            }
            .describe(),
            "off"
        );
    }

    #[test]
    fn every_preset_has_a_combo_position() {
        for (index, preset) in PRESETS.into_iter().enumerate() {
            let settings = Settings {
                preset,
                ..Settings::default()
            };
            assert_eq!(settings.preset_index(), index);
        }
    }
}
