// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koidrive_driver::{Credential, Paths};
use serde_json::{Value, json};

use crate::error::CliError;
use crate::report::{Decision, DiagnosticsData, Done};
use crate::service;

const ISSUES: &str = "https://github.com/koidrive/koidrive/issues";

const LOG_LINES: usize = 200;

const STATUS_WAIT: Duration = Duration::from_secs(10);

const SIGNALS: [&str; 2] = ["\"SIGTERM\"", "\"SIGINT\""];

const NAMED: [(&str, &str); 6] = [
    ("path", "folder"),
    ("folder", "folder"),
    ("server", "server"),
    ("space_name", "space"),
    ("device_name", "device"),
    ("user_name", "user"),
];

const KEPT: [&str; 8] = [
    "error",
    "problem",
    "reason",
    "space_id",
    "question_id",
    "restore_id",
    "retry_in",
    "wait",
];

const LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];

const OURS: [&str; 4] = [
    "config.toml",
    "credential.json",
    "koidrive.service",
    ".koiignore",
];

pub async fn run(paths: &Paths) -> Result<Done, CliError> {
    let mut redactor = Redactor::default();
    know_profile(&mut redactor, paths);
    let status = match tokio::time::timeout(STATUS_WAIT, crate::status(paths, false)).await {
        Ok(Ok(done)) => {
            let mut status = done.data;
            if let Value::Object(fields) = &mut status
                && !done.decisions.is_empty()
            {
                fields.insert(
                    "decisions".to_owned(),
                    decisions(&mut redactor, &done.decisions),
                );
            }
            status
        }
        failed => {
            let error = match failed {
                Ok(Err(error)) => error.message,
                _ => format!(
                    "the daemon did not answer within {} seconds",
                    STATUS_WAIT.as_secs()
                ),
            };
            json!({
                "error": error,
                "service": service::inspect(paths, false).await,
            })
        }
    };
    let text = bundle(&mut redactor, &system(), &status, &daemon_log(paths));
    let seconds = (SystemTime::now().duration_since(UNIX_EPOCH)).map_or(0, |since| since.as_secs());
    let dir = std::env::current_dir()
        .map_err(|error| format!("cannot find the current directory: {error}"))?;
    let mut attempt = 1;
    let (path, mut file) = loop {
        let suffix = match attempt {
            1 => String::new(),
            _ => format!("-{attempt}"),
        };
        let path = dir.join(format!("koi-diagnostics-{seconds}{suffix}.txt"));
        let opened = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        match opened {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => attempt += 1,
            Err(error) => return Err(format!("cannot write {}: {error}", path.display()).into()),
        }
    };
    file.write_all(text.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(Done {
        presentation: crate::output::Presentation::Value,
        note: format!("To report a problem, attach this file to an issue at {ISSUES}\n"),
        ..Done::ok(
            DiagnosticsData { path: path.clone() },
            format!("{}\n", path.display()),
        )
    })
}

fn decisions(redactor: &mut Redactor, decisions: &[Decision]) -> Value {
    let mut out = Vec::new();
    for decision in decisions {
        let mut items = Vec::new();
        for item in &decision.items {
            items.push(json!({
                "node_id": item.node_id,
                "path": redactor.name("path", &item.path),
                "files": item.files,
            }));
        }
        let answers: Vec<&String> = decision
            .answers
            .iter()
            .map(|answer| &answer.token)
            .collect();
        out.push(json!({
            "id": decision.id,
            "space_id": decision.space_id,
            "kind": decision.kind,
            "group": decision.group,
            "asked_at_ms": decision.asked_at_ms,
            "requires_choice": decision.requires_choice,
            "counts": decision.counts,
            "items": items,
            "answers": answers,
        }));
    }
    Value::Array(out)
}

fn know_profile(redactor: &mut Redactor, paths: &Paths) {
    let home = std::env::var("HOME").unwrap_or_default();
    for path in [
        &paths.state_dir,
        &paths.runtime_dir,
        paths.config_file.parent().unwrap_or(&paths.state_dir),
    ] {
        if !path.starts_with(&home) || home.is_empty() {
            redactor.know("path", &path.to_string_lossy());
        }
    }
    redactor.know("home", &home);
    let user = Path::new(&home)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    for user in [std::env::var("USER").ok(), user].into_iter().flatten() {
        redactor.know("user", &user);
    }
    let host = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    redactor.know("host", &host);
    redactor.know("host", host.split('.').next().unwrap_or_default());
    if let Ok(Some(credential)) = Credential::load(&paths.credential()) {
        redactor.know("token", &credential.refresh_credential);
        redactor.know_server(&credential.server);
        redactor.know("device", &credential.device_name);
        redactor.know("user", credential.user_name.as_deref().unwrap_or_default());
    }
}

fn bundle(
    redactor: &mut Redactor,
    system: &str,
    status: &Value,
    log: &Result<String, String>,
) -> String {
    redactor.register(status);
    let status = redactor.json(status);
    let status = serde_json::to_string_pretty(&status).expect("JSON values serialize");
    let log = match log {
        Ok(log) => {
            let lines: Vec<&str> = log.lines().collect();
            let tail = &lines[lines.len().saturating_sub(LOG_LINES)..];
            let lines: Vec<String> = tail.iter().map(|line| redactor.log_line(line)).collect();
            lines.join("\n")
        }
        Err(problem) => format!("(no log: {})", redactor.text(problem)),
    };
    format!(
        "Koidrive diagnostics\n\nkoi {}\n{system}\n\n## koi status --json\n\n{status}\n\n## The daemon's log, last {LOG_LINES} lines\n\n{log}\n",
        env!("CARGO_PKG_VERSION"),
    )
}

fn system() -> String {
    let arch = std::env::consts::ARCH;
    if cfg!(target_os = "macos") {
        let version = Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        return format!("macOS {} ({arch})", version.unwrap_or_default());
    }
    let name = (fs::read_to_string("/etc/os-release").ok())
        .and_then(|text| {
            (text.lines())
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|name| name.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| std::env::consts::OS.to_owned());
    let kernel = rustix::system::uname()
        .release()
        .to_string_lossy()
        .into_owned();
    format!("{name} ({arch}), kernel {kernel}")
}

fn daemon_log(paths: &Paths) -> Result<String, String> {
    let service = service::this_service(paths)?;
    if cfg!(target_os = "macos") {
        let path = service.log_file();
        let log = fs::read(&path).map_err(|error| match error.kind() {
            ErrorKind::NotFound => "the service has not written a log".to_owned(),
            _ => format!("cannot read {}: {error}", path.display()),
        })?;
        let log = String::from_utf8_lossy(&log).into_owned();
        if log.lines().count() < LOG_LINES
            && let Ok(older) = fs::read(crate::logfile::previous(&path))
        {
            return Ok(String::from_utf8_lossy(&older).into_owned() + &log);
        }
        return Ok(log);
    }
    let lines = LOG_LINES.to_string();
    let unit = service.unit_name();
    let args = [
        "--user",
        "--unit",
        &unit,
        "--lines",
        &lines,
        "--output",
        "cat",
        "--no-pager",
        "--quiet",
    ];
    let output = Command::new("journalctl")
        .args(args)
        .output()
        .map_err(|error| format!("cannot run journalctl: {error}"))?;
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        false => Err(format!(
            "journalctl failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

#[derive(Default)]
struct Redactor {
    placeholders: BTreeMap<String, String>,
    counts: BTreeMap<&'static str, usize>,
}

impl Redactor {
    fn name(&mut self, kind: &'static str, value: &str) -> String {
        if let Some(placeholder) = self.placeholders.get(value) {
            return placeholder.clone();
        }
        let count = self.counts.entry(kind).or_default();
        *count += 1;
        let placeholder = format!("<{kind}-{count}>");
        self.placeholders
            .insert(value.to_owned(), placeholder.clone());
        placeholder
    }

    fn know(&mut self, kind: &'static str, value: &str) {
        let placeholder = self
            .placeholders
            .values()
            .any(|placeholder| placeholder == value);
        if !value.trim().is_empty() && !placeholder {
            self.name(kind, value);
        }
    }

    fn know_server(&mut self, server: &str) {
        self.know("server", server);
        if let Some(host) = url::Url::parse(server)
            .ok()
            .as_ref()
            .and_then(url::Url::host_str)
        {
            self.know("host", host);
        }
    }

    fn register(&mut self, value: &Value) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    let kind = NAMED
                        .iter()
                        .find(|(name, _)| name == key)
                        .map(|(_, kind)| *kind);
                    match (kind, value.as_str()) {
                        (Some("server"), Some(text)) => self.know_server(text),
                        (Some(kind), Some(text)) => self.know(kind, text),
                        _ => self.register(value),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| self.register(item)),
            _ => {}
        }
    }

    fn json(&mut self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => items.iter().map(|item| self.json(item)).collect(),
            Value::Object(fields) => (fields.iter())
                .map(|(key, value)| (key.clone(), self.json(value)))
                .collect(),
            other => other.clone(),
        }
    }

    fn log_line(&mut self, line: &str) -> String {
        let Some((time, level, target, rest)) = log_parts(line) else {
            return "<redacted>".to_owned();
        };
        let starts = field_starts(rest);
        let message = &rest[..starts.first().map_or(rest.len(), |start| start - 1)];
        let mut out = format!("{time} {level} {target}: {}", self.text(message));
        for (index, start) in starts.iter().enumerate() {
            let end = starts.get(index + 1).map_or(rest.len(), |next| next - 1);
            let (key, value) = rest[*start..end].split_once('=').unwrap_or_default();
            let value = match key {
                "signal" if SIGNALS.contains(&value) => value.to_owned(),
                _ if KEPT.contains(&key) => self.text(value),
                _ => "<redacted>".to_owned(),
            };
            out.push_str(&format!(" {key}={value}"));
        }
        out
    }

    fn text(&mut self, text: &str) -> String {
        let mut known: Vec<(String, String)> = (self.placeholders.iter())
            .map(|(value, placeholder)| (value.clone(), placeholder.clone()))
            .collect();
        known.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let mut out = String::new();
        let mut rest = text;
        'scan: while let Some(c) = rest.chars().next() {
            let starts_word = !out.chars().next_back().is_some_and(char::is_alphanumeric);
            for (value, placeholder) in &known {
                let ends_word =
                    |after: &str| !after.chars().next().is_some_and(char::is_alphanumeric);
                if starts_word
                    && rest.starts_with(value.as_str())
                    && ends_word(&rest[value.len()..])
                {
                    out.push_str(placeholder);
                    rest = &rest[value.len()..];
                    continue 'scan;
                }
            }
            if c == '"'
                && let Some(end) = closing_quote(rest)
            {
                out.push('"');
                out.push_str(&self.name("name", &rest[1..end]));
                out.push('"');
                rest = &rest[end + 1..];
                continue;
            }
            if delimiter(c) {
                out.push(c);
                rest = &rest[c.len_utf8()..];
                continue;
            }
            let word = &rest[..rest.find(delimiter).unwrap_or(rest.len())];
            let len = match word.contains('/') && !word.contains("://") {
                true => path_len(rest),
                false => word.trim_end_matches(['.', ':', '!', '?']).len(),
            };
            let core = &rest[..len.max(c.len_utf8())];
            match kind_of(core) {
                Some(kind) => out.push_str(&self.name(kind, core)),
                None => out.push_str(core),
            }
            rest = &rest[core.len()..];
        }
        out
    }
}

fn closing_quote(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().skip(1);
    while let Some((index, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return Some(index),
            _ => {}
        }
    }
    None
}

fn log_parts(line: &str) -> Option<(&str, &str, &str, &str)> {
    let (time, rest) = line.split_once(' ')?;
    let time_shape = |c: char| c.is_ascii_digit() || "-:.TZ+".contains(c);
    if time.is_empty() || !time.chars().all(time_shape) {
        return None;
    }
    let (level, rest) = rest.trim_start().split_once(' ')?;
    let (target, rest) = rest.trim_start().split_once(": ")?;
    let target_shape =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == ':';
    (LEVELS.contains(&level) && !target.is_empty() && target.chars().all(target_shape))
        .then_some((time, level, target, rest))
}

fn field_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut index = 0;
    while let Some(c) = text[index..].chars().next() {
        if c == '"'
            && let Some(end) = closing_quote(&text[index..])
        {
            index += end + 1;
            continue;
        }
        index += c.len_utf8();
        let key = &text[index..];
        let len = key
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(key.len());
        if c == ' '
            && len > 0
            && !key.starts_with(|c: char| c.is_ascii_digit())
            && key[len..].starts_with('=')
        {
            starts.push(index);
        }
    }
    starts
}

fn delimiter(c: char) -> bool {
    c.is_whitespace() || "\"'<>()[]{},;=".contains(c)
}

fn path_len(text: &str) -> usize {
    let line = &text[..text.find('\n').unwrap_or(text.len())];
    let end = os_error_start(line).unwrap_or(line.len());
    line[..end].trim_end_matches(['.', ':']).len()
}

fn os_error_start(line: &str) -> Option<usize> {
    let (head, code) = (line.trim_end().strip_suffix(')')?).rsplit_once(" (os error ")?;
    code.parse::<i32>().ok()?;
    head.rfind(": ")
}

fn kind_of(word: &str) -> Option<&'static str> {
    if word.contains("://") {
        return Some("url");
    }
    if word.contains('@') || ip_address(word) {
        return Some("address");
    }
    if word.contains('/') {
        return Some("path");
    }
    if word.starts_with("dev.koidrive.") || OURS.contains(&word) {
        return None;
    }
    if let Some((head, tail)) = word.rsplit_once('.')
        && !head.is_empty()
        && head.chars().any(char::is_alphabetic)
        && (1..=12).contains(&tail.len())
        && tail.starts_with(|c: char| c.is_ascii_alphabetic())
        && tail.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Some("name");
    }
    let token = |c: char| c.is_ascii_alphanumeric() || "_-+/=.".contains(c);
    (word.len() >= 24
        && word.chars().all(token)
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic()))
    .then_some("token")
}

fn ip_address(word: &str) -> bool {
    let host = (word.rsplit_once(':'))
        .filter(|(_, port)| port.parse::<u16>().is_ok())
        .map_or(word, |(host, _)| host);
    host.parse::<std::net::IpAddr>().is_ok() || word.parse::<std::net::IpAddr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLANTED: [&str; 9] = [
        "/Users/ada",
        "Secret Plans",
        "budget 2026",
        "Tax Returns",
        "Ada's MacBook",
        "koi-ada-7f3k",
        "workers.dev",
        "kdr_9f8e7d6c5b4a39281706f5e4d3c2b1a0",
        "ledger",
    ];

    fn status() -> Value {
        json!({
            "account": {
                "server": "https://koi-ada-7f3k.workers.dev",
                "user_id": "u1",
                "user_name": "ada",
                "device_id": "d1",
                "device_name": "Ada's MacBook",
            },
            "running": true,
            "folders": [{
                "path": "/Users/ada/Secret Plans",
                "space_id": "s1",
                "space_name": "Tax Returns",
                "state": "halted",
                "problem": "cannot read /Users/ada/Secret Plans/budget 2026.xlsx: Permission denied (os error 13)",
            }],
            "conditions": [{"condition": "halted", "folder": "/Users/ada/Secret Plans", "problem": null}],
        })
    }

    const LOG: &str = "\
2026-11-12T06:29:22.152628Z  INFO koidrive_cli: the daemon is running
2026-11-12T06:29:23.000001Z  INFO koidrive_driver::folder: linked a folder space_id=s1 path=/Users/ada/Secret Plans
2026-11-12T06:29:24.000001Z  WARN koidrive_driver::folder: a local trash entry written after it entered was recovered space_id=s1 name=ledger
2026-11-12T06:29:25.000001Z DEBUG koidrive_net::session: koidrive session stopped reason=the server https://koi-ada-7f3k.workers.dev/v1/sync refused the token kdr_9f8e7d6c5b4a39281706f5e4d3c2b1a0
2026-11-12T06:29:26.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=no space is called \"Tax Returns\"
thread 'tokio-runtime-worker' panicked at /Users/ada/Secret Plans/ledger.rs:1:1:
";

    fn redacted() -> String {
        let mut redactor = Redactor::default();
        redactor.know("home", "/Users/ada");
        bundle(
            &mut redactor,
            "macOS 26.1 (aarch64)",
            &status(),
            &Ok(LOG.to_owned()),
        )
    }

    #[test]
    fn no_planted_path_name_url_or_token_reaches_the_file() {
        let text = redacted();
        for planted in PLANTED {
            assert!(!text.contains(planted), "{planted} in:\n{text}");
        }
    }

    #[test]
    fn the_file_keeps_what_explains_the_problem() {
        let text = redacted();
        for kept in [
            "the daemon is running",
            "linked a folder space_id=s1 path=<redacted>",
            "Permission denied (os error 13)",
            "\"state\": \"halted\"",
            "sync of a folder halted",
            "macOS 26.1 (aarch64)",
        ] {
            assert!(text.contains(kept), "{kept} not in:\n{text}");
        }
    }

    #[test]
    fn a_value_gets_the_same_placeholder_everywhere() {
        let text = redacted();
        assert!(text.contains("\"path\": \"<folder-1>\""), "{text}");
        assert!(text.contains("\"folder\": \"<folder-1>\""), "{text}");
        assert!(text.contains("\"space_name\": \"<space-1>\""), "{text}");
        assert!(text.contains("no space is called \"<space-1>\""), "{text}");
        assert!(
            text.contains("cannot read <folder-1><path-1>: Permission denied (os error 13)"),
            "{text}"
        );
    }

    #[test]
    fn a_word_that_holds_a_known_value_keeps_its_other_letters() {
        let mut redactor = Redactor::default();
        redactor.know("user", "ada");
        assert_eq!(redactor.text("ada and adam"), "<user-1> and adam");
    }

    #[test]
    fn a_path_is_redacted_to_the_error_after_it_whatever_its_name_holds() {
        let mut redactor = Redactor::default();
        redactor.know("home", "/Users/ada");
        let line = |reason: &str| {
            format!(
                "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {reason}"
            )
        };
        for (reason, error, leaks) in [
            (
                "/Volumes/외장 SSD/가족 사진/여권 사본 (김건우 900101).jpg: Permission denied (os error 13)",
                ": Permission denied (os error 13)",
                ["김건우", "900101", "jpg"],
            ),
            (
                "/Users/ada/Library/Mobile Documents/com~apple~CloudDocs/병원/Kim, Gunwoo - HIV 검사 결과.pdf: Operation not permitted (os error 1)",
                ": Operation not permitted (os error 1)",
                ["Gunwoo", "HIV", "검사"],
            ),
            (
                "/Users/ada/Documents/O'Brien Family/taxes: Permission denied (os error 13)",
                ": Permission denied (os error 13)",
                ["Brien", "Family", "taxes"],
            ),
            (
                "/home/ada/Sync/Re: 김건우 연봉계약서.pdf: Permission denied (os error 13)",
                ": Permission denied (os error 13)",
                ["김건우", "연봉계약서", "pdf"],
            ),
        ] {
            let out = redactor.log_line(&line(reason));
            assert!(out.contains("reason=the folder: <"), "{out}");
            assert!(out.ends_with(error), "{out}");
            for leak in leaks {
                assert!(!out.contains(leak), "{leak} in {out}");
            }
        }
    }

    #[test]
    fn no_word_of_a_random_path_survives_in_the_lines_the_daemon_writes() {
        const WORDS: [&str; 8] = [
            "김건우",
            "연봉계약서",
            "여권",
            "Jiyeon",
            "Passport",
            "OBrien",
            "Interview",
            "x7",
        ];
        const GLUE: [&str; 10] = [" ", ": ", ", ", " (", ")", "'", "\"", ".", "-", ""];
        const FIELD_GLUE: [&str; 2] = [" n=", " v2="];
        const EXTENSIONS: [&str; 4] = ["", ".pdf", ".mp4", ".tar.gz"];
        let relative_shapes = [
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {}: Permission denied (os error 13)",
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {} is missing",
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {} is not a folder",
        ];
        let absolute_shapes = [
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: the folder cannot be observed space_id=s1 error=cannot read {}: No such file or directory (os error 2)",
            "2026-11-12T06:29:23.000001Z  INFO koidrive_driver::driver: linked a folder space_id=s1 path={}",
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: the folder fails its root check space_id=s1 problem={} is not a folder",
        ];
        let mut rng = fastrand::Rng::with_seed(262);
        for _ in 0..2000 {
            let relative = rng.bool();
            let mut path = String::new();
            for _ in 0..rng.usize(1..5) {
                path.push('/');
                for _ in 0..rng.usize(1..5) {
                    path.push_str(WORDS[rng.usize(..WORDS.len())]);
                    path.push_str(match relative && rng.u8(..4) == 0 {
                        true => FIELD_GLUE[rng.usize(..FIELD_GLUE.len())],
                        false => GLUE[rng.usize(..GLUE.len())],
                    });
                }
                path.push_str(WORDS[rng.usize(..WORDS.len())]);
            }
            path.push_str(EXTENSIONS[rng.usize(..EXTENSIONS.len())]);
            if relative {
                path = format!("{:?}", Path::new(&path[1..]));
            }
            let shape = match relative || rng.bool() {
                true => relative_shapes[rng.usize(..relative_shapes.len())],
                false => absolute_shapes[rng.usize(..absolute_shapes.len())],
            };
            let mut redactor = Redactor::default();
            let out = redactor.log_line(&shape.replace("{}", &path));
            for word in WORDS {
                assert!(!out.contains(word), "{word} in {out}\nfrom {path}");
            }
            let problem = match relative {
                true => format!("the folder: {path} is not a folder"),
                false => format!("cannot read {path}: Permission denied (os error 13)"),
            };
            let problem = redactor.json(&json!({ "problem": problem }));
            assert!(
                WORDS.iter().all(|word| !problem.to_string().contains(word)),
                "{problem}"
            );
        }
    }

    #[test]
    fn a_quoted_path_relative_to_the_folder_is_redacted_whole() {
        let mut redactor = Redactor::default();
        let quoted = |path: &str| format!("{:?}", Path::new(path));
        assert_eq!(
            redactor.text(&format!(
                "the folder: {} is missing",
                quoted("Secret Plans/budget 2026.xlsx")
            )),
            "the folder: \"<name-1>\" is missing"
        );
        assert_eq!(
            redactor.text(&format!(
                "the folder: {}: Permission denied (os error 13)",
                quoted("Jiyeon Passport/scan")
            )),
            "the folder: \"<name-2>\": Permission denied (os error 13)"
        );
        let out = redactor.text(&format!(
            "the folder: {} is not a folder",
            quoted("Kim \"Private\" Ledger")
        ));
        assert_eq!(out, "the folder: \"<name-3>\" is not a folder");
    }

    #[test]
    fn stuck_items_the_driver_names_are_redacted_whole() {
        let quoted = |path: &str| format!("{:?}", Path::new(path));
        let problem = format!(
            "2 items could not be synced: {} (the server did not take it), {} (its name here is taken by {}, which this device's rules exclude; it is kept in {}); run koi folder restart once they are fixed",
            quoted("김건우 여권.jpg"),
            quoted("가족 사진/Q3 연봉 계약서.pdf"),
            quoted("가족 사진/q3 연봉 계약서.pdf"),
            quoted(".koidrive/parked/3 Q3 연봉 계약서.pdf"),
        );
        let status = json!({"running": true, "folders": [{"path": "/Users/ada/Sync", "space_id": "s1", "space_name": "Family", "state": "catching_up", "problem": problem}]});
        let text = bundle(
            &mut Redactor::default(),
            "macOS",
            &status,
            &Ok(String::new()),
        );
        for word in ["김건우", "여권", "가족", "연봉", "Q3", "q3"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
        assert!(
            text.contains(
                r#"2 items could not be synced: \"<name-1>\" (the server did not take it)"#
            ),
            "{text}"
        );
        assert!(
            text.contains("run koi folder restart once they are fixed"),
            "{text}"
        );
        let problem = format!(
            "1 item could not be synced: {} (the server did not take it); run koi folder restart once it is fixed",
            quoted("Secret Plans/budget 2026.xlsx")
        );
        let out = Redactor::default().json(&json!({ "problem": problem }));
        assert_eq!(
            out,
            json!({ "problem": "1 item could not be synced: \"<name-1>\" (the server did not take it); run koi folder restart once it is fixed" })
        );
    }

    #[test]
    fn a_field_inside_a_quoted_name_does_not_split_the_line() {
        let mut redactor = Redactor::default();
        let line = |name: &str| {
            format!(
                "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {:?} is missing",
                Path::new(name)
            )
        };
        assert_eq!(
            redactor.log_line(&line("Kim Gunwoo survey n=30/results.xlsx")),
            "2026-11-12T06:29:23.000001Z WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: \"<name-1>\" is missing"
        );
        assert_eq!(
            redactor.log_line(&line("Jiyeon contract v2=final path=draft")),
            "2026-11-12T06:29:23.000001Z WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: \"<name-2>\" is missing"
        );
    }

    #[test]
    fn no_escaped_form_of_a_decisions_names_reaches_the_file() {
        use crate::report::{AnswerChoice, Item};
        let names = [
            "O'Brien Family/taxes 2026.xlsx",
            "Kim \"Private\" Ledger.docx",
            "back\\slash Notes",
            "line\nbreak Scan.pdf",
            "김건우 연봉 계약서.docx",
        ];
        let words = [
            "Brien",
            "taxes",
            "Kim",
            "Private",
            "Ledger",
            "slash",
            "Notes",
            "break",
            "Scan",
            "김건우",
            "연봉",
            "계약서",
        ];
        let decision = Decision {
            id: "q1".to_owned(),
            space_id: "s1".to_owned(),
            kind: crate::report::DecisionKind::Conflict,
            group: None,
            recovery_cause: None,
            conflict: None,
            asked_at_ms: 1,
            requires_choice: true,
            counts: None,
            items: (names.iter().enumerate())
                .map(|(index, name)| Item {
                    node_id: format!("n{index}"),
                    path: (*name).to_owned(),
                    files: 1,
                })
                .collect(),
            answers: (names.iter().enumerate())
                .map(|(index, name)| AnswerChoice {
                    token: format!("keep-node:n{index}"),
                    consequence: format!(
                        "Keep {} or {name:?} or {} and move {} to the trash",
                        crate::pairing::shell_word(name),
                        json!(name),
                        crate::printable(name),
                    ),
                    argv: vec!["koi".to_owned(), "resolve".to_owned(), "q1".to_owned()],
                })
                .collect(),
        };
        let mut redactor = Redactor::default();
        let status = json!({ "running": true, "decisions": decisions(&mut redactor, &[decision]) });
        let text = bundle(&mut redactor, "macOS", &status, &Ok(String::new()));
        for word in words {
            assert!(!text.contains(word), "{word} in {text}");
        }
        assert!(text.contains("\"keep-node:n0\""), "{text}");
        assert!(text.contains("\"path\": \"<path-1>\""), "{text}");
    }

    #[test]
    fn a_signal_field_keeps_only_a_signal_name() {
        let mut redactor = Redactor::default();
        let line = "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: /x/report signal=HIV-result.txt: Permission denied";
        let out = redactor.log_line(line);
        assert!(!out.contains("HIV") && !out.contains("result"), "{out}");
        assert!(out.ends_with(" signal=<redacted>"), "{out}");
    }

    #[test]
    fn a_line_that_does_not_start_with_a_time_is_left_out() {
        let mut redactor = Redactor::default();
        let line = "/Users/ada/Secret INFO koidrive_cli: the daemon is running";
        assert_eq!(redactor.log_line(line), "<redacted>");
    }

    #[test]
    fn a_signal_keeps_its_name() {
        let mut redactor = Redactor::default();
        let line = "2026-11-12T06:29:23.000001Z  INFO koidrive_cli: stopping the daemon signal=\"SIGTERM\"";
        assert_eq!(redactor.log_line(line), line.replace("  INFO", " INFO"));
    }

    #[test]
    fn a_path_with_a_colon_in_a_name_runs_to_the_last_colon_before_the_error() {
        let mut redactor = Redactor::default();
        let out =
            redactor.text("cannot read /x/Meeting: Jiyeon/notes: Permission denied (os error 13)");
        assert_eq!(out, "cannot read <path-1>: Permission denied (os error 13)");
    }

    #[test]
    fn a_bare_file_name_and_an_ip_address_are_redacted() {
        let mut redactor = Redactor::default();
        let out =
            redactor.text("cannot upload 여권스캔.mp4 to 192.168.0.12:8787 or [fe80::1]:8787");
        for leak in ["여권스캔", "mp4", "192.168", "fe80"] {
            assert!(!out.contains(leak), "{leak} in {out}");
        }
        assert_eq!(
            redactor.text("retry in 1.5s, version 0.1.0"),
            "retry in 1.5s, version 0.1.0"
        );
    }

    #[test]
    fn koidrives_own_names_stay() {
        let mut redactor = Redactor::default();
        let text = "running (dev.koidrive.koi.1a2b3c4d) and koidrive.service, see config.toml, version 0.1.0";
        assert_eq!(redactor.text(text), text);
    }
}
