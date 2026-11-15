// SPDX-License-Identifier: Apache-2.0

    use std::time::Duration;

    use koidrive_driver::Paths;
    use koidrive_ipc::{Code, Next};

    use super::deploy::{create_bucket, free_name, keep_script_name, new_bucket_name};
    use super::onboarding::choices;
    use super::servers::newer;
    use super::session::{guarded, read_answer};
    use super::state::{SetupState, StateFile, Step};
    use crate::terminal::read_line;

    use clap::Parser;
    use koidrive_proto::v1::DeviceCredentials;
    use serde_json::json;

    use super::fake::{self, ACCOUNT, Fake, Reply, cf, proto, revoked, tokens};
    use super::*;

    #[tokio::test]
    async fn interrupted_login_preserves_raw_text_and_offers_a_human_next_step() {
        let error = guarded(
            std::future::pending::<Result<Session, CliError>>(),
            async || "stopped",
            async |_| Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(error.message, "stopped before the login finished");
        assert_eq!(
            error.human.as_deref(),
            Some("Stopped before sign-in finished. Run this command again when you are ready.")
        );
    }

    #[test]
    fn a_closed_input_is_not_an_empty_answer() {
        let closed = read_line(&b""[..]).unwrap_err();
        assert_eq!(closed.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(read_line(&b"\n"[..]).unwrap(), "\n");
        assert_eq!(read_line(&b"n\n"[..]).unwrap(), "n\n");
    }

    #[test]
    fn setup_guides_only_on_two_terminals_without_json_or_ci() {
        let guided = |stdin, stderr, json, ci| mode(stdin, stderr, json, ci, false).unwrap();
        assert_eq!(guided(true, true, false, false), Mode::Guided);
        for (stdin, stderr, json, ci) in [
            (false, true, false, false),
            (true, false, false, false),
            (false, false, false, false),
            (true, true, true, false),
            (true, true, false, true),
            (true, true, true, true),
        ] {
            assert_eq!(
                guided(stdin, stderr, json, ci),
                Mode::Direct,
                "stdin {stdin}, stderr {stderr}, json {json}, CI {ci}"
            );
        }
    }

    #[test]
    fn interactive_asks_for_the_guide_and_fails_without_terminals() {
        assert_eq!(mode(true, true, false, true, true).unwrap(), Mode::Guided);
        for (stdin, stderr) in [(false, true), (true, false), (false, false)] {
            let error = mode(stdin, stderr, false, false, true).unwrap_err();
            assert_eq!(error.code, Code::Usage);
            assert!(error.message.contains("needs a terminal"), "{error:?}");
        }
        assert_eq!(
            mode(true, true, true, false, true).unwrap_err().code,
            Code::Usage
        );
    }

    #[test]
    fn setup_lists_every_way_in_with_its_argv() {
        let argv: Vec<String> = choices()
            .iter()
            .map(|choice| choice.argv.join(" "))
            .collect();
        assert_eq!(
            argv,
            [
                "koi join <CODE>",
                "koi server deploy --interactive",
                "koi server add-device --interactive",
                "koi server claim <SERVER_URL> <CODE> --user <NAME>",
                "koi setup --interactive",
            ]
        );
    }

    #[tokio::test]
    async fn json_and_a_pasted_login_stop_before_the_login_and_name_the_interactive_command() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let parse = |args: &[&str]| match crate::Cli::try_parse_from(args).unwrap().command {
            Some(crate::Command::Server { action }) => action,
            _ => unreachable!(),
        };

        let error = server(
            &paths,
            parse(&["koi", "server", "deploy"]),
            true,
            false,
            Arc::new(Terminal::new(true, false)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, Code::AuthRequired);
        assert_eq!(
            error.next,
            [Next::of(&["koi", "server", "deploy", "--interactive"])]
        );

        let action = parse(&["koi", "server", "upgrade", "--worker", "fb", "--no-browser"]);
        let error = server(
            &paths,
            action,
            false,
            false,
            Arc::new(Terminal::new(true, false)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, Code::AuthRequired);
        assert_eq!(
            error.next,
            [Next::of(&[
                "koi",
                "server",
                "upgrade",
                "--worker",
                "fb",
                "--interactive",
                "--no-browser"
            ])]
        );
    }

    #[tokio::test]
    async fn owner_commands_without_interactive_and_terminals_need_auth_and_name_the_interactive_command()
     {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let parse = |args: &[&str]| match crate::Cli::try_parse_from(args).unwrap().command {
            Some(crate::Command::Server { action }) => action,
            _ => unreachable!(),
        };
        for (args, next) in [
            (&["deploy"][..], &["deploy", "--interactive"][..]),
            (&["add-device"], &["add-device", "--interactive"]),
            (
                &["upgrade", "--worker", "fb", "--yes"],
                &["upgrade", "--worker", "fb", "--interactive"],
            ),
            (&["upgrade"], &["upgrade", "--interactive"]),
            (
                &["delete", "--bucket", "fb-1a2b", "--yes"],
                &["delete", "--bucket", "fb-1a2b", "--interactive"],
            ),
            (&["deploy", "--interactive"], &["deploy", "--interactive"]),
        ] {
            let argv = [&["koi", "server"][..], args].concat();
            let error = server(
                &paths,
                parse(&argv),
                false,
                false,
                Arc::new(Terminal::new(true, false)),
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, Code::AuthRequired, "{args:?}");
            assert_eq!(
                error.next,
                [Next::of(&[&["koi", "server"][..], next].concat())],
                "{args:?}"
            );
        }
        assert!(!dir.path().join("state").exists(), "nothing was set up");
    }

    fn deployed(fake: &Fake) -> Found {
        Found {
            script: Script {
                id: "koidrive".into(),
                created_on: None,
                tags: vec![TAG.into()],
                migration_tag: None,
                has_assets: true,
            },
            bucket: Some(Bound {
                name: "koidrive".into(),
                jurisdiction: Jurisdiction::Default,
            }),
            url: fake.base.clone(),
            info: None,
        }
    }

    #[test]
    fn a_stop_ends_a_prompt_nobody_answers_and_the_runtime_does_not_wait_for_it() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(guarded(
            async {
                read_answer(|| {
                    loop {
                        std::thread::park();
                    }
                })
                .await?;
                unreachable!("nobody answers the prompt")
            },
            async || "stopped",
            async |_: &mut Session| Ok(()),
        ));
        assert_eq!(
            outcome.unwrap_err().message,
            "stopped before the login finished"
        );
        let (dropped, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(runtime);
            dropped.send(()).unwrap();
        });
        done.recv_timeout(Duration::from_secs(5))
            .expect("the runtime stops without waiting for the blocked read");
    }

    #[tokio::test]
    async fn an_answer_is_read_without_its_line_end() {
        let answer = read_answer(|| Ok(" y\n".to_owned())).await;
        assert_eq!(answer.unwrap(), "y");
        let failed = read_answer(|| Err(std::io::Error::other("closed"))).await;
        assert_eq!(failed.unwrap_err().message, "closed");
    }

    #[tokio::test]
    async fn a_stop_during_the_account_lookup_still_revokes_the_grant() {
        let oauth = Fake::start(vec![tokens(), revoked(), revoked()]).await;
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base =
            Url::parse(&format!("http://{}/client/v4/", api.local_addr().unwrap())).unwrap();
        let mut held = Vec::new();
        let stopped = async || {
            held.push(api.accept().await.unwrap().0);
            "stopped"
        };

        let outcome = guarded(
            async { Ok(fake::login(&oauth, api_base).await) },
            stopped,
            async |_: &mut Session| Ok(()),
        )
        .await;

        assert_eq!(
            outcome.unwrap_err().message,
            "stopped; run the same command again to continue"
        );
        let revocations = oauth
            .seen()
            .iter()
            .filter(|seen| seen.path == "/oauth2/revoke")
            .count();
        assert_eq!(revocations, 2);
    }

    #[tokio::test]
    async fn a_resumed_deploy_moves_off_a_worker_name_another_application_took() {
        let binds = |bucket: &str| json!({ "bindings": [{ "type": "r2_bucket", "name": "BUCKET", "bucket_name": bucket }] });
        let cases = [
            (
                json!([{ "id": "koidrive", "tags": [] }]),
                None,
                "koidrive-2",
            ),
            (
                json!([{ "id": "koidrive", "tags": [TAG] }, { "id": "koidrive-2", "tags": [] }]),
                Some(binds("photos")),
                "koidrive-3",
            ),
            (
                json!([{ "id": "koidrive", "tags": [TAG] }]),
                Some(binds("koidrive")),
                "koidrive",
            ),
            (json!([{ "id": "photos", "tags": [] }]), None, "koidrive"),
        ];
        for (scripts, settings, expected) in cases {
            let mut replies = vec![tokens(), cf("GET", "/workers/scripts", scripts)];
            replies.extend(
                settings.map(|settings| cf("GET", "/workers/scripts/koidrive/settings", settings)),
            );
            let fake = Fake::start(replies).await;
            let mut session = fake::session(&fake).await;
            let mut state = SetupState {
                account: ACCOUNT.into(),
                script: "koidrive".into(),
                bucket: "koidrive".into(),
                step: Step::BucketCreated,
                ..SetupState::default()
            };

            keep_script_name(&mut session, &mut state, "BUCKET", "koidrive")
                .await
                .unwrap();

            assert_eq!(state.script, expected);
        }
    }

    #[test]
    fn a_deploy_names_its_bucket_with_a_random_suffix() {
        let first = new_bucket_name("koidrive");
        let suffix = first.strip_prefix("koidrive-").unwrap();
        assert_eq!(suffix.len(), 16);
        assert!(
            suffix
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(first, new_bucket_name("koidrive"));
    }

    /// The bucket names of the creates `fake` received.
    fn created(fake: &Fake) -> Vec<String> {
        fake.seen()
            .iter()
            .filter(|seen| seen.method == "POST" && seen.path.ends_with("/r2/buckets"))
            .map(|seen| {
                let body: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
                body["name"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    fn create_answer(status: u16) -> Reply {
        Reply {
            method: "POST",
            path: format!("/client/v4/accounts/{ACCOUNT}/r2/buckets"),
            status,
            content_type: "application/json",
            body: json!({ "success": status == 200, "errors": [], "result": {} })
                .to_string()
                .into_bytes(),
        }
    }

    #[tokio::test]
    async fn a_lost_create_answer_is_adopted_under_the_saved_name_on_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        let name = new_bucket_name("koidrive");
        let planned = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: name.clone(),
            ..SetupState::default()
        };
        file.save(&planned).unwrap();

        let lost = Fake::start(vec![tokens(), create_answer(502)]).await;
        let mut state = file.load().unwrap().unwrap();
        let error = create_bucket(&mut fake::session(&lost).await, &file, &mut state)
            .await
            .unwrap_err()
            .message;
        assert!(error.contains("cannot create the bucket"), "{error}");
        assert_eq!(file.load().unwrap(), Some(planned));

        let retry = Fake::start(vec![tokens(), create_answer(409)]).await;
        let mut state = file.load().unwrap().unwrap();
        create_bucket(&mut fake::session(&retry).await, &file, &mut state)
            .await
            .unwrap();

        assert_eq!(created(&lost), [name.as_str()]);
        assert_eq!(created(&retry), [name.as_str()]);
        let saved = file.load().unwrap().unwrap();
        assert_eq!((saved.bucket, saved.step), (name, Step::BucketCreated));
    }

    #[tokio::test]
    async fn a_bucket_named_like_the_default_is_never_created_read_or_deleted() {
        let fake = Fake::start(vec![tokens(), create_answer(200)]).await;
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        let mut state = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: new_bucket_name("koidrive"),
            ..SetupState::default()
        };

        create_bucket(&mut fake::session(&fake).await, &file, &mut state)
            .await
            .unwrap();

        assert_eq!(created(&fake), [state.bucket.as_str()]);
        assert_ne!(state.bucket, "koidrive");
        assert!(
            !fake
                .seen()
                .iter()
                .any(|seen| seen.path.contains("/r2/buckets/koidrive") || seen.method == "DELETE")
        );
    }

    #[tokio::test]
    async fn setup_finishes_a_deploy_that_stopped_before_the_server_answered() {
        let info = ServerInfo {
            server_version: "0.1.0".into(),
            setup_attempts: true,
            ..ServerInfo::default()
        };
        let issued = DeviceCredentials {
            user_id: "user".into(),
            device_id: "0194f8a2-0000-7000-8000-000000000002".into(),
            refresh_credential: "device".into(),
            ..DeviceCredentials::default()
        };
        let secrets = "/workers/scripts/koidrive/secrets";
        let fake = Fake::start(vec![
            tokens(),
            cf("POST", "/workers/scripts/koidrive/subdomain", json!(null)),
            proto("GET", "/v1/server", info),
            cf("GET", secrets, json!([])),
            cf("PUT", secrets, json!(null)),
            proto("POST", "/v1/claim", issued),
            cf(
                "DELETE",
                &format!("{secrets}/KOIDRIVE_RECOVERY_KEY"),
                json!(null),
            ),
            cf("GET", secrets, json!([])),
        ])
        .await;
        let mut session = fake::session(&fake).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());

        session
            .add_device(&paths, deployed(&fake), "laptop", true)
            .await
            .unwrap();

        assert!(fake.seen().iter().any(|seen| seen.method == "POST"
            && seen.path.ends_with("/workers/scripts/koidrive/subdomain")));
        assert!(paths.credential().exists());
        assert_eq!(StateFile::new(&paths).load().unwrap(), None);
    }

    #[tokio::test]
    async fn uninstall_stops_at_another_unfinished_run_before_calling_the_account() {
        let deploy = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        let uninstall = SetupState {
            step: Step::Uninstalling,
            ..deploy.clone()
        };
        let elsewhere = SetupState {
            account: "another-account".into(),
            ..uninstall.clone()
        };
        let bucket = new_bucket_name("koidrive");
        for (other, named) in [
            (deploy, None),
            (elsewhere, None),
            (uninstall, Some(bucket.as_str())),
        ] {
            let fake = Fake::start(vec![tokens()]).await;
            let mut session = fake::session(&fake).await;
            let before = fake.seen().len();
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            StateFile::new(&paths).save(&other).unwrap();

            let error = session
                .uninstall(&paths, None, named, Jurisdiction::Default, true)
                .await
                .unwrap_err()
                .message;

            assert!(error.contains("has not finished"), "{error}");
            assert_eq!(StateFile::new(&paths).load().unwrap(), Some(other));
            assert_eq!(fake.seen().len(), before, "{error}");
        }
    }

    #[tokio::test]
    async fn setup_and_deploy_stop_at_an_unfinished_run_before_calling_the_account() {
        let others = [
            SetupState {
                account: "another-account".into(),
                script: "koidrive".into(),
                bucket: new_bucket_name("koidrive"),
                step: Step::BucketCreated,
                ..SetupState::default()
            },
            SetupState {
                account: ACCOUNT.into(),
                script: "koidrive".into(),
                bucket: new_bucket_name("koidrive"),
                step: Step::Uninstalling,
                ..SetupState::default()
            },
        ];
        for other in others {
            for entry in ["setup", "server deploy", "server add-device"] {
                let fake = Fake::start(vec![tokens()]).await;
                let mut session = fake::session(&fake).await;
                let before = fake.seen().len();
                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::under(dir.path());
                StateFile::new(&paths).save(&other).unwrap();

                let error = match entry {
                    "setup" => session.setup(&paths, "desk").await,
                    "server deploy" => session.server_deploy(&paths, "desk").await,
                    _ => session.server_add_device(&paths, None, "desk").await,
                }
                .unwrap_err()
                .message;

                assert!(error.contains("has not finished"), "{entry}: {error}");
                assert_eq!(StateFile::new(&paths).load().unwrap(), Some(other.clone()));
                assert_eq!(fake.seen().len(), before, "{entry} called the account");
            }
        }
    }

    #[tokio::test]
    async fn setup_and_deploy_name_the_remaining_bucket_when_uninstall_has_no_worker() {
        for account in [ACCOUNT, "another-account"] {
            for entry in ["setup", "server deploy", "server add-device"] {
                let fake = Fake::start(vec![tokens()]).await;
                let mut session = fake::session(&fake).await;
                let before = fake.seen().len();
                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::under(dir.path());
                let state = SetupState {
                    account: account.into(),
                    bucket: "remaining-bucket".into(),
                    step: Step::Uninstalling,
                    ..SetupState::default()
                };
                StateFile::new(&paths).save(&state).unwrap();

                let error = match entry {
                    "setup" => session.setup(&paths, "desk").await,
                    "server deploy" => session.server_deploy(&paths, "desk").await,
                    _ => session.server_add_device(&paths, None, "desk").await,
                }
                .unwrap_err();

                for text in [&error.message, error.human.as_ref().unwrap()] {
                    assert!(
                        text.contains("the bucket remaining-bucket"),
                        "{entry}: {text}"
                    );
                    assert!(!text.contains("of  "), "{entry}: {text}");
                }
                assert_eq!(StateFile::new(&paths).load().unwrap(), Some(state));
                assert_eq!(fake.seen().len(), before, "{entry} called the account");
            }
        }
    }

    #[tokio::test]
    async fn adding_a_device_keeps_the_unfinished_deploy_of_another_worker() {
        let fake = Fake::start(vec![tokens()]).await;
        let mut session = fake::session(&fake).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let unfinished = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive-2".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        StateFile::new(&paths).save(&unfinished).unwrap();

        let error = session
            .add_device(&paths, deployed(&fake), "laptop", true)
            .await
            .unwrap_err()
            .message;

        assert!(
            error.contains("the deploy of koidrive-2 has not finished"),
            "{error}"
        );
        assert_eq!(StateFile::new(&paths).load().unwrap(), Some(unfinished));
        assert!(
            !fake
                .seen()
                .iter()
                .any(|seen| seen.path.contains("/accounts/") || seen.path.starts_with("/v1/"))
        );
    }

    #[tokio::test]
    async fn adding_a_device_to_a_named_server_stops_at_another_workers_deploy_before_calling_the_account()
     {
        let fake = Fake::start(vec![tokens()]).await;
        let mut session = fake::session(&fake).await;
        let before = fake.seen().len();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let unfinished = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive-2".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        StateFile::new(&paths).save(&unfinished).unwrap();

        let error = session
            .server_add_device(&paths, Some("koidrive"), "laptop")
            .await
            .unwrap_err()
            .message;

        assert!(
            error.contains("the deploy of koidrive-2 has not finished"),
            "{error}"
        );
        assert_eq!(StateFile::new(&paths).load().unwrap(), Some(unfinished));
        assert_eq!(fake.seen().len(), before);
    }

    #[test]
    fn free_names_count_up_from_the_default() {
        let taken = vec!["koidrive".to_owned(), "koidrive-2".to_owned()];
        assert_eq!(free_name("koidrive", &[]), "koidrive");
        assert_eq!(free_name("koidrive", &taken), "koidrive-3");
    }

    #[test]
    fn the_embedded_bundle_reads_back_when_the_build_has_one() {
        match option_env!("KOIDRIVE_SERVER_BUNDLE") {
            None => assert!(
                embedded_bundle()
                    .unwrap_err()
                    .message
                    .contains("built without a server bundle")
            ),
            Some(_) => {
                let bundle = embedded_bundle().unwrap();
                assert!(!bundle.module.is_empty() && !bundle.assets.is_empty());
            }
        }
    }

    #[test]
    fn newer_compares_dotted_numbers_only() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.3.0", "0.3.0"));
        assert!(!newer("0.2.9", "0.3.0"));
        assert!(newer("0.4.0-rc1", "0.3.0"));
    }

    #[test]
    fn newer_obeys_semver_prerelease_precedence() {
        for (server, cli) in [
            ("0.1.0-alpha.2", "0.1.0-alpha.1"),
            ("0.2.0-alpha.1", "0.1.0-alpha.1"),
            ("0.1.0", "0.1.0-alpha.1"),
            ("0.1.0-alpha.10", "0.1.0-alpha.2"),
            ("0.1.0-rc.1", "0.1.0-alpha.10"),
        ] {
            assert!(newer(server, cli), "{server} must be newer than {cli}");
            assert!(!newer(cli, server), "{cli} must not be newer than {server}");
        }
        assert!(!newer("0.1.0+build.2", "0.1.0+build.1"));
        assert!(!newer("not-a-version", "0.1.0"));
        assert!(!newer("0.1.0", "not-a-version"));
    }
