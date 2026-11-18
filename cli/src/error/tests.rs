// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::path::PathBuf;

fn refused(status: u16, code: ServerCode) -> ApiError {
    refused_with(status, code, "refused")
}

fn refused_with(status: u16, code: ServerCode, message: &str) -> ApiError {
    ApiError::Server {
        status,
        error: ServerError(koidrive_proto::v1::Error {
            code: code as i32,
            message: message.into(),
            ..Default::default()
        }),
    }
}

#[test]
fn human_text_is_optional_and_context_keeps_it_separate() {
    let raw = "unknown cause: /private/state\n{debug: true}";
    let plain = CliError::from(raw);
    assert_eq!(plain.message.as_bytes(), raw.as_bytes());
    assert!(plain.human.is_none());
    let wrapped = CliError::caused(
        "the original contextual message",
        plain.human("The selected folder cannot be read."),
    )
    .context("raw context with internal-id")
    .because(Reason::NotInLinkedFolder)
    .then(&["koi", "status"]);
    assert_eq!(wrapped.code, Code::Failed);
    assert_eq!(wrapped.reason, Some(Reason::NotInLinkedFolder));
    assert_eq!(wrapped.next, [Next::of(&["koi", "status"])]);
    assert_eq!(
        wrapped.message,
        "raw context with internal-id: the original contextual message"
    );
    assert_eq!(
        wrapped.human.as_deref(),
        Some("The selected folder cannot be read.")
    );
    let raw = CliError::caused("outer", CliError::from("inner")).context("context");
    assert_eq!(raw.message, "context: outer");
    assert!(raw.human.is_none());
}

#[test]
fn human_mapping_keeps_the_original_json_bytes() {
    let stale = CliError::from(DriverError::QuestionNotFound("q1".into()));
    assert!(stale.human.as_deref().unwrap().contains("koi resolve"));
    assert!(!stale.human.as_deref().unwrap().contains("q1"));
    let json =
        serde_json::to_vec(&crate::report::Envelope::failed("resolve", stale, None)).unwrap();
    assert_eq!(
        json,
        br#"{"schema_version":1,"command":"resolve","outcome":"error","data":null,"error":{"code":"stale_decision","message":"no unanswered question has the id \"q1\"; koi resolve lists the decisions waiting now","retryable":false},"decisions":[],"next_commands":[{"argv":["koi","resolve","--json"]}]}"#
    );
}

#[test]
fn typed_server_causes_survive_without_protocol_tags() {
    let cause = "this deployment already has an owner";
    let api = CliError::from(refused_with(403, ServerCode::Forbidden, cause));
    assert_eq!(api.code, Code::Failed);
    assert_eq!(
        api.message,
        "the server answered HTTP 403: CODE_FORBIDDEN: this deployment already has an owner"
    );
    assert!(api.next.is_empty());
    assert_eq!(
        api.human.as_deref(),
        Some("The server refused the request: this deployment already has an owner")
    );
    let rpc = CliError::from(DriverError::Refused(ServerError(ProtoError {
        code: ServerCode::Forbidden as i32,
        message: cause.into(),
        ..Default::default()
    })));
    assert_eq!(rpc.code, Code::Failed);
    assert_eq!(rpc.message, format!("the server refused: {cause}"));
    assert_eq!(rpc.human, api.human);
    assert!(rpc.next.is_empty());
}

#[test]
fn retry_safe_network_guidance_does_not_require_an_existing_sign_in() {
    for (error, action) in [
        (
            ApiError::Transport(reqwest::Client::new().get("not a url").build().unwrap_err()),
            "Check your connection and the server address, then try again",
        ),
        (
            ApiError::Status { status: 503 },
            "Wait before trying this command again",
        ),
        (
            ApiError::Unreadable { status: 429 },
            "Wait before trying this command again",
        ),
    ] {
        let raw = error.to_string();
        let shown = CliError::api_retry_safe(error);
        assert_eq!(shown.message, raw);
        let human = shown.human.unwrap();
        assert!(human.contains(action), "{human}");
        assert!(!human.contains("inspect"), "{human}");
        assert!(!human.contains("koi device list"), "{human}");
        assert!(!human.contains("koi space list"), "{human}");
    }
}

#[test]
fn known_outer_outcomes_keep_their_own_recovery_guidance() {
    let message =
        "The saved sign-in could not be checked. Nothing was changed; run the command again";
    let cause = CliError::from(ApiError::Status { status: 503 });
    assert!(cause.human.as_ref().unwrap().contains("Before repeating"));
    let shown = CliError::known_outcome(message, cause);
    assert_eq!(shown.message, message);
    assert_eq!(shown.code, Code::Unavailable);
    assert_eq!(shown.human.as_deref(), Some(message));
}

#[test]
fn typed_server_failures_keep_the_retry_safety_of_the_operation() {
    for status in [500, 503, 599, 429] {
        for safe in [false, true] {
            let error = refused(status, ServerCode::Internal);
            let raw = error.to_string();
            let shown = if safe {
                CliError::api_retry_safe(error)
            } else {
                CliError::from(error)
            };
            assert_eq!(shown.message, raw);
            assert_eq!(
                shown.code,
                if matches!(status, 429 | 503) {
                    Code::Unavailable
                } else {
                    Code::Failed
                }
            );
            assert!(shown.next.is_empty());
            let human = shown.human.unwrap();
            assert!(human.contains("refused"), "{human}");
            if status >= 500 {
                assert!(
                    human.starts_with("The server reported a problem:"),
                    "{human}"
                );
            }
            if safe || status == 429 {
                assert!(human.contains("Wait before trying"), "{human}");
                assert!(!human.contains("inspect"), "{human}");
            } else {
                assert!(human.contains("Before repeating a change"), "{human}");
                assert!(human.contains("inspect the affected item"), "{human}");
            }
        }
    }
}

#[test]
fn unreadable_answers_require_inspection_before_repeating_changes() {
    let transport = || reqwest::Client::new().get("not a url").build().unwrap_err();
    for error in [
        ApiError::Transport(transport()),
        ApiError::Status { status: 503 },
        ApiError::Unreadable { status: 502 },
        ApiError::TooLarge { limit: 1 },
        ApiError::Decode("InternalParserType"),
    ] {
        let original = error.to_string();
        let shown = CliError::from(error);
        assert_eq!(shown.message.as_bytes(), original.as_bytes());
        assert!(shown.next.is_empty());
        let human = shown.human.unwrap();
        assert!(human.contains("Before repeating a change"), "{human}");
        assert!(human.contains("inspect the affected item"), "{human}");
        assert!(!human.contains("InternalParserType"), "{human}");
        assert!(!human.contains("Nothing was sent"), "{human}");
    }
    for error in [
        CloudflareError::Transport {
            call: "upload the Worker",
            source: transport(),
        },
        CloudflareError::Unreadable {
            call: "upload the Worker",
            detail: "parser/debug payload",
        },
    ] {
        let original = error.to_string();
        let shown = CliError::from(error);
        assert_eq!(shown.message.as_bytes(), original.as_bytes());
        assert!(shown.next.is_empty());
        let human = shown.human.unwrap();
        assert!(human.contains("Cloudflare dashboard"), "{human}");
        assert!(human.contains("before repeating"), "{human}");
        assert!(!human.contains("parser/debug"), "{human}");
    }
}

#[test]
fn local_typed_errors_hide_internal_paths_ids_and_parser_payloads() {
    let path = || PathBuf::from("/private/koidrive/state/credential.json");
    let cases = [
        (
            DriverError::Busy(path()),
            "Another Koidrive process",
            Code::Failed,
        ),
        (
            DriverError::FolderHoldsProfile(path()),
            "overlaps Koidrive's own local data",
            Code::Usage,
        ),
        (
            DriverError::Config(ConfigError::Invalid {
                path: path(),
                reason: "parser/debug payload".into(),
            }),
            "configuration is not valid",
            Code::Usage,
        ),
        (
            DriverError::Config(ConfigError::Io {
                path: path(),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            }),
            "could not read its configuration",
            Code::Failed,
        ),
        (
            DriverError::Credential(CredentialError::Io {
                path: path(),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            }),
            "could not access this device's saved sign-in",
            Code::Failed,
        ),
        (
            DriverError::Credential(CredentialError::Invalid {
                path: path(),
                reason: "parser/debug payload".into(),
            }),
            "saved sign-in is damaged",
            Code::Failed,
        ),
        (
            DriverError::Credential(CredentialError::Exposed {
                path: path(),
                mode: 0o644,
            }),
            "Other users can read",
            Code::Failed,
        ),
        (
            DriverError::Credential(CredentialError::NotOwned { path: path() }),
            "another system user",
            Code::Failed,
        ),
        (
            DriverError::Credential(CredentialError::Exists { path: path() }),
            "already has a saved sign-in",
            Code::Failed,
        ),
        (
            DriverError::Credential(CredentialError::NotExcludedFromBackups {
                path: path(),
                source: std::io::Error::other("parser/debug payload"),
            }),
            "Time Machine backups",
            Code::Failed,
        ),
        (
            DriverError::AlreadyPaired {
                device_id: "internal-id".into(),
            },
            "already set up",
            Code::Usage,
        ),
        (
            DriverError::CredentialReplaced {
                device_id: "internal-id".into(),
            },
            "saved sign-in changed",
            Code::Failed,
        ),
        (
            DriverError::SpaceAmbiguous {
                name: "Notes".into(),
                ids: vec!["internal-id".into()],
            },
            "koi space list",
            Code::Usage,
        ),
        (
            DriverError::MarkerOfOtherSpace("internal-id".into()),
            "different space",
            Code::Usage,
        ),
        (
            DriverError::FolderOfOtherDevice {
                device_id: "internal-id".into(),
                server: "https://server.example".into(),
            },
            "another device",
            Code::Usage,
        ),
        (
            DriverError::LinkedByOtherDevice {
                device_id: "internal-id".into(),
                server: "https://server.example".into(),
            },
            "different sign-in",
            Code::Usage,
        ),
        (
            DriverError::SpaceNotLinked("internal-id".into()),
            "no linked folder",
            Code::Usage,
        ),
        (
            DriverError::Paths(PathsError::BadSpaceId("internal-id".into())),
            "space ID is not valid",
            Code::Usage,
        ),
        (
            DriverError::Paths(PathsError::NoHome),
            "home folder",
            Code::Failed,
        ),
        (
            DriverError::UnsupportedFileSystem,
            "cannot safely apply",
            Code::Usage,
        ),
        (
            DriverError::Store {
                path: path(),
                error: koidrive_store::Error::InvalidEngineState("parser/debug payload".into()),
            },
            "cannot use its saved sync state",
            Code::Failed,
        ),
    ];
    for (error, expected, code) in cases {
        let original = error.to_string();
        let shown = CliError::from(error);
        assert_eq!(shown.message.as_bytes(), original.as_bytes());
        assert_eq!(shown.code, code);
        assert!(shown.next.is_empty());
        let human = shown.human.unwrap();
        assert!(human.contains(expected), "{human}");
        for hidden in [
            "/private/",
            "internal-id",
            "parser/debug",
            "credential.json",
        ] {
            assert!(!human.contains(hidden), "{human}");
        }
    }
}

#[test]
fn known_filesystem_causes_have_actions_and_unknown_causes_stay_raw() {
    for (kind, cause) in [
        (std::io::ErrorKind::PermissionDenied, "permission"),
        (std::io::ErrorKind::NotFound, "missing"),
        (std::io::ErrorKind::AlreadyExists, "already exists"),
        (std::io::ErrorKind::StorageFull, "drive is full"),
        (std::io::ErrorKind::ReadOnlyFilesystem, "read-only"),
    ] {
        let error = DriverError::Io {
            path: PathBuf::from("/private/state"),
            error: std::io::Error::new(kind, "parser/debug payload"),
        };
        let original = error.to_string();
        let shown = CliError::from(error);
        assert_eq!(shown.message.as_bytes(), original.as_bytes());
        assert_eq!(shown.code, Code::Failed);
        assert!(shown.next.is_empty());
        let human = shown.human.unwrap();
        assert!(human.contains(cause), "{human}");
        assert!(!human.contains("/private/"), "{human}");
        assert!(!human.contains("parser/debug"), "{human}");
    }
    let unknown = CliError::from(DriverError::Io {
        path: PathBuf::from("/x"),
        error: std::io::Error::other("a new filesystem failure"),
    });
    assert!(unknown.human.is_none());
    assert!(unknown.message.contains("a new filesystem failure"));
    assert_eq!(unknown.code, Code::Failed);
}

#[test]
fn sign_out_presentations_keep_last_device_recovery_warnings() {
    for error in [
        DriverError::LastDevice,
        DriverError::NotRevoked("parser/debug payload".into()),
        DriverError::Credential(CredentialError::SignOutPending),
    ] {
        let original = error.to_string();
        let shown = CliError::from(error);
        assert_eq!(shown.message.as_bytes(), original.as_bytes());
        let human = shown.human.unwrap();
        assert!(human.contains("last active device"), "{human}");
        assert!(human.contains("owner recovery"), "{human}");
        assert!(!human.contains("parser/debug"), "{human}");
    }
}

#[test]
fn pairing_and_unlinking_presentations_keep_original_actions() {
    let unpaired = CliError::from(DriverError::NotPaired);
    assert_eq!(unpaired.code, Code::AuthRequired);
    assert_eq!(
        unpaired.message,
        "this device is not set up; run koi setup first"
    );
    assert_eq!(unpaired.next, [Next::of(&["koi", "setup"])]);
    assert!(unpaired.human.unwrap().contains("koi setup"));
    let unlinked = CliError::from(DriverError::NotLinked(PathBuf::from("/Documents")));
    assert_eq!(unlinked.code, Code::Usage);
    assert!(unlinked.next.is_empty());
    assert!(unlinked.human.unwrap().contains("koi folder link"));
    let unfinished = DriverError::UnlinkUnfinished(PathBuf::from("/Documents"));
    let original = unfinished.to_string();
    let unfinished = CliError::from(unfinished);
    assert_eq!(unfinished.code, Code::Usage);
    assert_eq!(unfinished.message.as_bytes(), original.as_bytes());
    assert_eq!(
        unfinished.next,
        [Next::of(&["koi", "folder", "unlink", "/Documents"])]
    );
    assert!(unfinished.human.unwrap().contains("Its files stay"));
    let wrong = CliError::code_refused(DriverError::Api(refused_with(
        403,
        ServerCode::Forbidden,
        WRONG_CODE[0],
    )));
    assert_eq!(wrong.code, Code::WrongCode);
    assert_eq!(wrong.message, "the code is wrong or has expired");
    assert!(wrong.next.is_empty());
    assert!(wrong.human.unwrap().contains("Get a new code"));
    let revoked = CliError::from(refused(401, ServerCode::Unauthenticated));
    assert_eq!(revoked.code, Code::AuthRequired);
    assert!(revoked.human.unwrap().contains(SET_UP_AGAIN));
}

#[test]
fn cloudflare_refusals_keep_the_cause_and_login_permissions_have_actions() {
    let error = CloudflareError::Failed {
        call: "create the bucket",
        status: 403,
        errors: vec![koidrive_cloudflare::CloudflareMessage {
            code: 10042,
            message: "Enable R2 before creating a bucket".into(),
        }],
    };
    let original = error.to_string();
    let shown = CliError::from(error);
    assert_eq!(shown.message.as_bytes(), original.as_bytes());
    assert_eq!(shown.code, Code::AuthRequired);
    assert!(shown.next.is_empty());
    let human = shown.human.unwrap();
    assert!(human.contains("Enable R2 before creating a bucket"));
    assert!(!human.contains("10042"));
    for (error, expected, code) in [
        (OauthError::NotCallback, "local page address", Code::Usage),
        (
            OauthError::StateMismatch,
            "another login attempt",
            Code::Usage,
        ),
        (OauthError::MissingCode, "no login code", Code::Usage),
        (
            OauthError::MissingScopes {
                missing: "internal-scope".into(),
            },
            "allow every permission",
            Code::AuthRequired,
        ),
        (
            OauthError::Denied {
                error: "access_denied".into(),
                description: Some("You declined access".into()),
            },
            "You declined access",
            Code::AuthRequired,
        ),
        (
            OauthError::TokenRefused {
                status: 400,
                error: "invalid_grant".into(),
                description: Some("The login code expired".into()),
            },
            "The login code expired",
            Code::AuthRequired,
        ),
        (
            OauthError::RevokeRefused {
                token: "refresh_token",
                status: 503,
            },
            "My Profile, Access Management, Connected Applications",
            Code::Failed,
        ),
    ] {
        let original = error.to_string();
        let shown = CliError::from(error);
        assert_eq!(shown.message.as_bytes(), original.as_bytes());
        assert_eq!(shown.code, code);
        assert!(shown.next.is_empty());
        let human = shown.human.unwrap();
        assert!(human.contains(expected), "{human}");
        assert!(!human.contains("internal-scope"), "{human}");
        assert!(!human.contains("refresh_token"), "{human}");
    }
}

#[test]
fn a_refused_claim_or_pairing_code_is_the_codes_fault() {
    for message in WRONG_CODE {
        let wrong = CliError::code_refused(DriverError::Api(refused_with(
            403,
            ServerCode::Forbidden,
            message,
        )));
        assert_eq!(wrong.code, Code::WrongCode);
        assert_eq!(wrong.code.exit(), 8);
        assert_eq!(wrong.message, "the code is wrong or has expired");
        assert!(wrong.next.is_empty());
    }
    let owned = CliError::code_refused(DriverError::Api(refused_with(
        403,
        ServerCode::Forbidden,
        "this deployment already has an owner",
    )));
    assert_ne!(owned.code, Code::WrongCode);
    assert_eq!(owned.reason, Some(Reason::Owned));
    assert!(
        owned.message.contains("already has an owner"),
        "{}",
        owned.message
    );
    assert!(
        owned.human.as_deref().unwrap().contains(RECOVERY_STEPS),
        "{:?}",
        owned.human
    );
    let too_many = CliError::code_refused(DriverError::Api(refused_with(
        429,
        ServerCode::RetryLater,
        "too many wrong codes from this address; try again in a few minutes",
    )));
    assert_eq!(
        too_many.human.as_deref(),
        Some(
            "The server refused the request: too many wrong codes from this address; try again in a few minutes. Wait before trying this command again."
        )
    );
    let throttled = CliError::code_refused(DriverError::Api(refused(400, ServerCode::RetryLater)));
    assert_eq!(throttled.code, Code::Unavailable);
    let offline = CliError::code_refused(DriverError::Api(ApiError::Status { status: 503 }));
    assert_eq!(offline.code, Code::Unavailable);
    assert_eq!(
        CliError::code_refused(DriverError::NotPaired).code,
        CliError::from(DriverError::NotPaired).code
    );
}

#[test]
fn server_refusals_are_classified_by_their_cause() {
    let classify = |error: ApiError| CliError::from(error).code;
    assert_eq!(
        classify(refused(401, ServerCode::Unauthenticated)),
        Code::AuthRequired
    );
    assert_eq!(
        classify(refused(429, ServerCode::Unspecified)),
        Code::Unavailable
    );
    assert_eq!(
        classify(ApiError::Status { status: 503 }),
        Code::Unavailable
    );
    assert_eq!(
        classify(refused(400, ServerCode::RetryLater)),
        Code::Unavailable
    );
    assert_eq!(
        classify(refused(403, ServerCode::QuotaExceeded)),
        Code::Failed
    );
    let mut waiting = refused(429, ServerCode::QuotaExceeded);
    if let ApiError::Server { error, .. } = &mut waiting {
        error.0.retry_after_ms = 1_000;
    }
    assert_eq!(classify(waiting), Code::Unavailable);
    assert_eq!(classify(ApiError::TooLarge { limit: 1 }), Code::Unavailable);
    assert_eq!(classify(ApiError::Status { status: 404 }), Code::Failed);
    assert_eq!(classify(ApiError::Decode("Status")), Code::Failed);
}

#[test]
fn an_rpc_refusal_keeps_the_meaning_of_its_code() {
    let rpc = |code: ServerCode, retry_after_ms: u64| {
        CliError::rpc_refusal(ProtoError {
            code: code as i32,
            message: "refused".into(),
            retry_after_ms,
            ..Default::default()
        })
    };
    let revoked = rpc(ServerCode::Unauthenticated, 0);
    assert_eq!(revoked.code, Code::AuthRequired);
    assert!(revoked.message.contains(SET_UP_AGAIN), "{revoked:?}");
    assert_eq!(rpc(ServerCode::RetryLater, 0).code, Code::Unavailable);
    assert_eq!(
        rpc(ServerCode::QuotaExceeded, 1_000).code,
        Code::Unavailable
    );
    assert_eq!(rpc(ServerCode::QuotaExceeded, 0).code, Code::Failed);
    let other = rpc(ServerCode::NotFound, 0);
    assert_eq!(other.code, Code::Failed);
    assert_eq!(other.message, "the server refused: refused");
}

#[test]
fn driver_errors_are_classified_and_name_the_real_next_command() {
    let stale = CliError::from(DriverError::QuestionNotFound("q1".into()));
    assert_eq!(stale.code, Code::StaleDecision);
    assert_eq!(stale.next, [Next::of(&["koi", "resolve", "--json"])]);
    let leave = [Next::of(&["koi", "device", "leave", "--last-device"])];
    let last = CliError::from(DriverError::LastDevice);
    assert_eq!((last.code, &last.next[..]), (Code::Failed, &leave[..]));
    let unconfirmed = CliError::from(DriverError::NotRevoked("no answer".into()));
    assert_eq!(
        (unconfirmed.code, &unconfirmed.next[..]),
        (Code::Unavailable, &leave[..])
    );
    let unpaired = CliError::from(DriverError::NotPaired);
    assert_eq!(unpaired.code, Code::AuthRequired);
    assert_eq!(unpaired.next, [Next::of(&["koi", "setup"])]);
    assert_eq!(
        CliError::from(DriverError::NotLinked(PathBuf::from("/x"))).code,
        Code::Usage
    );
    assert_eq!(
        CliError::from(DriverError::Api(ApiError::Status { status: 503 })).code,
        Code::Unavailable
    );
    let other = CliError::from(DriverError::Io {
        path: PathBuf::from("/x"),
        error: std::io::Error::other("the disk failed"),
    });
    assert_eq!(other.code, Code::Failed);
    assert!(other.next.is_empty());
}

#[test]
fn argument_errors_are_usage_and_a_change_that_waits_for_the_owner_needs_auth() {
    let usage = [
        DriverError::FolderOverlaps(PathBuf::from("/home/ada/Docs")),
        DriverError::FolderHoldsProfile(PathBuf::from("/home/ada/.local/state/koidrive")),
        DriverError::FolderOfOtherDevice {
            device_id: "d2".into(),
            server: "https://files.example.com".into(),
        },
        DriverError::LinkedByOtherDevice {
            device_id: "d1".into(),
            server: "https://files.example.com".into(),
        },
        DriverError::SpaceAlreadyLinked(PathBuf::from("/home/ada/Docs")),
        DriverError::SpaceUnplaced("s1".into()),
        DriverError::MarkerOfOtherSpace("s2".into()),
        DriverError::UnsupportedFileSystem,
        DriverError::AlreadyPaired {
            device_id: "d1".into(),
        },
        DriverError::NothingToLift,
        DriverError::Paths(PathsError::BadSpaceId("../x".into())),
        DriverError::Config(ConfigError::Invalid {
            path: PathBuf::from("/home/ada/.config/koidrive/config.toml"),
            reason: "unknown key".into(),
        }),
    ];
    for error in usage {
        let shown = error.to_string();
        assert_eq!(CliError::from(error).code, Code::Usage, "{shown}");
    }
    assert_eq!(
        CliError::from(DriverError::NeedsConfirmation).code,
        Code::AuthRequired
    );
}

#[test]
fn cloudflare_and_login_errors_are_classified_by_their_cause() {
    let transport = || reqwest::Client::new().get("not a url").build().unwrap_err();
    let failed = |status| CloudflareError::Failed {
        call: "list the account's Workers",
        status,
        errors: Vec::new(),
    };
    let classify = |error: CloudflareError| CliError::from(error).code;
    assert_eq!(
        classify(CloudflareError::Transport {
            call: "list the account's Workers",
            source: transport(),
        }),
        Code::Unavailable
    );
    assert_eq!(classify(failed(503)), Code::Unavailable);
    assert_eq!(classify(failed(429)), Code::Unavailable);
    assert_eq!(classify(failed(403)), Code::AuthRequired);
    assert_eq!(classify(failed(400)), Code::Failed);

    let login = |error: OauthError| CliError::from(error).code;
    assert_eq!(login(OauthError::NotCallback), Code::Usage);
    assert_eq!(login(OauthError::StateMismatch), Code::Usage);
    assert_eq!(
        login(OauthError::Denied {
            error: "access_denied".into(),
            description: None,
        }),
        Code::AuthRequired
    );
    assert_eq!(
        login(OauthError::TokenRefused {
            status: 400,
            error: "invalid_grant".into(),
            description: None,
        }),
        Code::AuthRequired
    );
    assert_eq!(login(OauthError::Transport(transport())), Code::Unavailable);

    let wrapped = CliError::caused(
        "cannot list the account's Workers: no answer; run the command again",
        failed(502),
    );
    assert_eq!(wrapped.code, Code::Unavailable);
    assert!(wrapped.message.ends_with("run the command again"));
    let prefixed = CliError::from(failed(401)).context("cannot read the bucket fb");
    assert_eq!(prefixed.code, Code::AuthRequired);
    assert!(
        prefixed
            .message
            .starts_with("cannot read the bucket fb: could not")
    );
}

#[test]
fn each_share_refusal_names_its_reason() {
    let path = || PathBuf::from("/home/ada/Documents/report.docx");
    for (error, reason) in [
        (
            DriverError::OutsideLinkedFolders(path()),
            Reason::NotInLinkedFolder,
        ),
        (DriverError::PathNotFound(path()), Reason::NotFound),
        (DriverError::NotOnServer(path()), Reason::NotOnServer),
        (DriverError::PasswordLength, Reason::PasswordLength),
        (DriverError::ShareLinkNotFound, Reason::NoSuchLink),
        (DriverError::NotAShareLink, Reason::NoSuchLink),
    ] {
        let message = error.to_string();
        let refused = CliError::from(error);
        assert_eq!(
            (refused.code, refused.reason, refused.message),
            (Code::Usage, Some(reason), message)
        );
    }
    let full = CliError::from(DriverError::ShareLinkLimit);
    assert!(full.human.as_deref().unwrap().contains("koi share list"));
    assert!(full.human.as_deref().unwrap().contains("revoke"));
    assert_eq!(
        (full.code, full.reason),
        (Code::Failed, Some(Reason::ShareLinkLimit))
    );
    let ambiguous = CliError::from(DriverError::ShareLinkAmbiguous("0123456789".into()));
    assert_eq!((ambiguous.code, ambiguous.reason), (Code::Usage, None));
}

#[test]
fn permission_guidance_matches_the_platform() {
    let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    let human = local_io_human(&error).unwrap();
    assert_eq!(human.contains("System Settings"), cfg!(target_os = "macos"));
    assert!(human.contains("file permissions"));
}

#[test]
fn a_pause_that_was_not_saved_names_the_cause_before_asking_to_pause_again() {
    let unsaved = |error| {
        let path = PathBuf::from("/private/state/paused");
        CliError::from(DriverError::PausedUnsaved { path, error })
            .human
            .unwrap()
    };
    let denied = unsaved(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    assert!(denied.starts_with("Sync is paused, but"), "{denied}");
    assert!(denied.contains("file permissions"), "{denied}");
    assert!(
        denied.ends_with("Then run koi pause again to save it."),
        "{denied}"
    );
    let unknown = unsaved(std::io::Error::other("a new failure"));
    assert!(
        unknown.ends_with("crash. Run koi pause again to save it."),
        "{unknown}"
    );
}

#[test]
fn human_store_errors_keep_the_specific_recovery_action() {
    for (error, expected) in [
        (
            koidrive_store::Error::UnknownMigration("future-migration".into()),
            "Update Koidrive",
        ),
        (
            koidrive_store::Error::SchemaTooOld {
                missing: "migration".into(),
            },
            "koi service start",
        ),
        (
            koidrive_store::Error::LogNotApplied("/private/state/wal".into()),
            "koi service start",
        ),
    ] {
        let error = CliError::from(DriverError::Store {
            path: "/private/state/store".into(),
            error,
        });
        assert!(
            error.human.as_deref().unwrap().contains(expected),
            "{error:?}"
        );
    }
}
