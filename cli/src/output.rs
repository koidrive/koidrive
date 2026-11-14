// SPDX-License-Identifier: Apache-2.0

use std::io::{self, IsTerminal};
use std::path::Path;

use cliclack::Theme as _;

#[derive(Clone, Copy, Debug, Default)]
pub enum Presentation {
    #[default]
    Text,
    Value,
    Literal,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum Tone {
    #[default]
    Neutral,
    Completed,
    Attention,
}

pub enum Note<'a> {
    Information(&'a str),
    Error(&'a str),
    Usage(&'a str),
}

struct Theme;

impl cliclack::Theme for Theme {}

pub fn path(path: &Path) -> String {
    crate::printable(&crate::setup::shown_folder(path))
}

pub fn command_word(word: &str) -> String {
    crate::printable(&crate::pairing::shell_word(word))
}

pub fn command_path(path: &Path) -> String {
    let relative = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|home| home.is_absolute())
        .and_then(|home| path.strip_prefix(home).ok());
    match relative {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", command_word(&rest.to_string_lossy())),
        None => command_word(&path.to_string_lossy()),
    }
}

pub fn wait_cause(wait: &koidrive_driver::Wait, now_ms: i64) -> String {
    let cause = match wait.kind {
        koidrive_driver::WaitKind::Quota => "Storage is full or a daily limit was reached.",
        koidrive_driver::WaitKind::Retry => "Sync is waiting to retry an operation.",
    };
    let cause = match &wait.unreachable {
        Some(unreachable) => {
            let mut cause = format!(
                "Cannot reach the server: {}",
                crate::printable(&unreachable.why)
            );
            if !cause.ends_with(['.', '!', '?']) {
                cause.push('.');
            }
            cause
        }
        None if wait.kind == koidrive_driver::WaitKind::Retry && wait.until_ms.is_none() => {
            "A sync operation needs another try.".to_owned()
        }
        None => cause.to_owned(),
    };
    let Some(until_ms) = wait.until_ms else {
        return format!("{cause} Retrying now.");
    };
    let seconds = (until_ms.saturating_sub(now_ms).max(0) as u64).div_ceil(1000);
    let retry = match seconds {
        0 => "Retry is due now.".to_owned(),
        1..=60 => format!("Retry in {}.", crate::count(seconds, "second")),
        61..=3600 => format!(
            "Retry in about {}.",
            crate::count(seconds.div_ceil(60), "minute")
        ),
        _ => format!(
            "Retry in about {}.",
            crate::count(seconds.div_ceil(3600), "hour")
        ),
    };
    format!("{cause} {retry}")
}

pub fn concise(command: &str) -> bool {
    !matches!(command, "update" | "_daemon" | "_reconcile")
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
}

pub fn rich(command: &str) -> bool {
    !matches!(command, "_daemon" | "_reconcile")
        && crate::terminal::rich(
            crate::terminal::plain_requested(),
            console::Term::stderr()
                .size_checked()
                .map(|(_, width)| width),
            io::stdout().is_terminal() && io::stderr().is_terminal(),
        )
}

pub fn print(
    command: &str,
    presentation: Presentation,
    tone: Tone,
    text: &str,
    note: Note<'_>,
    pending: Option<&str>,
) {
    let (note, error, prefix) = match note {
        Note::Information(text) => (text, false, ""),
        Note::Error(text) => (text, true, "koi: "),
        Note::Usage(text) => (text, true, ""),
    };
    if !rich(command) {
        print!("{text}");
        if !note.is_empty() {
            eprint!("{prefix}{note}");
        }
        if let Some(pending) = pending {
            eprintln!("koi: {pending}");
        }
        return;
    }
    crate::terminal::colors(false);
    let theme = Theme;
    let result_symbol = match tone {
        Tone::Completed => theme.submit_symbol(),
        Tone::Neutral => theme.info_symbol(),
        Tone::Attention => theme.warning_symbol(),
    };
    match presentation {
        Presentation::Text if !text.is_empty() => {
            print!(
                "{}",
                theme.format_log_with_spacing(text.trim_end(), &result_symbol, false)
            );
        }
        _ => print!("{text}"),
    }
    if !note.is_empty() {
        let symbol = match (error, tone) {
            (true, _) => theme.error_symbol(),
            (false, Tone::Attention) => theme.warning_symbol(),
            (false, _) => theme.info_symbol(),
        };
        eprint!(
            "{}",
            theme.format_log_with_spacing(note.trim_end(), &symbol, false)
        );
    }
    if let Some(pending) = pending {
        eprint!(
            "{}",
            theme.format_log_with_spacing(pending, &theme.warning_symbol(), false)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_words_quote_shell_syntax_and_escape_terminal_controls() {
        for word in [
            "Notes",
            "Work Files",
            "Team's $(exit 99)",
            "Work\u{1b}[2J\u{202e}",
        ] {
            let shown = command_word(word);
            assert!(!shown.chars().any(char::is_control));
            assert!(!shown.contains('\u{202e}'));
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &format!("printf '%s' {shown}")])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, crate::printable(word).as_bytes());
        }
    }

    #[test]
    fn wait_causes_keep_the_cause_and_round_retry_times_up() {
        for (kind, cause) in [
            (
                koidrive_driver::WaitKind::Quota,
                "Storage is full or a daily limit was reached.",
            ),
            (
                koidrive_driver::WaitKind::Retry,
                "Sync is waiting to retry an operation.",
            ),
        ] {
            for (delay, retry) in [
                (-1, "Retry is due now."),
                (0, "Retry is due now."),
                (1, "Retry in 1 second."),
                (60_000, "Retry in 60 seconds."),
                (60_001, "Retry in about 2 minutes."),
                (3_600_000, "Retry in about 60 minutes."),
                (3_600_001, "Retry in about 2 hours."),
            ] {
                let wait = koidrive_driver::Wait {
                    kind,
                    until_ms: Some(1_000_000 + delay),
                    unreachable: None,
                };
                assert_eq!(wait_cause(&wait, 1_000_000), format!("{cause} {retry}"));
            }
        }
    }

    #[test]
    fn wait_causes_keep_offline_reasons_and_identify_an_active_retry() {
        let mut wait = koidrive_driver::Wait {
            kind: koidrive_driver::WaitKind::Retry,
            until_ms: None,
            unreachable: Some(koidrive_driver::Unreachable {
                why: "the server's address could not be found\u{1b}[2J".into(),
                no_internet: false,
            }),
        };
        assert_eq!(
            wait_cause(&wait, 1_000_000),
            "Cannot reach the server: the server's address could not be found\\u{1b}[2J. Retrying now."
        );
        wait.until_ms = Some(1_001_000);
        assert!(wait_cause(&wait, 1_000_000).ends_with("Retry in 1 second."));
        wait.until_ms = None;
        wait.unreachable = None;
        assert_eq!(
            wait_cause(&wait, 1_000_000),
            "A sync operation needs another try. Retrying now."
        );
    }

    #[test]
    fn concise_command_paths_expand_home_and_quote_only_the_relative_name() {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        let folder = home.join("Work Files");
        assert_eq!(command_path(&home), "~");
        assert_eq!(command_path(&folder), "~/'Work Files'");
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", command_path(&folder)))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, folder.to_string_lossy().as_bytes());
    }
}
