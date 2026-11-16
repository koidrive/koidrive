// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;

/// What the telemetry of this process does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Nothing is recorded, written or sent.
    Off,
    On,
    /// Recorded as with `On`, but each upload is printed to stderr instead of sent.
    Debug,
}

/// What decided the [`Mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `DO_NOT_TRACK` is set.
    DoNotTrack,
    /// `KOIDRIVE_TELEMETRY` is set.
    Variable,
    /// `koi telemetry on` or `off`, or the answer to the question in `koi setup`.
    Stored,
    /// Nobody chose, so it is off.
    Unset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub mode: Mode,
    pub source: Source,
}

pub const VARIABLE: &str = "KOIDRIVE_TELEMETRY";

/// The mode, from the first of these that says something:
///
/// 1. `DO_NOT_TRACK` set to anything but an empty value or `0` turns it off.
/// 2. `KOIDRIVE_TELEMETRY`: `0`, `off` or `false` turns it off, `1`, `on`, `true` or `internal` on, `debug` to
///    printing. Any other value turns it off.
/// 3. The stored choice.
/// 4. Off.
pub fn resolve(var: impl Fn(&str) -> Option<OsString>, stored: Option<bool>) -> Status {
    if do_not_track(&var) {
        return Status {
            mode: Mode::Off,
            source: Source::DoNotTrack,
        };
    }
    if let Some(value) = var(VARIABLE) {
        let mode = match value.to_str().map(str::to_ascii_lowercase).as_deref() {
            Some("1" | "on" | "true" | "internal") => Mode::On,
            Some("debug") => Mode::Debug,
            _ => Mode::Off,
        };
        return Status {
            mode,
            source: Source::Variable,
        };
    }
    match stored {
        Some(on) => Status {
            mode: if on { Mode::On } else { Mode::Off },
            source: Source::Stored,
        },
        None => Status {
            mode: Mode::Off,
            source: Source::Unset,
        },
    }
}

/// Whether reports are marked internal: by the stored mark or by `KOIDRIVE_TELEMETRY=internal`.
pub fn internal(var: impl Fn(&str) -> Option<OsString>, stored: bool) -> bool {
    stored
        || var(VARIABLE).is_some_and(|value| {
            value
                .to_str()
                .is_some_and(|value| value.eq_ignore_ascii_case("internal"))
        })
}

/// Whether `koi setup` should ask: nobody chose yet, no variable decides, and the command does not run in CI.
pub fn should_ask(var: impl Fn(&str) -> Option<OsString>, stored: Option<bool>) -> bool {
    let ci = var("CI").is_some_and(|value| !value.is_empty() && value != "0" && value != "false");
    resolve(&var, stored).source == Source::Unset && !ci
}

fn do_not_track(var: &impl Fn(&str) -> Option<OsString>) -> bool {
    var("DO_NOT_TRACK").is_some_and(|value| !value.is_empty() && value != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.into())
    }

    #[test]
    fn do_not_track_wins_over_everything() {
        let status = resolve(vars(&[("DO_NOT_TRACK", "1"), (VARIABLE, "on")]), Some(true));
        assert_eq!(
            status,
            Status {
                mode: Mode::Off,
                source: Source::DoNotTrack
            }
        );
        assert_eq!(
            resolve(vars(&[("DO_NOT_TRACK", "0")]), Some(true)).mode,
            Mode::On
        );
        assert_eq!(
            resolve(vars(&[("DO_NOT_TRACK", "")]), Some(true)).mode,
            Mode::On
        );
    }

    #[test]
    fn the_variable_wins_over_the_stored_choice() {
        assert_eq!(
            resolve(vars(&[(VARIABLE, "0")]), Some(true)).mode,
            Mode::Off
        );
        assert_eq!(
            resolve(vars(&[(VARIABLE, "ON")]), Some(false)).mode,
            Mode::On
        );
        assert_eq!(
            resolve(vars(&[(VARIABLE, "debug")]), None).mode,
            Mode::Debug
        );
        assert_eq!(
            resolve(vars(&[(VARIABLE, "maybe")]), Some(true)),
            Status {
                mode: Mode::Off,
                source: Source::Variable
            }
        );
    }

    #[test]
    fn the_internal_variable_turns_reports_on_and_marks_them() {
        let var = vars(&[(VARIABLE, "Internal")]);
        assert_eq!(
            resolve(&var, Some(false)),
            Status {
                mode: Mode::On,
                source: Source::Variable
            }
        );
        assert!(internal(&var, false));
        assert!(internal(vars(&[]), true));
        assert!(!internal(vars(&[(VARIABLE, "on")]), false));
    }

    #[test]
    fn without_a_choice_it_is_off() {
        assert_eq!(
            resolve(vars(&[]), None),
            Status {
                mode: Mode::Off,
                source: Source::Unset
            }
        );
        assert_eq!(
            resolve(vars(&[]), Some(true)),
            Status {
                mode: Mode::On,
                source: Source::Stored
            }
        );
    }

    #[test]
    fn setup_asks_only_when_nothing_decided_and_not_in_ci() {
        assert!(should_ask(vars(&[]), None));
        assert!(!should_ask(vars(&[]), Some(false)));
        assert!(!should_ask(vars(&[("CI", "true")]), None));
        assert!(should_ask(vars(&[("CI", "false")]), None));
        assert!(!should_ask(vars(&[(VARIABLE, "1")]), None));
        assert!(!should_ask(vars(&[("DO_NOT_TRACK", "1")]), None));
    }
}
