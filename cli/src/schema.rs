// SPDX-License-Identifier: Apache-2.0

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Map, Value, json};

use crate::manifest::file_name;
use crate::report::{
    AccountData, AnsweredData, ChoicesData, DaemonLine, DevicesData, DiagnosticsData, Empty,
    Envelope, FolderData, HelpData, InvitationData, LeftData, RequestsData, RestoreAskedData,
    RevokedData, SCHEMA_VERSION, SetupData, ShareLinkData, ShareLinksData, SharedData, SpaceData,
    SpacesData, StatusData, TrashData, UnlinkedData, UpdatedData, UsagesData, VersionInfoData,
    VersionsData, WithApp, WithService,
};
use crate::telemetry::{ChoiceData, TelemetryStatusData};

type Data = fn(&mut SchemaGenerator) -> Schema;

fn of<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<T>()
}

fn data(command: &str) -> Option<&'static [Data]> {
    Some(match command {
        "" => &[of::<VersionInfoData>],
        "status" => &[of::<StatusData>],
        "resolve" => &[of::<AnsweredData>],
        "help" | "server" | "server upgrade" | "server delete" => &[],
        "space" | "folder" | "service" | "device" | "history" | "share" | "telemetry"
        | "_daemon" => &[],
        "setup" => &[
            of::<WithApp<WithService<ChoicesData>>>,
            of::<WithApp<WithService<SetupData>>>,
            of::<WithApp<WithService<Empty>>>,
        ],
        "join" | "server claim" => &[of::<WithService<AccountData>>],
        "server deploy" | "server add-device" | "service start" => &[of::<WithService<Empty>>],
        "_reconcile" => &[of::<WithApp<WithService<Empty>>>],
        "pause" => &[of::<Empty>],
        "resume" => &[of::<WithService<Empty>>],
        "space create" => &[of::<SpaceData>],
        "space list" => &[of::<SpacesData>],
        "space usage" => &[of::<UsagesData>],
        "folder link" => &[of::<WithService<FolderData>>],
        "folder restart" | "folder retry" => &[of::<FolderData>],
        "folder unlink" => &[of::<WithService<UnlinkedData>>],
        "device invite" => &[of::<InvitationData>],
        "device list" => &[of::<DevicesData>],
        "device revoke" => &[of::<RevokedData>],
        "device leave" => &[of::<WithService<LeftData>>],
        "history trash" => &[of::<TrashData>],
        "history versions" => &[of::<VersionsData>],
        "history restore" => &[of::<RestoreAskedData>],
        "history requests" => &[of::<RequestsData>],
        "share create" => &[of::<SharedData>],
        "share list" => &[of::<ShareLinksData>],
        "share revoke" => &[of::<ShareLinkData>],
        "telemetry status" => &[of::<TelemetryStatusData>],
        "telemetry on" | "telemetry off" | "telemetry reset" => &[of::<ChoiceData>],
        "telemetry preview" => &[of::<Map<String, Value>>],
        "diagnose" => &[of::<DiagnosticsData>],
        "update" => &[of::<UpdatedData>],
        "uninstall" => &[of::<crate::uninstall::UninstalledData>],
        _ => return None,
    })
}

pub fn envelope(command: &str) -> Option<Value> {
    let data = data(command)?;
    let mut generator = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator();
    let mut kinds = vec![
        json!({ "type": "null" }),
        of::<HelpData>(&mut generator).to_value(),
    ];
    kinds.extend(data.iter().map(|data| data(&mut generator).to_value()));
    let mut root = generator.root_schema_for::<Envelope>().to_value();
    let shown = match command {
        "" => "koi --json".to_owned(),
        command => format!("koi {command} --json"),
    };
    root["title"] = json!(shown);
    let properties = &mut root["properties"];
    properties["schema_version"] = json!({ "const": SCHEMA_VERSION });
    properties["command"] = match command {
        "help" => json!({ "type": "string", "pattern": "^help( |$)" }),
        command => json!({ "const": command }),
    };
    properties["data"] = json!({ "anyOf": kinds });
    root["if"] = json!({ "properties": { "outcome": { "const": "error" } } });
    root["then"] = json!({ "properties": { "error": { "type": "object" } } });
    root["else"] = json!({ "properties": { "error": { "type": "null" } } });
    let line = (command == "_daemon").then(|| of::<DaemonLine>(&mut generator).to_value());
    let mut defs = match root.get_mut("$defs").map(Value::take) {
        Some(Value::Object(defs)) => defs,
        _ => Map::new(),
    };
    defs.extend(generator.take_definitions(true));
    if let Some(line) = defs.get_mut("DaemonLine") {
        line["properties"]["schema_version"] = json!({ "const": SCHEMA_VERSION });
    }
    let Some(line) = line else {
        root["$defs"] = Value::Object(defs);
        return Some(root);
    };
    let mut envelope = root.as_object().cloned().unwrap_or_default();
    let header = ["$schema", "title"].map(|key| (key, envelope.remove(key)));
    envelope.remove("$defs");
    let mut root = Map::new();
    root.extend(
        header
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_owned(), value?))),
    );
    root.insert("anyOf".to_owned(), json!([envelope, line]));
    root.insert("$defs".to_owned(), Value::Object(defs));
    Some(Value::Object(root))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;
    use crate::report::Done;

    fn commands() -> Vec<String> {
        crate::manifest::commands("")
            .into_iter()
            .map(|info| info.command)
            .collect()
    }

    #[test]
    fn every_command_has_a_schema_and_no_schema_names_a_missing_command() {
        let commands = commands();
        let missing: Vec<_> = (commands.iter())
            .filter(|command| data(command).is_none())
            .collect();
        assert!(missing.is_empty(), "no schema for {missing:?}");
        assert!(commands.contains(&"folder link".to_owned()), "{commands:?}");
    }

    #[test]
    fn app_reports_are_optional_but_validated_when_present() {
        for command in ["setup", "_reconcile"] {
            let validator = jsonschema::validator_for(&envelope(command).unwrap()).unwrap();
            let mut printed = json!({
                "schema_version": 1,
                "command": command,
                "outcome": "error",
                "data": {"service": {"manager":"launchd", "name":"fixture", "state":"unavailable"}},
                "error": {"code":"failed", "message":"registration failed", "retryable":false},
                "decisions": [],
                "next_commands": [],
            });
            assert!(validator.is_valid(&printed), "{printed:#}");
            printed["data"]["app"] = json!({"state":"skipped", "reason":"ci"});
            assert!(validator.is_valid(&printed), "{printed:#}");
            for invalid in [json!({}), json!({"state":"unknown"}), json!(false)] {
                printed["data"]["app"] = invalid;
                assert!(!validator.is_valid(&printed), "{printed:#}");
            }
        }
    }

    #[test]
    fn the_published_schemas_match_the_cli_types() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/json-schema/v1");
        let write = std::env::var_os("KOI_WRITE_SCHEMAS").is_some();
        let mut expected = BTreeSet::new();
        let mut stale = Vec::new();
        for command in commands() {
            let name = file_name(&command);
            let text = serde_json::to_string_pretty(&envelope(&command).unwrap()).unwrap() + "\n";
            let path = dir.join(&name);
            if write {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &text).unwrap();
            } else if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                stale.push(name.clone());
            }
            expected.insert(name);
        }
        let present: BTreeSet<_> = (std::fs::read_dir(&dir).into_iter().flatten())
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let extra: Vec<_> = present.difference(&expected).collect();
        assert!(
            stale.is_empty() && extra.is_empty(),
            "out of date: {stale:?}, not a command: {extra:?}; run KOI_WRITE_SCHEMAS=1 cargo test -p koidrive-cli the_published_schemas_match_the_cli_types"
        );
    }

    #[test]
    fn a_schema_takes_the_envelope_its_command_prints_and_refuses_another() {
        let validator = jsonschema::validator_for(&envelope("device revoke").unwrap()).unwrap();
        let done = Done::ok(
            crate::report::RevokedData {
                device_id: "d1".into(),
            },
            "",
        );
        let printed = serde_json::to_value(Envelope::of("device revoke", done)).unwrap();
        assert!(validator.is_valid(&printed), "{printed:#}");
        let mut other = printed.clone();
        other["command"] = json!("device list");
        assert!(!validator.is_valid(&other));
        other = printed.clone();
        other["data"] = json!({ "device_id": 1 });
        assert!(!validator.is_valid(&other));
        other = printed;
        other["error"] = json!({ "code": "failed", "message": "m", "retryable": false });
        assert!(!validator.is_valid(&other));
        other["outcome"] = json!("error");
        other["error"] = Value::Null;
        assert!(!validator.is_valid(&other));
        let lines = jsonschema::validator_for(&envelope("_daemon").unwrap()).unwrap();
        let line =
            |event| serde_json::from_str::<Value>(&crate::report::Lifecycle::line(&event)).unwrap();
        let mut stopping = line(crate::report::Lifecycle::Stopping {
            signal: crate::report::Signal::Term,
        });
        assert!(lines.is_valid(&stopping), "{stopping:#}");
        stopping["signal"] = json!("SIGHUP");
        assert!(!lines.is_valid(&stopping));
        let mut stopped = line(crate::report::Lifecycle::Stopped);
        stopped["schema_version"] = json!(2);
        assert!(!lines.is_valid(&stopped));
    }
}
