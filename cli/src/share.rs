// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;

use koidrive_driver::Paths;
use koidrive_driver::{Expiry, LinkState, Secret, SharedLink, TargetKind};
use koidrive_ipc::{Request, Response};
use rustix::termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcsetattr};

use crate::error::CliError;
use crate::report::{Done, NodeType, ShareLinkData, ShareLinksData, ShareState, SharedData};
use crate::{ago, ipc, now_ms, printable, unexpected};

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;
const LONGEST_MS: u64 = 365 * DAY_MS;

#[derive(clap::Subcommand)]
pub enum ShareAction {
    /// Make a read-only link to a file or folder in a linked folder, and print it. Anyone with the link can open and
    /// download what it shares, and a folder's link also shows what is added to the folder later.
    Create {
        /// The file or folder to share.
        path: PathBuf,
        /// How long the link works: a number of minutes, hours or days such as 90m, 12h or 30d, at most 365d; or
        /// never. Defaults to 7d.
        #[arg(long, value_parser = expiry, value_name = "TIME")]
        expires: Option<Expiry>,
        /// Ask for a password of 8 to 256 characters that the link needs before it opens. It is read from the
        /// terminal, or from the first line of standard input.
        #[arg(long)]
        password: bool,
    },
    /// List the links that still work, newest first.
    List {
        /// Also list the links that expired or were revoked.
        #[arg(long)]
        all: bool,
    },
    /// End a link at once.
    Revoke {
        /// The link's ID, as koi share list shows it, or the link itself.
        link: String,
    },
}

pub async fn run(paths: &Paths, action: ShareAction) -> Result<Done, CliError> {
    match action {
        ShareAction::Create {
            path,
            expires,
            password,
        } => {
            let request = Request::Share {
                path: crate::absolute(&path)?,
                expiry: expires.unwrap_or_default(),
                password: if password {
                    Some(read_password().await?)
                } else {
                    None
                },
            };
            let response = ipc::run(paths, request);
            tokio::pin!(response);
            let response = tokio::select! {
                biased;
                response = &mut response => response?,
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    eprintln!("Checking files before sharing...");
                    response.await?
                }
            };
            match response {
                Response::Shared { link } => Ok(created(&link, now_ms())),
                other => Err(unexpected(other)),
            }
        }
        ShareAction::List { all } => match ipc::run(paths, Request::ShareLinks).await? {
            Response::ShareLinks { mut links } => {
                if !all {
                    links.retain(|link| link.state == LinkState::Active);
                }
                let text = table(&links, all, now_ms());
                let data = ShareLinksData {
                    links: links.iter().map(data).collect(),
                };
                Ok(Done::ok(data, text))
            }
            other => Err(unexpected(other)),
        },
        ShareAction::Revoke { link } => {
            let request = Request::RevokeShareLink { link: Secret(link) };
            match ipc::run(paths, request).await? {
                Response::ShareLinkRevoked { link, already } => {
                    let shared = shown(&link);
                    if already {
                        let text = format!("The link to {shared} was already revoked.\n");
                        return Ok(Done::ok(data(&link), text).unchanged());
                    }
                    let text = format!("Revoked the link to {shared}. It no longer opens.\n");
                    Ok(Done::ok(data(&link), text).completed())
                }
                other => Err(unexpected(other)),
            }
        }
    }
}

fn created(link: &SharedLink, now_ms: i64) -> Done {
    let shared = shown(link);
    let what = match link.target.as_ref().map(|target| target.kind) {
        Some(TargetKind::Folder) => {
            format!("browse and download {shared}, with what is added to it later")
        }
        _ => format!("download {shared}"),
    };
    let until = match link.expires_at_ms {
        Some(expires) => format!("for {}", remaining(expires - now_ms)),
        None => "until you revoke it".to_owned(),
    };
    let password = match link.has_password {
        true => " They also need its password.",
        false => "",
    };
    let note = format!(
        "Anyone with this link can {what}, {until}.{password}\nTo end it: koi share revoke {}\n",
        link.id
    );
    let url = link.url.0.clone();
    let data = SharedData {
        url: url.clone(),
        link: data(link),
    };
    Done::value(data, &url, note)
}

fn table(links: &[SharedLink], all: bool, now_ms: i64) -> String {
    if links.is_empty() {
        return match all {
            true => "No share links.\n",
            false => "No share links work now. Make one with koi share create <path>.\n",
        }
        .to_owned();
    }
    let rows: Vec<[String; 4]> = (links.iter())
        .map(|link| {
            let state = match (link.state, link.expires_at_ms, link.revoked_at_ms) {
                (LinkState::Revoked, _, Some(revoked)) => {
                    format!("revoked {}", ago(revoked, now_ms))
                }
                (LinkState::Expired, Some(expires), _) => {
                    format!("expired {}", ago(expires, now_ms))
                }
                (_, Some(expires), _) => format!("in {}", remaining(expires - now_ms)),
                _ => "never".to_owned(),
            };
            let password = if link.has_password { "yes" } else { "" };
            [link.id.clone(), state, password.to_owned(), shown(link)]
        })
        .collect();
    let header = ["ID", "ENDS", "PASSWORD", "SHARES"].map(str::to_owned);
    let mut widths = [0; 4];
    for row in std::iter::once(&header).chain(&rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut table = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let cells: Vec<String> = (row.iter().zip(widths))
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        table.push_str(cells.join("  ").trim_end());
        table.push('\n');
    }
    table
}

fn shown(link: &SharedLink) -> String {
    let Some(target) = &link.target else {
        return format!(
            "a file or folder of {} whose path is unavailable",
            printable(&link.space_name)
        );
    };
    let place = match (&link.folder, target.path.is_empty()) {
        (Some(folder), true) => folder.clone(),
        (Some(folder), false) => format!("{folder}/{}", target.path),
        (None, true) => format!("the space {}", link.space_name),
        (None, false) => format!("{} in the space {}", target.path, link.space_name),
    };
    printable(&place)
}

fn remaining(ms: i64) -> String {
    let minutes = (ms.max(0) as u64 + MINUTE_MS / 2) / MINUTE_MS;
    match minutes {
        0..=1 => "about a minute".to_owned(),
        2..120 => format!("{minutes} minutes"),
        120..2880 => format!("{} hours", (minutes + 30) / 60),
        _ => format!("{} days", (minutes + 720) / 1440),
    }
}

fn data(link: &SharedLink) -> ShareLinkData {
    ShareLinkData {
        id: link.id.clone(),
        space_id: link.space_id.clone(),
        space_name: link.space_name.clone(),
        folder: link.folder.clone(),
        node_id: link.node_id.clone(),
        path: link.target.as_ref().map(|target| target.path.clone()),
        kind: link.target.as_ref().map(|target| match target.kind {
            TargetKind::File => NodeType::File,
            TargetKind::Folder => NodeType::Folder,
        }),
        created_at_ms: link.created_at_ms,
        expires_at_ms: link.expires_at_ms,
        revoked_at_ms: link.revoked_at_ms,
        has_password: link.has_password,
        state: match link.state {
            LinkState::Active => ShareState::Active,
            LinkState::Expired => ShareState::Expired,
            LinkState::Revoked => ShareState::Revoked,
        },
    }
}

fn expiry(text: &str) -> Result<Expiry, String> {
    if text == "never" {
        return Ok(Expiry::Never);
    }
    let units = [("m", MINUTE_MS), ("h", HOUR_MS), ("d", DAY_MS)];
    let ms = units
        .iter()
        .find_map(|(unit, ms)| Some((text.strip_suffix(unit)?.parse::<u64>().ok()?, ms)))
        .map(|(count, ms)| count.saturating_mul(*ms))
        .ok_or("give a number of minutes, hours or days, such as 90m, 12h or 30d, or never")?;
    if ms <= MINUTE_MS {
        return Err("a link must work for more than a minute".into());
    }
    if ms > LONGEST_MS {
        return Err("a link works for at most 365 days, unless it never expires".into());
    }
    Ok(Expiry::After { ms })
}

async fn read_password() -> Result<Secret, CliError> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return Ok(Secret(read_line().await?));
    }
    let password = hidden("Password for the link: ").await?;
    if hidden("The same password again: ").await? != password {
        return Err(CliError::usage("the two passwords differ"));
    }
    Ok(Secret(password))
}

async fn hidden(prompt: &str) -> Result<String, CliError> {
    let stdin = std::io::stdin();
    let failed = |error: std::io::Error| format!("reading the password failed: {error}");
    let echoing = tcgetattr(&stdin).map_err(|error| failed(error.into()))?;
    let mut quiet = echoing.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    eprint!("{prompt}");
    let mut restore = Echo(Some(echoing));
    tcsetattr(&stdin, OptionalActions::Now, &quiet).map_err(|error| failed(error.into()))?;
    let read = read_line().await;
    let restored = restore.restore();
    eprintln!();
    let line = read?;
    restored.map_err(|error| failed(error.into()))?;
    Ok(line)
}

struct Echo(Option<Termios>);

impl Echo {
    fn restore(&mut self) -> Result<(), rustix::io::Errno> {
        if let Some(echoing) = &self.0 {
            tcsetattr(std::io::stdin(), OptionalActions::Now, echoing)?;
            self.0 = None;
        }
        Ok(())
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

async fn read_line() -> Result<String, CliError> {
    let (sender, answer) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::stdin()
            .lock()
            .read_line(&mut line)
            .and_then(|read| {
                if read == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "stdin closed before a password",
                    ));
                }
                Ok(line.trim_end_matches(['\n', '\r']).to_owned())
            });
        let _ = sender.send(result);
    });
    answer
        .await
        .map_err(|_| "the password could not be read".to_owned())?
        .map_err(|error| format!("reading the password failed: {error}").into())
}

#[cfg(test)]
mod tests {
    use koidrive_driver::Target;

    use super::*;

    const URL: &str = "https://files.example.com/s/AAAAAAAAAAAAAAAAAAAAAA";

    fn link(kind: TargetKind, expires_at_ms: Option<i64>) -> SharedLink {
        SharedLink {
            id: "0123456789".into(),
            url: Secret(URL.into()),
            space_id: "s1".into(),
            space_name: "Team".into(),
            folder: Some("/home/ada/Team".into()),
            node_id: "n1".into(),
            target: Some(Target {
                path: "Plans/Q3".into(),
                kind,
            }),
            created_at_ms: 0,
            expires_at_ms,
            revoked_at_ms: None,
            has_password: false,
            state: LinkState::Active,
        }
    }

    #[test]
    fn an_expiry_is_minutes_hours_or_days_up_to_365_days_or_never() {
        assert_eq!(expiry("90m"), Ok(Expiry::After { ms: 90 * MINUTE_MS }));
        assert_eq!(expiry("12h"), Ok(Expiry::After { ms: 12 * HOUR_MS }));
        assert_eq!(expiry("365d"), Ok(Expiry::After { ms: LONGEST_MS }));
        assert_eq!(expiry("2m"), Ok(Expiry::After { ms: 2 * MINUTE_MS }));
        assert_eq!(expiry("never"), Ok(Expiry::Never));
        for refused in [
            "1m",
            "0d",
            "366d",
            "8761h",
            "30",
            "d",
            "-1d",
            "1.5d",
            "1w",
            "Never",
            "99999999999999999999d",
        ] {
            assert!(expiry(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_new_link_is_the_only_line_on_stdout_and_its_note_never_repeats_it() {
        let done = created(&link(TargetKind::Folder, Some(7 * DAY_MS as i64)), 0);
        assert_eq!(done.text, format!("{URL}\n"));
        assert_eq!(
            done.note,
            "Anyone with this link can browse and download /home/ada/Team/Plans/Q3, with what is added to it later, for 7 \
             days.\nTo end it: koi share revoke 0123456789\n"
        );
        assert_eq!(done.data["url"], URL);
        assert_eq!(done.data["id"], "0123456789");

        let file = SharedLink {
            has_password: true,
            ..link(TargetKind::File, None)
        };
        let done = created(&file, 0);
        assert_eq!(
            done.note,
            "Anyone with this link can download /home/ada/Team/Plans/Q3, until you revoke it. They also need its \
             password.\nTo end it: koi share revoke 0123456789\n"
        );
    }

    #[test]
    fn a_time_left_reads_in_the_nearest_whole_unit() {
        let left = |ms: u64| remaining(ms as i64);
        assert_eq!(left(2 * HOUR_MS - 900), "2 hours");
        assert_eq!(left(7 * DAY_MS - 5_000), "7 days");
        assert_eq!(left(90 * MINUTE_MS), "90 minutes");
        assert_eq!(left(30 * HOUR_MS), "30 hours");
        assert_eq!(left(20_000), "about a minute");
    }

    #[test]
    fn the_list_shows_each_link_without_its_url() {
        let now = 10 * DAY_MS as i64;
        let unlinked = SharedLink {
            id: "abcdefabcd".into(),
            folder: None,
            has_password: true,
            ..link(TargetKind::File, None)
        };
        let revoked = SharedLink {
            id: "1111111111".into(),
            target: None,
            state: LinkState::Revoked,
            revoked_at_ms: Some(now - 3 * HOUR_MS as i64),
            ..link(TargetKind::File, Some(now + DAY_MS as i64))
        };
        let links = [
            link(TargetKind::Folder, Some(now + 3 * DAY_MS as i64)),
            unlinked,
            revoked,
        ];
        assert_eq!(
            table(&links, true, now),
            "ID          ENDS             PASSWORD  SHARES\n\
             0123456789  in 3 days                  /home/ada/Team/Plans/Q3\n\
             abcdefabcd  never            yes       Plans/Q3 in the space Team\n\
             1111111111  revoked 3 h ago            a file or folder of Team whose path is unavailable\n"
        );
        let data = serde_json::to_string(&ShareLinksData {
            links: links.iter().map(data).collect(),
        })
        .unwrap();
        assert!(!data.contains("AAAAAAAAAAAAAAAAAAAAAA"), "{data}");
        assert_eq!(
            table(&[], false, now),
            "No share links work now. Make one with koi share create <path>.\n"
        );
    }
}
