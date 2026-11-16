// SPDX-License-Identifier: Apache-2.0

use url::Url;

#[derive(Debug, PartialEq, Eq)]
pub struct Invitation {
    pub code: String,
    pub server: String,
}

pub fn parse(text: &str, server: Option<&str>) -> Result<Invitation, String> {
    let text = text.trim();
    let (code, host) = match text.rsplit_once('@') {
        Some((code, host)) => (code, Some(host)),
        None => (text, None),
    };
    if code.is_empty() {
        return Err(format!("{text:?} has no pairing code before the @"));
    }
    let server = match (server, host) {
        (Some(server), _) => server.to_owned(),
        (None, Some(host)) if !host.is_empty() && !host.contains('/') => format!("https://{host}"),
        (None, Some(host)) => return Err(format!("{host:?} is not a server host name")),
        (None, None) => {
            return Err(format!(
                "{text:?} names no server; use the code shown as code@host, or add --server"
            ));
        }
    };
    Ok(Invitation {
        code: code.to_owned(),
        server,
    })
}

pub fn join_argv(code: &str, server: &Url) -> Vec<String> {
    match (
        server.scheme(),
        server.host_str(),
        server.path(),
        server.query(),
    ) {
        ("https", Some(host), "/", None) => match server.port() {
            Some(port) => vec![format!("{code}@{host}:{port}")],
            None => vec![format!("{code}@{host}")],
        },
        _ => vec![code.to_owned(), "--server".to_owned(), server.to_string()],
    }
}

pub fn join_args(code: &str, server: &Url) -> String {
    let words: Vec<String> = join_argv(code, server)
        .iter()
        .map(|word| shell_word(word))
        .collect();
    words.join(" ")
}

pub fn shell_word(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c))
    {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}

pub fn server_url(text: &str) -> String {
    let text = text.trim();
    let server = if text.contains("://") {
        text.to_owned()
    } else {
        format!("https://{text}")
    };
    if let Ok(mut url) = Url::parse(&server)
        && let Some(path) = url.path().trim_end_matches('/').strip_suffix("/setup")
    {
        let path = path.to_owned();
        url.set_path(&path);
        return url.as_str().trim_end_matches('/').to_owned();
    }
    server
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_carries_its_server() {
        assert_eq!(
            parse(" K7QM-2XPD@koidrive.alice.workers.dev ", None),
            Ok(Invitation {
                code: "K7QM-2XPD".into(),
                server: "https://koidrive.alice.workers.dev".into(),
            })
        );
    }

    #[test]
    fn the_last_at_sign_separates_the_host() {
        assert_eq!(parse("a@b@files.example.com", None).unwrap().code, "a@b");
    }

    #[test]
    fn server_replaces_the_host_and_completes_a_bare_code() {
        let local = Some("http://127.0.0.1:8787");
        assert_eq!(
            parse("K7QM-2XPD", local).unwrap().server,
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            parse("K7QM-2XPD@files.example.com", local).unwrap().server,
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn join_args_read_back_as_the_same_code_and_server() {
        for server in [
            "https://koidrive.alice.workers.dev",
            "https://files.example.com:8443",
            "http://127.0.0.1:8787",
            "https://files.example.com/koidrive/",
        ] {
            let url = Url::parse(server).unwrap();
            let args = join_args("K7QM-2XPD", &url);
            let (code, flag) = match args.split_once(" --server ") {
                Some((code, flag)) => (code, Some(flag)),
                None => (args.as_str(), None),
            };
            let read = parse(code, flag).unwrap();
            assert_eq!(read.code, "K7QM-2XPD", "{args}");
            assert_eq!(Url::parse(&read.server).unwrap(), url, "{args}");
        }
        assert_eq!(
            join_args(
                "K7QM-2XPD",
                &Url::parse("https://koidrive.alice.workers.dev").unwrap()
            ),
            "K7QM-2XPD@koidrive.alice.workers.dev"
        );
    }

    #[test]
    fn a_shell_reads_the_printed_join_arguments_literally() {
        for (code, server) in [
            ("ABCD-EFGH$(echo)", "https://files.example.com/"),
            ("ABCD-EFGH", "https://files.example.com/$(echo)/"),
            ("AB'CD `echo`", "http://127.0.0.1:8787/"),
        ] {
            let url = Url::parse(server).unwrap();
            let script = format!("printf '%s\\n' {}", join_args(code, &url));
            let shell = std::process::Command::new("sh")
                .args(["-c", &script])
                .output()
                .unwrap();
            assert!(shell.status.success(), "{script}");
            let words = String::from_utf8(shell.stdout).unwrap();
            let words: Vec<&str> = words.lines().collect();
            let read = match words.as_slice() {
                [code] => parse(code, None),
                [code, "--server", server] => parse(code, Some(server)),
                _ => panic!("{script} printed {words:?}"),
            }
            .unwrap();
            assert_eq!(read.code, code, "{script}");
            assert_eq!(Url::parse(&read.server).unwrap(), url, "{script}");
        }
    }

    #[test]
    fn a_bare_server_host_gets_https() {
        assert_eq!(
            server_url(" koidrive.alice.workers.dev "),
            "https://koidrive.alice.workers.dev"
        );
        assert_eq!(server_url("http://127.0.0.1:8787"), "http://127.0.0.1:8787");
    }

    #[test]
    fn a_pasted_setup_page_names_its_server() {
        for pasted in [
            "https://koidrive.alice.workers.dev/setup",
            "https://koidrive.alice.workers.dev/setup/",
            "koidrive.alice.workers.dev/setup",
        ] {
            assert_eq!(server_url(pasted), "https://koidrive.alice.workers.dev");
        }
        assert_eq!(
            server_url("https://files.example.com/koidrive/setup"),
            "https://files.example.com/koidrive"
        );
    }

    #[test]
    fn a_host_named_setup_is_not_removed_as_a_path() {
        for (input, expected) in [
            ("http://setup", "http://setup"),
            ("https://setup", "https://setup"),
            ("setup", "https://setup"),
            ("http://setup/setup", "http://setup"),
        ] {
            assert_eq!(server_url(input), expected, "{input}");
        }
    }

    #[test]
    fn refuses_a_code_without_a_server_or_a_bad_host() {
        assert!(parse("K7QM-2XPD", None).unwrap_err().contains("--server"));
        assert!(parse("@files.example.com", None).is_err());
        assert!(parse("K7QM-2XPD@", None).is_err());
        assert!(parse("K7QM-2XPD@https://files.example.com", None).is_err());
    }
}
