// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};

const CRATES_THE_DAEMON_AND_THE_APP_RUN: [&str; 10] = [
    "chunk",
    "driver",
    "engine",
    "ffi",
    "fs",
    "ignore",
    "net",
    "proto",
    "store",
    "telemetry",
];

const WORDS_THAT_NAME_A_PATH: [&str; 8] = [
    "path", "name", "root", "parked", "folder", "file", "dir", "other",
];

const ARGUMENTS_THAT_NAME_NO_PATH: [(&str, &str); 14] = [
    ("chunk/src/lib.rs", "size(self.small_file_threshold)"),
    ("driver/src/config.rs", "path"),
    ("driver/src/config.rs", "escape(name)"),
    ("driver/src/credential.rs", "name"),
    ("driver/src/driver.rs", "folder.link.device_id"),
    ("driver/src/driver.rs", "folder.link.server_url"),
    ("driver/src/folder.rs", "code.as_str_name()"),
    ("driver/src/ignore.rs", "dir"),
    ("driver/src/ignore.rs", "path.trim_matches('/')"),
    ("engine/src/question.rs", "names.join(\", \")"),
    ("ffi/src/fake/scenario.rs", "name"),
    ("net/src/api.rs", "base.path()"),
    ("net/src/error.rs", "self.code().as_str_name()"),
    ("telemetry/src/send.rs", "base.path()"),
];

const MACROS: [&str; 10] = [
    "format!(",
    "write!(",
    "writeln!(",
    "panic!(",
    "info!(",
    "warn!(",
    "error!(",
    "debug!(",
    "trace!(",
    "#[error(",
];

#[test]
fn core_text_quotes_every_path_and_name() {
    let core = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut found = Vec::new();
    for name in CRATES_THE_DAEMON_AND_THE_APP_RUN {
        let mut files = Vec::new();
        sources(&core.join(name).join("src"), &mut files);
        for file in files {
            let shown = file.strip_prefix(core).unwrap().display().to_string();
            check(&shown, &fs::read_to_string(&file).unwrap(), &mut found);
        }
    }
    assert!(
        found.is_empty(),
        "text that shows a path or a name unquoted; pass it through koidrive_fs::quoted:\n{}",
        found.join("\n")
    );
}

#[test]
fn an_item_path_is_plain_text_on_the_wire_and_quoted_in_text() {
    let path = koidrive_fs::ItemPath::from("Q3 \"draft\"/notes.txt");
    assert_eq!(
        serde_json::to_string(&path).unwrap(),
        r#""Q3 \"draft\"/notes.txt""#
    );
    assert_eq!(
        format!("{}", koidrive_fs::quoted(&path)),
        r#""Q3 \"draft\"/notes.txt""#
    );
    let mut found = Vec::new();
    check(
        "ffi/src/state.rs",
        r#"let id = format!("disk-{space}-{}", disk.path.unquoted());"#,
        &mut found,
    );
    assert_eq!(found.len(), 1, "{found:?}");
}

fn sources(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.file_name().unwrap().to_str().unwrap() {
            "tests" | "tests.rs" => {}
            _ if path.is_dir() => sources(&path, files),
            _ if path.extension().is_some_and(|ext| ext == "rs") => files.push(path),
            _ => {}
        }
    }
}

fn check(file: &str, source: &str, found: &mut Vec<String>) {
    let lines: Vec<&str> = source.lines().collect();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at].trim();
        if line == "#[cfg(test)]"
            && lines
                .get(at + 1)
                .is_some_and(|next| next.trim_start().starts_with("mod tests {"))
        {
            return;
        }
        let mut report = |why: &str| found.push(format!("{file}:{}: {why}: {line}", at + 1));
        if line.contains(".display()") {
            report("shows a path with .display()");
        }
        for start in MACROS
            .iter()
            .filter_map(|name| line.find(name).map(|at| at + name.len()))
        {
            let text = std::iter::once(&line[start..])
                .chain(lines[at + 1..].iter().take(40).copied())
                .collect::<Vec<_>>()
                .join("\n");
            for shown in unquoted_arguments(&arguments(&text)) {
                if !ARGUMENTS_THAT_NAME_NO_PATH.contains(&(file, shown.as_str())) {
                    report(&format!("shows {shown} unquoted"));
                }
            }
        }
        if line.starts_with("#[error(") {
            let mut attribute = String::new();
            let mut end = at;
            while end < lines.len() {
                attribute.push_str(lines[end].trim());
                end += 1;
                if attribute.ends_with(")]") {
                    break;
                }
            }
            let mut variant = String::new();
            while end < lines.len() {
                let next = lines[end].trim();
                if next.starts_with("#[") || next.starts_with("///") || next == "}" {
                    break;
                }
                variant.push_str(next);
                end += 1;
            }
            for field in path_fields(&variant) {
                if placeholders(&attribute)
                    .iter()
                    .any(|(arg, spec)| *arg == field && !spec.contains('?'))
                {
                    report(&format!("shows the path field {field} unquoted"));
                }
            }
        }
        at += 1;
    }
}

fn arguments(text: &str) -> Vec<String> {
    let mut items = vec![String::new()];
    let mut depth = 0;
    let mut literal = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let item = items.last_mut().unwrap();
        if literal {
            item.push(c);
            match c {
                '\\' => item.extend(chars.next()),
                '"' => literal = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => literal = true,
            '\'' => {
                let ahead: Vec<char> = chars.clone().take(3).collect();
                let len = match ahead.as_slice() {
                    ['\\', _, '\'', ..] => 3,
                    [_, '\'', ..] => 2,
                    _ => 0,
                };
                item.push(c);
                item.extend(chars.by_ref().take(len));
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                items.push(String::new());
                continue;
            }
            _ => {}
        }
        item.push(c);
    }
    items
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect()
}

fn unquoted_arguments(arguments: &[String]) -> Vec<String> {
    let Some(literal) = arguments
        .iter()
        .position(|argument| argument.starts_with('"'))
    else {
        return Vec::new();
    };
    let named = |argument: &str| {
        let len = argument
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(argument.len());
        let rest = argument[len..].trim_start();
        len > 0 && rest.starts_with('=') && !rest.starts_with("==")
    };
    let after = &arguments[literal + 1..];
    let positional: Vec<&String> = after.iter().filter(|argument| !named(argument)).collect();
    let mut next = 0;
    let mut out = Vec::new();
    for (arg, spec) in placeholders(&arguments[literal]) {
        let expression = match arg.parse::<usize>() {
            _ if arg.is_empty() => {
                next += 1;
                positional
                    .get(next - 1)
                    .map(|argument| argument.to_string())
            }
            Ok(index) => positional.get(index).map(|argument| argument.to_string()),
            Err(_) => Some(
                after
                    .iter()
                    .find_map(|argument| {
                        let (name, value) = argument.split_once('=')?;
                        (name.trim() == arg).then(|| value.trim().to_owned())
                    })
                    .unwrap_or(arg),
            ),
        };
        let Some(expression) = expression else {
            continue;
        };
        let lower = expression.to_lowercase();
        if !spec.contains('?')
            && !lower.replace("unquoted", "").contains("quoted")
            && WORDS_THAT_NAME_A_PATH
                .iter()
                .any(|word| lower.contains(word))
        {
            out.push(expression);
        }
    }
    out
}

fn placeholders(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut literal = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if literal => {
                chars.next();
            }
            '"' => literal = !literal,
            '{' if literal && chars.peek() == Some(&'{') => {
                chars.next();
            }
            '{' if literal => {
                let inner: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let (arg, spec) = inner.split_once(':').unwrap_or((&inner, ""));
                out.push((arg.to_owned(), spec.to_owned()));
            }
            _ => {}
        }
    }
    out
}

fn path_fields(variant: &str) -> Vec<String> {
    let Some(open) = variant.find(['(', '{']) else {
        return Vec::new();
    };
    let close = variant.rfind([')', '}']).unwrap_or(variant.len());
    let fields = variant[open + 1..close.max(open + 1)]
        .split(',')
        .map(str::trim);
    let tuple = variant[open..].starts_with('(');
    (fields.enumerate())
        .filter(|(_, field)| field.contains("Path"))
        .map(|(index, field)| match tuple {
            true => index.to_string(),
            false => field.split(':').next().unwrap().trim().to_owned(),
        })
        .collect()
}
