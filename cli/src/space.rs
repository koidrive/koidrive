// SPDX-License-Identifier: Apache-2.0

use crate::error::CliError;
use crate::formatting::{count, printable, size};
use crate::options::short_name;
use crate::output;
use crate::profile::signed_in;
use crate::report::{Done, RefusedSpace, SpaceData, SpacesData, UsageData, UsagesData};
use clap::Subcommand;
use koidrive_driver::{DriverError, Paths};
use koidrive_ipc::Code;
use koidrive_net::Auth;
use koidrive_proto::v1::{SpaceInfo, SpaceUsage};

#[derive(Subcommand)]
pub(crate) enum SpaceAction {
    /// Create a space. Link a folder to it with koi folder link.
    Create {
        /// The space's name, at most 255 bytes.
        #[arg(value_parser = short_name)]
        name: String,
    },
    /// List the spaces of the server.
    List,
    /// Show how many bytes each space stores: in live files, in the trash and in old versions.
    Usage,
}

/// Runs a space command. Creating a name another space has returns that space, also when another run creates it at the
/// same time, since koi folder link names a space by its name and the server keeps names unique.
pub(crate) async fn space(paths: Paths, action: SpaceAction) -> Result<Done, CliError> {
    let auth = signed_in(&paths)?;
    let spaces = auth
        .api()
        .list_spaces(&auth)
        .await
        .map_err(CliError::api_retry_safe)?
        .spaces;
    match action {
        SpaceAction::List => {
            let data = SpacesData {
                spaces: spaces.iter().map(SpaceData::of).collect(),
            };
            Ok(Done::ok(data, space_table(&spaces)))
        }
        SpaceAction::Usage => usage(&auth, spaces).await,
        SpaceAction::Create { name } => {
            let name = name.trim();
            if name.is_empty() {
                return Err(CliError::usage("a space needs a name"));
            }
            let named = named_space(&paths, &auth, &spaces, name).await?;
            let space = named.space();
            let id = printable(&space.space_id);
            let shown = format!("{} ({id})", printable(&space.name));
            let said = match &named {
                Named::Existing(_) => format!("The space {shown} already exists."),
                Named::Created(_) => format!("Created the space {shown}."),
            };
            let note = format!(
                "{said}\nLink a folder to it with koi folder link <folder> --space {id}.\n"
            );
            let done = Done::value(SpaceData::of(space), &id, note);
            let summary = match &named {
                Named::Existing(_) => format!(
                    "The shared folder {} already exists.\n",
                    printable(&space.name)
                ),
                Named::Created(_) => {
                    format!("Created the shared folder {}.\n", printable(&space.name))
                }
            };
            let summary = format!(
                "{summary}Link a folder with:\n  koi folder link <folder> --space {}\n",
                output::command_word(&space.space_id)
            );
            let done = done.human(format!("{id}\n"), summary);
            Ok(match named {
                Named::Existing(_) => done.unchanged(),
                Named::Created(_) => done,
            })
        }
    }
}

pub(crate) enum Named {
    Existing(SpaceInfo),
    Created(SpaceInfo),
}

impl Named {
    pub(crate) fn space(&self) -> &SpaceInfo {
        match self {
            Named::Existing(space) | Named::Created(space) => space,
        }
    }
}

pub(crate) async fn named_space(
    paths: &Paths,
    auth: &Auth,
    spaces: &[SpaceInfo],
    name: &str,
) -> Result<Named, CliError> {
    if let Some(space) = existing_space(spaces, name)?
        && !koidrive_driver::space_create_pending(paths, auth, name)
    {
        return Ok(Named::Existing(space));
    }
    match koidrive_driver::create_space(paths, auth, name).await {
        Ok(created) => Ok(Named::Created(created)),
        Err(DriverError::Api(error)) if error.status() == Some(409) => {
            let now = auth
                .api()
                .list_spaces(auth)
                .await
                .map_err(CliError::api_retry_safe)?;
            match existing_space(&now.spaces, name)? {
                Some(space) => Ok(Named::Existing(space)),
                None => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn existing_space(spaces: &[SpaceInfo], name: &str) -> Result<Option<SpaceInfo>, CliError> {
    let matches: Vec<_> = spaces.iter().filter(|space| space.name == name).collect();
    match matches.as_slice() {
        [] => Ok(None),
        [space] => Ok(Some((*space).clone())),
        _ => Err(koidrive_driver::DriverError::SpaceAmbiguous {
            name: name.to_owned(),
            ids: matches
                .into_iter()
                .map(|space| space.space_id.clone())
                .collect(),
        }
        .into()),
    }
}

/// One line per space: its ID and its name.
pub(crate) fn space_table(spaces: &[SpaceInfo]) -> String {
    if spaces.is_empty() {
        return "No spaces. Create one with koi space create <name>.\n".to_owned();
    }
    let mut table = format!("{:<36}  NAME\n", "ID");
    for space in spaces {
        table.push_str(&format!(
            "{:<36}  {}\n",
            printable(&space.space_id),
            printable(&space.name)
        ));
    }
    table
}

/// Shows what each space of the account stores (RFC 0010). A space the server refuses, such as one a rewind holds
/// closed, does not hide the others: they are reported, and the command then fails naming the refused ones. A refused
/// credential stops it, since every later space would refuse it too, and the failure keeps what was read before.
pub(crate) async fn usage(auth: &Auth, spaces: Vec<SpaceInfo>) -> Result<Done, CliError> {
    let mut usages = Vec::new();
    let mut refused = Vec::new();
    for space in spaces {
        match auth.api().space_usage(auth, &space.space_id).await {
            Ok(usage) => usages.push((space, usage)),
            Err(error) if error.is_unauthenticated() => {
                let mut error = CliError::api_retry_safe(error);
                if !(usages.is_empty() && refused.is_empty()) {
                    error.partial = Some(Box::new(usage_read(&usages, &refused)));
                }
                return Err(error);
            }
            Err(error) => refused.push((space, CliError::api_retry_safe(error))),
        }
    }
    usage_done(&usages, &refused)
}

/// The usage of the spaces that answered, which fails naming the spaces the server refused. Those add nothing to the
/// data but their names: unavailable when each refusal may pass, failed otherwise.
pub(crate) fn usage_done(
    usages: &[(SpaceInfo, SpaceUsage)],
    refused: &[(SpaceInfo, CliError)],
) -> Result<Done, CliError> {
    let done = usage_read(usages, refused);
    if refused.is_empty() {
        return Ok(done);
    }
    let code = match refused
        .iter()
        .all(|(_, error)| error.code == Code::Unavailable)
    {
        true => Code::Unavailable,
        false => Code::Failed,
    };
    let names: Vec<String> = (refused.iter())
        .map(|(space, error)| {
            format!(
                "{} ({}): {}",
                printable(&space.name),
                space.space_id,
                error.message
            )
        })
        .collect();
    let mut error = CliError::new(code, format!("no usage for {}", names.join("; ")));
    error.partial = Some(Box::new(done));
    Err(error)
}

/// What was read: the usage of the spaces that answered and the names of those the server refused.
fn usage_read(usages: &[(SpaceInfo, SpaceUsage)], refused: &[(SpaceInfo, CliError)]) -> Done {
    let data = UsagesData {
        spaces: (usages.iter())
            .map(|(space, usage)| UsageData::of(space, usage))
            .collect(),
        refused: (refused.iter())
            .map(|(space, error)| RefusedSpace {
                space_id: space.space_id.clone(),
                name: space.name.clone(),
                message: error.message.clone(),
            })
            .collect(),
    };
    let summary = if usages.is_empty() && !refused.is_empty() {
        String::new()
    } else if usages.is_empty() {
        "No shared folders.\n".to_owned()
    } else {
        let mut summary = String::new();
        for (space, usage) in usages {
            summary.push_str(&format!(
                "{}\n  Live files: {}, {}\n  Trash: {}, {}\n  Old versions: {}, {}\n  Other stored data: {}\n  Total: {}\n",
                printable(&space.name), count(usage.live_files, "file"), size(usage.live_bytes), count(usage.trashed_files, "file"), size(usage.trash_bytes),
                count(usage.old_versions, "version"), size(usage.old_version_bytes), size(usage.unreferenced_bytes), size(total_bytes(usage)),
            ));
        }
        summary.push_str("Totals count shared data once.\n");
        summary
    };
    Done::ok(data, usage_output(usages, refused.len())).human(summary, "")
}

/// What `koi space usage` prints on stdout: the spaces that answered. It prints nothing when every space refused, so
/// that stdout never shows an empty account that is not empty; the error names them instead.
fn usage_output(usages: &[(SpaceInfo, SpaceUsage)], refused: usize) -> String {
    if usages.is_empty() && refused > 0 {
        return String::new();
    }
    usage_report(usages)
}

pub(crate) fn total_bytes(usage: &SpaceUsage) -> u64 {
    usage.live_bytes + usage.trash_bytes + usage.old_version_bytes + usage.unreferenced_bytes
}

/// One block per space: its live files, trash, old versions and unreferenced chunks, then a note on how they count.
pub(crate) fn usage_report(usages: &[(SpaceInfo, SpaceUsage)]) -> String {
    if usages.is_empty() {
        return "No spaces.\n".to_owned();
    }
    let mut report = String::new();
    for (space, usage) in usages {
        let rows = [
            (
                "Live files",
                count(usage.live_files, "file"),
                usage.live_bytes,
            ),
            (
                "Trash",
                count(usage.trashed_files, "file"),
                usage.trash_bytes,
            ),
            (
                "Old versions",
                count(usage.old_versions, "version"),
                usage.old_version_bytes,
            ),
            ("Unreferenced", String::new(), usage.unreferenced_bytes),
            ("Total", String::new(), total_bytes(usage)),
        ];
        report.push_str(&format!(
            "{} ({})\n",
            printable(&space.name),
            space.space_id
        ));
        for (label, what, bytes) in rows {
            report.push_str(&format!("  {label:<14}{what:>14}  {:>9}\n", size(bytes)));
        }
    }
    report.push_str(
        "\nEach stored chunk counts once, under the first of live files, trash and old versions that holds it.\n\
         Unreferenced chunks are uploads not committed yet and chunks waiting out the grace period before deletion.\n",
    );
    report
}
