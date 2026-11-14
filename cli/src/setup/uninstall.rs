// SPDX-License-Identifier: Apache-2.0

use koidrive_cloudflare::{Jurisdiction, Script};

use super::Cloud;
use super::state::{SetupState, StateFile, Step};
use crate::error::CliError;

const SERVER_PREFIX: &str = "spaces/";

#[derive(Debug, PartialEq, Eq)]
pub struct Contents {
    pub objects: usize,
    pub foreign: Option<String>,
}

pub async fn plan(
    cloud: &mut impl Cloud,
    script: Option<&Script>,
    bucket: &str,
    jurisdiction: Jurisdiction,
) -> Result<(SetupState, Contents), CliError> {
    let script_created_on = match script {
        None => None,
        Some(script) => Some(script.created_on.clone().ok_or_else(|| {
            format!(
                "Cloudflare did not say when the Worker {} was created, so a rerun could not tell it apart from a new Worker of that name; deleted nothing",
                script.id
            )
        })?),
    };
    let account = cloud.account().to_owned();
    let found = cloud
        .fresh()
        .await?
        .bucket(&account, bucket, jurisdiction)
        .await
        .map_err(|error| match error.status() {
            Some(404) => CliError::usage(format!(
                "there is no R2 bucket {bucket} in the {} jurisdiction",
                jurisdiction.as_str()
            )),
            _ => {
                let error = CliError::caused(
                    format!("cannot read the bucket {bucket}: {error}; deleted nothing"),
                    error,
                );
                let human = format!(
                    "{} Nothing was deleted.",
                    error.human.as_deref().unwrap_or(&error.message)
                );
                error.human(human)
            }
        })?;
    if found.creation_date.is_empty()
        || found
            .jurisdiction
            .is_some_and(|other| other != jurisdiction)
    {
        return Err(format!(
            "Cloudflare did not describe the bucket {bucket} in the {} jurisdiction with its creation date; deleted nothing",
            jurisdiction.as_str()
        ).into());
    }
    let contents = scan(cloud, bucket, jurisdiction)
        .await?
        .ok_or_else(|| format!("the bucket {bucket} was deleted meanwhile; deleted nothing"))?;
    let state = SetupState {
        account,
        script: script.map(|script| script.id.clone()).unwrap_or_default(),
        bucket: bucket.to_owned(),
        step: Step::Uninstalling,
        script_created_on,
        jurisdiction,
        bucket_created_on: Some(found.creation_date),
        ..SetupState::default()
    };
    Ok((state, contents))
}

pub async fn finish(
    cloud: &mut impl Cloud,
    file: &StateFile,
    mut state: SetupState,
) -> Result<(), CliError> {
    let account = cloud.account().to_owned();
    let jurisdiction = state.jurisdiction;
    if !state.script.is_empty() {
        delete_worker(cloud, &state).await?;
        state.script.clear();
        state.script_created_on = None;
        file.save(&state)?;
    }
    let bucket = state.bucket.clone();
    let found = match cloud
        .fresh()
        .await?
        .bucket(&account, &bucket, jurisdiction)
        .await
    {
        Ok(found) => found,
        Err(error) if error.status() == Some(404) => return gone(file, &bucket),
        Err(error) => {
            return Err(CliError::known_outcome(
                format!("cannot read the bucket {bucket}: {error}; run the command again"),
                error,
            ));
        }
    };
    if state.bucket_created_on.as_ref() != Some(&found.creation_date)
        || found
            .jurisdiction
            .is_some_and(|other| other != jurisdiction)
    {
        file.remove()?;
        println!(
            "The bucket {bucket} was created again after this uninstall began, so it is another bucket. Deleted nothing in it."
        );
        return Ok(());
    }
    if let Some(other) = binder(cloud, &bucket, jurisdiction).await? {
        file.remove()?;
        println!(
            "Kept the bucket {bucket} and its files, because the Worker {} also uses it.",
            crate::printable(&other)
        );
        return Ok(());
    }
    let Some(contents) = scan(cloud, &bucket, jurisdiction).await? else {
        return gone(file, &bucket);
    };
    if let Some(key) = contents.foreign {
        file.remove()?;
        println!(
            "Kept the bucket {bucket} and its files, because it holds {}, which Koidrive does not write: another application stores data there.",
            crate::printable(&key)
        );
        return Ok(());
    }
    println!("Emptying the bucket {bucket}...");
    loop {
        let api = cloud.fresh().await?;
        let page = match api.objects(&account, &bucket, jurisdiction, None).await {
            Ok(page) => page,
            Err(error) if error.status() == Some(404) => return gone(file, &bucket),
            Err(error) => {
                return Err(CliError::known_outcome(
                    format!("cannot list the bucket {bucket}: {error}; run the command again"),
                    error,
                ));
            }
        };
        if page.keys.is_empty() {
            break;
        }
        for key in &page.keys {
            if !key.starts_with(SERVER_PREFIX) {
                return Err(format!(
                    "the bucket {bucket} now holds {key}, which Koidrive does not write, so it stopped emptying it; run the command again"
                ).into());
            }
            api.delete_object(&account, &bucket, jurisdiction, key)
                .await
                .map_err(|error| {
                    CliError::known_outcome(
                        format!(
                            "cannot delete {key} from {bucket}: {error}; run the command again"
                        ),
                        error,
                    )
                })?;
        }
    }
    cloud
        .fresh()
        .await?
        .delete_bucket(&account, &bucket, jurisdiction)
        .await
        .map_err(|error| {
            CliError::known_outcome(
                format!("cannot delete the bucket {bucket}: {error}; run the command again"),
                error,
            )
        })?;
    file.remove()?;
    println!("Deleted the bucket {bucket}. The uninstall is complete.");
    Ok(())
}

fn gone(file: &StateFile, bucket: &str) -> Result<(), CliError> {
    file.remove()?;
    println!("The bucket {bucket} is already gone. The uninstall is complete.");
    Ok(())
}

async fn delete_worker(cloud: &mut impl Cloud, state: &SetupState) -> Result<(), CliError> {
    let account = cloud.account().to_owned();
    let api = cloud.fresh().await?;
    let scripts = api.scripts(&account).await.map_err(|error| {
        CliError::known_outcome(
            format!("cannot list the account's Workers: {error}; run the command again"),
            error,
        )
    })?;
    let Some(script) = scripts.iter().find(|script| script.id == state.script) else {
        println!("The Worker {} is already gone.", state.script);
        return Ok(());
    };
    match (&script.created_on, &state.script_created_on) {
        (Some(now), Some(recorded)) if now == recorded => {
            println!("Deleting the Worker {}...", state.script);
            api.delete_script(&account, &state.script)
                .await
                .map_err(|error| {
                    CliError::known_outcome(format!(
                        "cannot delete the Worker {}: {error}; run the command again",
                        state.script
                    ), error)
                })
        }
        (Some(_), Some(_)) => {
            println!(
                "Left the Worker {} alone: it was created after this uninstall began, so it is a new Worker with that name.",
                state.script
            );
            Ok(())
        }
        _ => Err(format!(
            "Cloudflare did not say when the Worker {} was created, so it cannot be told apart from a new Worker with that name; deleted nothing",
            state.script
        ).into()),
    }
}

async fn binder(
    cloud: &mut impl Cloud,
    bucket: &str,
    jurisdiction: Jurisdiction,
) -> Result<Option<String>, CliError> {
    let account = cloud.account().to_owned();
    let api = cloud.fresh().await?;
    let scripts = api.scripts(&account).await.map_err(|error| {
        CliError::known_outcome(
            format!("cannot list the account's Workers: {error}; run the command again"),
            error,
        )
    })?;
    for script in scripts {
        let settings = api
            .script_settings(&account, &script.id)
            .await
            .map_err(|error| {
                CliError::known_outcome(
                    format!(
                        "cannot read the settings of {}: {error}; run the command again",
                        script.id
                    ),
                    error,
                )
            })?;
        let binds = settings.bindings.iter().any(|binding| {
            binding.kind == "r2_bucket"
                && binding.bucket_name.as_deref() == Some(bucket)
                && binding.jurisdiction.unwrap_or_default() == jurisdiction
        });
        if binds {
            return Ok(Some(script.id));
        }
    }
    Ok(None)
}

async fn scan(
    cloud: &mut impl Cloud,
    bucket: &str,
    jurisdiction: Jurisdiction,
) -> Result<Option<Contents>, CliError> {
    let account = cloud.account().to_owned();
    let mut contents = Contents {
        objects: 0,
        foreign: None,
    };
    let mut cursor: Option<String> = None;
    loop {
        let page = match cloud
            .fresh()
            .await?
            .objects(&account, bucket, jurisdiction, cursor.as_deref())
            .await
        {
            Ok(page) => page,
            Err(error) if error.status() == Some(404) => return Ok(None),
            Err(error) => {
                let error = CliError::caused(
                    format!("cannot list the bucket {bucket}: {error}; deleted nothing"),
                    error,
                );
                let human = format!(
                    "{} Nothing was deleted.",
                    error.human.as_deref().unwrap_or(&error.message)
                );
                return Err(error.human(human));
            }
        };
        contents.objects += page.keys.len();
        if contents.foreign.is_none() {
            contents.foreign = page
                .keys
                .into_iter()
                .find(|key| !key.starts_with(SERVER_PREFIX));
        }
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(Some(contents)),
        }
    }
}

#[cfg(test)]
mod tests {
    use koidrive_cloudflare::{Cloudflare, Secret};
    use koidrive_driver::Paths;
    use serde_json::{Value, json};

    use super::*;
    use crate::setup::fake::{Fake, Reply};

    const ACCOUNT: &str = "0123456789abcdef";
    const CREATED: &str = "2026-11-10T01:02:03.456789Z";
    const BUCKET_CREATED: &str = "2026-11-10T01:02:00.000Z";

    struct TestCloud(Cloudflare);

    impl Cloud for TestCloud {
        fn account(&self) -> &str {
            ACCOUNT
        }

        async fn fresh(&mut self) -> Result<Cloudflare, CliError> {
            Ok(self.0.clone())
        }
    }

    fn cf(method: &'static str, path: &str, result: Value) -> Reply {
        cf_status(
            method,
            path,
            200,
            json!({ "success": true, "errors": [], "result": result }),
        )
    }

    fn cf_status(method: &'static str, path: &str, status: u16, body: Value) -> Reply {
        Reply {
            method,
            path: format!("/client/v4/accounts/{ACCOUNT}{path}"),
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
        }
    }

    fn missing(method: &'static str, path: &str) -> Reply {
        cf_status(
            method,
            path,
            404,
            json!({ "success": false, "errors": [{ "code": 10006, "message": "The specified bucket does not exist." }], "result": null }),
        )
    }

    fn page(keys: &[&str]) -> Reply {
        let objects: Vec<Value> = keys.iter().map(|key| json!({ "key": key })).collect();
        let mut body = json!({ "success": true, "errors": [], "result": objects });
        body["result_info"] = json!({ "cursor": "", "is_truncated": false });
        cf_status(
            "GET",
            "/r2/buckets/koidrive/objects?per_page=1000",
            200,
            body,
        )
    }

    fn bucket(date: &str) -> Reply {
        cf(
            "GET",
            "/r2/buckets/koidrive",
            json!({ "name": "koidrive", "creation_date": date, "jurisdiction": "eu" }),
        )
    }

    fn delete_object(key: &str) -> Reply {
        cf(
            "DELETE",
            &format!("/r2/buckets/koidrive/objects/{}", key.replace('/', "%2F")),
            Value::Null,
        )
    }

    async fn start(replies: Vec<Reply>) -> (Fake, TestCloud) {
        let fake = Fake::start(replies).await;
        let api = Cloudflare::new(
            reqwest::Client::new(),
            fake.base.join("client/v4/").unwrap(),
            Secret::new("token".into()),
        );
        (fake, TestCloud(api))
    }

    fn script(created_on: Option<&str>) -> Script {
        serde_json::from_value(
            json!({ "id": "koidrive", "created_on": created_on, "tags": ["koidrive-server"] }),
        )
        .unwrap()
    }

    fn recorded(script: &str) -> SetupState {
        SetupState {
            account: ACCOUNT.into(),
            script: script.into(),
            bucket: "koidrive".into(),
            step: Step::Uninstalling,
            script_created_on: (!script.is_empty()).then(|| CREATED.to_owned()),
            jurisdiction: Jurisdiction::Eu,
            bucket_created_on: Some(BUCKET_CREATED.into()),
            ..SetupState::default()
        }
    }

    fn state_file() -> (tempfile::TempDir, StateFile) {
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        (dir, file)
    }

    fn assert_r2_in(fake: &Fake, jurisdiction: &str) {
        for seen in fake.seen() {
            if seen.path.contains("/r2/") {
                assert_eq!(seen.jurisdiction.as_deref(), Some(jurisdiction), "{seen:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_worker_without_its_creation_time_is_not_planned() {
        let (fake, mut cloud) = start(vec![]).await;
        let error = plan(
            &mut cloud,
            Some(&script(None)),
            "koidrive",
            Jurisdiction::Eu,
        )
        .await
        .unwrap_err()
        .message;
        assert!(
            error.contains("did not say when the Worker koidrive was created"),
            "{error}"
        );
        assert!(error.ends_with("deleted nothing"), "{error}");
        assert!(fake.seen().is_empty());
    }

    #[tokio::test]
    async fn a_bucket_without_its_creation_date_is_not_planned() {
        let (fake, mut cloud) = start(vec![cf(
            "GET",
            "/r2/buckets/koidrive",
            json!({ "name": "koidrive" }),
        )])
        .await;
        let error = plan(
            &mut cloud,
            Some(&script(Some(CREATED))),
            "koidrive",
            Jurisdiction::Eu,
        )
        .await
        .unwrap_err()
        .message;
        assert!(error.ends_with("deleted nothing"), "{error}");
        assert!(!fake.sent(&["DELETE", "PUT", "POST"]));
    }

    #[tokio::test]
    async fn a_bucket_in_another_jurisdiction_is_not_planned() {
        let (fake, mut cloud) = start(vec![cf(
            "GET",
            "/r2/buckets/koidrive",
            json!({ "name": "koidrive", "creation_date": BUCKET_CREATED, "jurisdiction": "us" }),
        )])
        .await;
        let error = plan(
            &mut cloud,
            Some(&script(Some(CREATED))),
            "koidrive",
            Jurisdiction::Eu,
        )
        .await
        .unwrap_err()
        .message;
        assert!(error.ends_with("deleted nothing"), "{error}");
        assert!(!fake.sent(&["DELETE"]));
    }

    #[tokio::test]
    async fn a_plan_records_both_identities_and_counts_the_objects() {
        let (fake, mut cloud) = start(vec![
            bucket(BUCKET_CREATED),
            page(&["spaces/a/meta/1/space.json", "notes.txt"]),
        ])
        .await;
        let (state, contents) = plan(
            &mut cloud,
            Some(&script(Some(CREATED))),
            "koidrive",
            Jurisdiction::Eu,
        )
        .await
        .unwrap();
        assert_eq!(state, recorded("koidrive"));
        assert_eq!(
            contents,
            Contents {
                objects: 2,
                foreign: Some("notes.txt".into())
            }
        );
        assert_r2_in(&fake, "eu");
    }

    #[tokio::test]
    async fn the_recorded_worker_and_bucket_are_deleted() {
        let keys = ["spaces/a/meta/1/space.json", "spaces/a/log/2"];
        let (fake, mut cloud) = start(vec![
            cf(
                "GET",
                "/workers/scripts",
                json!([{ "id": "koidrive", "created_on": CREATED }]),
            ),
            cf(
                "DELETE",
                "/workers/scripts/koidrive?force=true",
                Value::Null,
            ),
            bucket(BUCKET_CREATED),
            cf("GET", "/workers/scripts", json!([])),
            page(&keys),
            page(&keys),
            delete_object(keys[0]),
            delete_object(keys[1]),
            page(&[]),
            cf("DELETE", "/r2/buckets/koidrive", Value::Null),
        ])
        .await;
        let (_dir, file) = state_file();
        file.save(&recorded("koidrive")).unwrap();

        finish(&mut cloud, &file, recorded("koidrive"))
            .await
            .unwrap();

        let deletes: Vec<String> = fake
            .seen()
            .into_iter()
            .filter(|seen| seen.method == "DELETE")
            .map(|seen| seen.path)
            .collect();
        assert_eq!(deletes.len(), 4, "{deletes:?}");
        assert!(deletes.last().unwrap().ends_with("/r2/buckets/koidrive"));
        assert_r2_in(&fake, "eu");
        assert_eq!(file.load().unwrap(), None);
    }

    #[tokio::test]
    async fn a_new_worker_and_a_new_bucket_with_the_recorded_names_are_left_alone() {
        let (fake, mut cloud) = start(vec![
            cf(
                "GET",
                "/workers/scripts",
                json!([{ "id": "koidrive", "created_on": "2026-11-11T00:00:00Z" }]),
            ),
            bucket("2026-11-11T00:00:00Z"),
        ])
        .await;
        let (_dir, file) = state_file();
        file.save(&recorded("koidrive")).unwrap();

        finish(&mut cloud, &file, recorded("koidrive"))
            .await
            .unwrap();

        assert!(!fake.sent(&["DELETE"]));
        assert_eq!(file.load().unwrap(), None);
    }

    #[tokio::test]
    async fn a_worker_without_its_creation_time_stops_the_uninstall() {
        let (fake, mut cloud) = start(vec![cf(
            "GET",
            "/workers/scripts",
            json!([{ "id": "koidrive" }]),
        )])
        .await;
        let (_dir, file) = state_file();
        file.save(&recorded("koidrive")).unwrap();

        let error = finish(&mut cloud, &file, recorded("koidrive"))
            .await
            .unwrap_err()
            .message;

        assert!(error.ends_with("deleted nothing"), "{error}");
        assert!(!fake.sent(&["DELETE"]));
        assert_eq!(file.load().unwrap(), Some(recorded("koidrive")));
    }

    #[tokio::test]
    async fn a_bucket_another_worker_binds_is_kept() {
        let (fake, mut cloud) = start(vec![
            bucket(BUCKET_CREATED),
            cf("GET", "/workers/scripts", json!([{ "id": "photos" }])),
            cf(
                "GET",
                "/workers/scripts/photos/settings",
                json!({ "bindings": [{ "type": "r2_bucket", "name": "STORE", "bucket_name": "koidrive", "jurisdiction": "eu" }] }),
            ),
        ])
        .await;
        let (_dir, file) = state_file();

        finish(&mut cloud, &file, recorded("")).await.unwrap();

        assert!(!fake.sent(&["DELETE"]));
        assert_eq!(file.load().unwrap(), None);
    }

    #[tokio::test]
    async fn a_binding_of_the_same_name_in_another_jurisdiction_does_not_keep_the_bucket() {
        let (fake, mut cloud) = start(vec![
            bucket(BUCKET_CREATED),
            cf("GET", "/workers/scripts", json!([{ "id": "photos" }])),
            cf(
                "GET",
                "/workers/scripts/photos/settings",
                json!({ "bindings": [{ "type": "r2_bucket", "name": "STORE", "bucket_name": "koidrive" }] }),
            ),
            page(&[]),
            page(&[]),
            cf("DELETE", "/r2/buckets/koidrive", Value::Null),
        ])
        .await;
        let (_dir, file) = state_file();

        finish(&mut cloud, &file, recorded("")).await.unwrap();

        assert!(fake.sent(&["DELETE"]));
        assert_r2_in(&fake, "eu");
    }

    #[tokio::test]
    async fn a_bucket_with_keys_koidrive_does_not_write_is_kept() {
        let (fake, mut cloud) = start(vec![
            bucket(BUCKET_CREATED),
            cf("GET", "/workers/scripts", json!([])),
            page(&["spaces/a/meta/1/space.json", "backups/2026-11-01.tar"]),
        ])
        .await;
        let (_dir, file) = state_file();

        finish(&mut cloud, &file, recorded("")).await.unwrap();

        assert!(!fake.sent(&["DELETE"]));
        assert_eq!(file.load().unwrap(), None);
    }

    #[tokio::test]
    async fn a_listing_that_repeats_its_cursor_stops_before_any_delete() {
        let listing = |query: &str| {
            let body = json!({
                "success": true,
                "errors": [],
                "result": [{ "key": "spaces/a/meta/1/space.json" }],
                "result_info": { "cursor": "c1", "is_truncated": true },
            });
            cf_status(
                "GET",
                &format!("/r2/buckets/koidrive/objects?per_page=1000{query}"),
                200,
                body,
            )
        };
        let (fake, mut cloud) = start(vec![
            bucket(BUCKET_CREATED),
            cf("GET", "/workers/scripts", json!([])),
            listing(""),
            listing("&cursor=c1"),
        ])
        .await;
        let (_dir, file) = state_file();
        file.save(&recorded("")).unwrap();

        let error = finish(&mut cloud, &file, recorded(""))
            .await
            .unwrap_err()
            .message;

        assert!(error.contains("same cursor"), "{error}");
        assert!(!fake.sent(&["DELETE"]));
        assert_eq!(file.load().unwrap(), Some(recorded("")));
    }

    #[tokio::test]
    async fn a_bucket_that_is_gone_completes_the_uninstall() {
        let (fake, mut cloud) = start(vec![missing("GET", "/r2/buckets/koidrive")]).await;
        let (_dir, file) = state_file();
        file.save(&recorded("")).unwrap();

        finish(&mut cloud, &file, recorded("")).await.unwrap();

        assert!(!fake.sent(&["DELETE"]));
        assert_eq!(file.load().unwrap(), None);
    }

    #[tokio::test]
    async fn an_unreadable_bucket_keeps_the_state_file() {
        let (fake, mut cloud) = start(vec![cf_status(
            "GET",
            "/r2/buckets/koidrive",
            403,
            json!({ "success": false, "errors": [{ "code": 10000, "message": "Authentication error" }], "result": null }),
        )])
        .await;
        let (_dir, file) = state_file();
        file.save(&recorded("")).unwrap();

        finish(&mut cloud, &file, recorded("")).await.unwrap_err();

        assert!(!fake.sent(&["DELETE"]));
        assert_eq!(file.load().unwrap(), Some(recorded("")));
    }
}
