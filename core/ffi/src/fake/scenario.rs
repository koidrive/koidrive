// SPDX-License-Identifier: Apache-2.0

use super::*;

pub(super) struct Script {
    pub saved: Saved,
    pub activity: FakeActivity,
    pub progress: Option<SyncProgress>,
    pub recent: Vec<RecentChange>,
}

pub(super) const DOCUMENTS: &str = "space-documents";
pub(super) const PHOTOS: &str = "space-photos";
const DAY_MS: i64 = 86_400_000;

fn file(id: String, path: String, detail: ItemDetail) -> HeldItem {
    HeldItem {
        node_id: id,
        path,
        kind: NodeKind::File,
        files: 1,
        detail,
    }
}

fn folder(id: &str, path: &str, files: u64) -> HeldItem {
    HeldItem {
        node_id: id.to_owned(),
        path: path.to_owned(),
        kind: NodeKind::Folder,
        files,
        detail: ItemDetail::Plain,
    }
}

fn question(saved: &mut Saved, space_id: &str, at: i64, kind: QuestionKind, items: Vec<HeldItem>) {
    saved.next_id += 1;
    let answers = kind.answers();
    saved.questions.push(Question {
        id: format!("question-{}", saved.next_id),
        space_id: space_id.to_owned(),
        asked_at_ms: at,
        kind,
        items,
        answers,
    });
}

fn problem(
    saved: &mut Saved,
    space_id: Option<&str>,
    path: Option<&str>,
    kind: ProblemKind,
    at: i64,
) {
    saved.next_id += 1;
    saved.problems.push(Problem {
        id: format!("problem-{}", saved.next_id),
        space_id: space_id.map(str::to_owned),
        path: path.map(str::to_owned),
        kind,
        since_ms: at,
    });
}

fn signed_in() -> Saved {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/me".to_owned());
    let url = "https://box.example.com".to_owned();
    let linked = |space_id: &str, name: &str| LinkedFolder {
        space_id: space_id.to_owned(),
        space_name: name.to_owned(),
        path: format!("{home}/Koidrive/{name}"),
        state: FolderState::UpToDate,
    };
    Saved {
        server: Some(ServerInfo {
            url: url.clone(),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
            claimed: true,
        }),
        claimed: true,
        account: Some(Account {
            server_url: url,
            user_id: "user-1".to_owned(),
            user_name: "Ada".to_owned(),
            device_id: "device-1".to_owned(),
            device_name: "Ada's Mac mini".to_owned(),
        }),
        devices: vec![
            Device {
                device_id: "device-1".to_owned(),
                name: "Ada's Mac mini".to_owned(),
                platform: "macos".to_owned(),
                this_device: true,
                added_at_ms: now_ms() - 30 * DAY_MS,
            },
            Device {
                device_id: "device-2".to_owned(),
                name: "Ada's MacBook Air".to_owned(),
                platform: "macos".to_owned(),
                this_device: false,
                added_at_ms: now_ms() - 7 * DAY_MS,
            },
        ],
        revoked: Vec::new(),
        spaces: vec![
            Space {
                space_id: DOCUMENTS.to_owned(),
                name: "Documents".to_owned(),
            },
            Space {
                space_id: PHOTOS.to_owned(),
                name: "Photos".to_owned(),
            },
            Space {
                space_id: "space-archive".to_owned(),
                name: "Archive".to_owned(),
            },
        ],
        folders: vec![linked(DOCUMENTS, "Documents"), linked(PHOTOS, "Photos")],
        questions: Vec::new(),
        problems: Vec::new(),
        notices: Vec::new(),
        paused: false,
        next_id: 100,
        history: BTreeMap::new(),
        share_links: Vec::new(),
    }
}

fn recent(now: i64) -> Vec<RecentChange> {
    let change = |space: &str, path: &str, kind, change, direction, minutes: i64| RecentChange {
        space_id: space.to_owned(),
        path: path.to_owned(),
        kind,
        change,
        direction,
        at_ms: now - minutes * 60_000,
    };
    vec![
        change(
            DOCUMENTS,
            "Reports/q3.pdf",
            NodeKind::File,
            ChangeKind::Edited,
            Direction::Uploaded,
            1,
        ),
        change(
            PHOTOS,
            "2026/Seoul/IMG_0412.heic",
            NodeKind::File,
            ChangeKind::Added,
            Direction::Downloaded,
            4,
        ),
        change(
            DOCUMENTS,
            "Notes/plan.md",
            NodeKind::File,
            ChangeKind::Moved {
                from: "plan.md".to_owned(),
            },
            Direction::Uploaded,
            9,
        ),
        change(
            DOCUMENTS,
            "Drafts",
            NodeKind::Folder,
            ChangeKind::Deleted,
            Direction::Downloaded,
            15,
        ),
        change(
            DOCUMENTS,
            "Budget (conflicted copy from Build server).xlsx",
            NodeKind::File,
            ChangeKind::ConflictCopy {
                original: "Budget.xlsx".to_owned(),
            },
            Direction::Downloaded,
            31,
        ),
    ]
}

fn mass_delete(saved: &mut Saved, now: i64) {
    let mut items = vec![
        folder("node-md-1", "Archive/2019", 812),
        folder("node-md-2", "Archive/2020", 1_090),
        folder("node-md-3", "Clients/Old", 2_640),
    ];
    items.extend((1..=11).map(|i| {
        file(
            format!("node-md-f{i}"),
            format!("Scans/scan-{i:03}.pdf"),
            ItemDetail::Plain,
        )
    }));
    question(
        saved,
        DOCUMENTS,
        now - 120_000,
        QuestionKind::MassDelete {
            deletions: 14,
            affected_files: 4_553,
            space_files: 6_000,
            exceeded: vec![MassDeleteLimit::ShareOfSpace { percent: 50 }],
        },
        items,
    );
}

fn notice(saved: &mut Saved, space_id: &str, at: i64, kind: NoticeKind) {
    saved.next_id += 1;
    saved.notices.push(Notice {
        id: format!("notice-{}", saved.next_id),
        space_id: space_id.to_owned(),
        at_ms: at,
        kind,
    });
}

fn rewound(now: i64) -> RecoveryCause {
    RecoveryCause {
        trigger: RecoveryTrigger::SpaceRewound,
        restore_point_ms: Some(now - 3 * DAY_MS),
    }
}

fn recovery(saved: &mut Saved, now: i64, per_group: u64) {
    let cause = rewound(now);
    notice(
        saved,
        PHOTOS,
        now - 90_000,
        NoticeKind::Recovery {
            cause: cause.clone(),
            held_files: 5 * per_group,
        },
    );
    let groups = [
        RecoveryGroup::CreatedAfterRestorePoint,
        RecoveryGroup::ChangedOnThisDevice,
        RecoveryGroup::CreatedOnThisDevice,
        RecoveryGroup::DeletedOnThisDevice,
        RecoveryGroup::RequestedBeforeReset,
    ];
    for (g, group) in groups.into_iter().enumerate() {
        let items = (1..=per_group)
            .map(|i| {
                let path = format!("2026/Trip/IMG_{:04}.heic", g as u64 * 1_000 + i);
                let detail = match (group, i % 3) {
                    (RecoveryGroup::ChangedOnThisDevice, 0) => ItemDetail::Moved {
                        from: format!("Inbox/IMG_{:04}.heic", g as u64 * 1_000 + i),
                    },
                    (RecoveryGroup::ChangedOnThisDevice, 1) => ItemDetail::Edited,
                    (RecoveryGroup::ChangedOnThisDevice, _) => ItemDetail::EditedAndMoved {
                        from: format!("Inbox/IMG_{:04}.heic", g as u64 * 1_000 + i),
                    },
                    (RecoveryGroup::RequestedBeforeReset, 0) => ItemDetail::RestoreVersion {
                        version_at_ms: now - 5 * DAY_MS,
                    },
                    (RecoveryGroup::RequestedBeforeReset, _) => ItemDetail::RestoreFromTrash,
                    _ => ItemDetail::Plain,
                };
                file(format!("node-rc-{g}-{i}"), path, detail)
            })
            .collect();
        question(
            saved,
            PHOTOS,
            now - 60_000,
            QuestionKind::Recovery {
                cause: cause.clone(),
                group,
            },
            items,
        );
    }
    for folder in &mut saved.folders {
        if folder.space_id == PHOTOS {
            folder.state = FolderState::Recovering;
        }
    }
}

fn disappearance(saved: &mut Saved, now: i64, n: usize, files: u64) {
    let items = (1..=files)
        .map(|i| {
            file(
                format!("node-ad-{n}-{i}"),
                format!("Projects/p{n}/file-{i}.txt"),
                ItemDetail::Plain,
            )
        })
        .collect();
    question(
        saved,
        DOCUMENTS,
        now - 30_000,
        QuestionKind::AmbiguousDisappearance,
        items,
    );
}

fn history(saved: &mut Saved, now: i64) {
    let deleted = |id: &str, path: &str, kind, files, bytes, modified_at_ms| DeletedItem {
        node_id: id.to_owned(),
        path: path.to_owned(),
        kind,
        files,
        bytes,
        modified_at_ms,
        deleted_at_ms: modified_at_ms.map(|saved: i64| saved + 3_600_000),
        deleted_by_device_id: Some("device-2".to_owned()),
        deleted_by_device_name: Some("Ada's MacBook Air".to_owned()),
    };
    let version = |id: &str, size, days: i64, device: &str, name: &str, current| FileVersion {
        version_id: id.to_owned(),
        size,
        saved_at_ms: now - days * DAY_MS - 3_600_000,
        device_id: device.to_owned(),
        device_name: Some(name.to_owned()),
        current,
    };
    let documents = History {
        trash: vec![
            deleted(
                "node-letter",
                "Drafts/Letter to the landlord.docx",
                NodeKind::File,
                1,
                22_016,
                Some(now - 5 * 3_600_000),
            ),
            deleted(
                "node-old-slides",
                "Old Slides",
                NodeKind::Folder,
                12,
                84_200_000,
                None,
            ),
            deleted(
                "node-budget",
                "Q3 Budget.xlsx",
                NodeKind::File,
                1,
                48_213,
                Some(now - 2 * DAY_MS),
            ),
        ],
        versions: vec![FileVersions {
            path: "Reports/Annual Report.docx".to_owned(),
            node_id: "node-annual-report".to_owned(),
            versions: vec![
                version("version-3", 310_400, 0, "device-1", "Ada's Mac mini", true),
                version(
                    "version-2",
                    298_112,
                    1,
                    "device-2",
                    "Ada's MacBook Air",
                    false,
                ),
                version("version-1", 201_730, 3, "device-1", "Ada's Mac mini", false),
            ],
        }],
        requests: Vec::new(),
    };
    saved.history.insert(DOCUMENTS.to_owned(), documents);
}

fn share_links(saved: &mut Saved, now: i64) {
    let folder = |space_id: &str| {
        (saved.folders.iter())
            .find(|folder| folder.space_id == space_id)
            .map(|folder| folder.path.clone())
    };
    let link = |id: &str, folder: Option<String>, target: Option<(&str, NodeKind)>| ShareLink {
        id: id.to_owned(),
        folder,
        path: target.map(|(path, _)| path.to_owned()),
        kind: target.map(|(_, kind)| kind),
        created_at_ms: now,
        expires_at_ms: None,
        has_password: false,
        state: ShareState::Active,
    };
    let report = ("Reports/Annual Report.docx", NodeKind::File);
    saved.share_links = vec![
        ShareLink {
            created_at_ms: now - 2 * 3_600_000,
            expires_at_ms: Some(now + 7 * DAY_MS - 2 * 3_600_000),
            has_password: true,
            ..link("link-report", folder(DOCUMENTS), Some(report))
        },
        ShareLink {
            created_at_ms: now - 3 * DAY_MS,
            expires_at_ms: Some(now + 27 * DAY_MS),
            ..link(
                "link-seoul",
                folder(PHOTOS),
                Some(("2026/Seoul", NodeKind::Folder)),
            )
        },
        ShareLink {
            created_at_ms: now - 5 * DAY_MS,
            expires_at_ms: Some(now + 2 * DAY_MS),
            ..link("link-gone", folder(DOCUMENTS), None)
        },
        ShareLink {
            created_at_ms: now - 10 * DAY_MS,
            expires_at_ms: Some(now - 3 * DAY_MS),
            state: ShareState::Expired,
            ..link(
                "link-minutes",
                folder(DOCUMENTS),
                Some(("Reports/Board Minutes.docx", NodeKind::File)),
            )
        },
        ShareLink {
            created_at_ms: now - 20 * DAY_MS,
            expires_at_ms: Some(now + 10 * DAY_MS),
            state: ShareState::Revoked,
            ..link("link-report-old", folder(DOCUMENTS), Some(report))
        },
    ];
}

pub(super) fn build(scenario: FakeScenario, now: i64) -> Script {
    let mut script = Script {
        saved: signed_in(),
        activity: FakeActivity::Idle,
        progress: None,
        recent: recent(now),
    };
    let saved = &mut script.saved;
    match scenario {
        FakeScenario::FirstRun => {
            script.saved = Saved::default();
            script.recent.clear();
        }
        FakeScenario::UpToDate => {
            history(saved, now);
            share_links(saved, now);
        }
        FakeScenario::Syncing => {
            script.activity = FakeActivity::Syncing;
            script.progress = Some(SyncProgress {
                files_to_upload: 3,
                files_to_download: 12,
                bytes_done: 48_000_000,
                bytes_total: 210_000_000,
                current: Some(FolderPath {
                    space_id: PHOTOS.to_owned(),
                    path: "2026/Seoul/IMG_0413.heic".to_owned(),
                }),
            });
            for folder in &mut saved.folders {
                folder.state = FolderState::Syncing;
            }
        }
        FakeScenario::Paused => saved.paused = true,
        FakeScenario::Offline => script.activity = FakeActivity::Offline,
        FakeScenario::MassDelete => mass_delete(saved, now),
        FakeScenario::AmbiguousDisappearance => disappearance(saved, now, 1, 4),
        FakeScenario::Recovery => recovery(saved, now, 6),
        FakeScenario::RecoveredWithoutQuestions => notice(
            saved,
            PHOTOS,
            now - 90_000,
            NoticeKind::Recovery {
                cause: rewound(now),
                held_files: 0,
            },
        ),
        FakeScenario::RemoteMassDelete => {
            notice(
                saved,
                PHOTOS,
                now - 45_000,
                NoticeKind::RemoteMassDelete {
                    deletions: 3,
                    affected_files: 1_200,
                    space_files: 2_000,
                    exceeded: vec![MassDeleteLimit::ShareOfSpace { percent: 50 }],
                },
            );
            script.recent.insert(
                0,
                RecentChange {
                    space_id: PHOTOS.to_owned(),
                    path: "2024".to_owned(),
                    kind: NodeKind::Folder,
                    change: ChangeKind::Deleted,
                    direction: Direction::Downloaded,
                    at_ms: now - 45_000,
                },
            );
        }
        FakeScenario::ManyQuestions => {
            let items = (1..=1_500)
                .map(|i| {
                    file(
                        format!("node-big-{i}"),
                        format!("Export/{:02}/row-{i:05}.csv", i / 40),
                        ItemDetail::Plain,
                    )
                })
                .collect();
            question(
                saved,
                DOCUMENTS,
                now - 300_000,
                QuestionKind::MassDelete {
                    deletions: 1_500,
                    affected_files: 1_500,
                    space_files: 9_800,
                    exceeded: vec![MassDeleteLimit::Deletions { limit: 1_000 }],
                },
                items,
            );
            recovery(saved, now, 300);
            for n in 1..=25 {
                disappearance(saved, now, n, 3);
            }
        }
        FakeScenario::Problems => {
            let documents = Some(DOCUMENTS);
            let kinds = [
                (
                    documents,
                    Some("Taxes/2025.pdf"),
                    ProblemKind::FileUnreadable {
                        reason: "Permission denied".to_owned(),
                    },
                ),
                (
                    documents,
                    Some("Shared/notes.txt"),
                    ProblemKind::FileNotWritable {
                        reason: "Read-only file system".to_owned(),
                    },
                ),
                (
                    documents,
                    Some("Inbox/a:b.txt"),
                    ProblemKind::NameNotAllowed,
                ),
                (documents, Some("Links/current"), ProblemKind::NameTaken),
                (
                    documents,
                    Some("Huge/video.mov"),
                    ProblemKind::ChangeRejected {
                        reason: "QUOTA_EXCEEDED".to_owned(),
                    },
                ),
                (
                    documents,
                    Some(".koiignore"),
                    ProblemKind::IgnoreRuleInvalid { line: 7 },
                ),
                (Some(PHOTOS), None, ProblemKind::FolderMissing),
                (Some("space-archive"), None, ProblemKind::FolderNotAFolder),
                (
                    Some("space-archive"),
                    None,
                    ProblemKind::FolderUnreadable {
                        reason: "Operation not permitted".to_owned(),
                    },
                ),
                (
                    Some("space-archive"),
                    None,
                    ProblemKind::FolderOnOtherVolume,
                ),
                (Some("space-archive"), None, ProblemKind::MarkerMissing),
                (
                    Some("space-archive"),
                    None,
                    ProblemKind::MarkerUnreadable {
                        reason: "invalid JSON".to_owned(),
                    },
                ),
                (
                    Some("space-archive"),
                    None,
                    ProblemKind::MarkerOfOtherSpace {
                        space_id: "space-other".to_owned(),
                    },
                ),
                (
                    Some("space-archive"),
                    None,
                    ProblemKind::DiskOlderThanSyncState,
                ),
                (
                    Some("space-archive"),
                    None,
                    ProblemKind::UnsupportedFileSystem,
                ),
                (Some("space-archive"), None, ProblemKind::SpaceFormatTooNew),
                (None, None, ProblemKind::SignInRequired),
                (
                    None,
                    None,
                    ProblemKind::StateNotWritable {
                        reason: "No space left on device".to_owned(),
                    },
                ),
                (documents, Some("Logs/server.log"), ProblemKind::FileBusy),
            ];
            let count = kinds.len() as i64;
            for (i, (space, path, kind)) in kinds.into_iter().enumerate() {
                problem(saved, space, path, kind, now - (count - i as i64) * 60_000);
            }
            for folder in &mut saved.folders {
                if folder.space_id == PHOTOS {
                    folder.state = FolderState::Halted;
                }
            }
        }
    }
    script
}
