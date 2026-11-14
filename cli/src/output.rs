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

pub fn command_path(path: &Path) -> String {
    let relative = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|home| home.is_absolute())
        .and_then(|home| path.strip_prefix(home).ok());
    let word = match relative {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", crate::pairing::shell_word(&rest.to_string_lossy())),
        None => crate::pairing::shell_word(&path.to_string_lossy()),
    };
    crate::printable(&word)
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
        let symbol = match error {
            true => theme.error_symbol(),
            false => theme.info_symbol(),
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
