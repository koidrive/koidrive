// SPDX-License-Identifier: Apache-2.0

/// GUN-388. The status named only how many items could not be synced, so the user could not tell which ones or why.
#[test]
fn the_status_names_a_node_the_server_rejected_an_op_for_and_why() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    assert_eq!(kinds(&commit), ["create file"]);
    let last_seq = a.folder.engine.cursor.as_ref().unwrap().last_seq;
    let result = CommitResult {
        seq: last_seq + 1,
        results: vec![koidrive_proto::v1::OpResult {
            code: op_result::Code::Rejected as i32,
            reject_reason: "INVALID_NAME".into(),
            ..Default::default()
        }],
        changes: Vec::new(),
        ..Default::default()
    };
    let step = a.folder.sent(Ok(result));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert_eq!(
        a.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: \"notes.txt\" (the server rejected it: INVALID_NAME); run koi folder restart once it is fixed"
        )
    );
}

/// GUN-388, for a commit the server refused as a whole: the node it leaves out of commits is named with the refusal.
#[test]
fn the_status_names_a_node_of_a_refused_commit_and_the_refusal() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let refusal = koidrive_proto::v1::Error {
        code: Code::LimitExceeded as i32,
        message: "the commit is too large".into(),
        ..Default::default()
    };
    let step = a.folder.sent(Err(CallError::Server(ServerError(refusal))));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert_eq!(
        a.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: \"notes.txt\" (the server refused it: CODE_LIMIT_EXCEEDED: the commit is too large); run koi folder restart once it is fixed"
        )
    );
}

#[test]
fn a_refused_commit_counts_its_code_and_each_node_it_leaves_out() {
    use crate::failures::{self, Failure, Refusal};

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let refusal = koidrive_proto::v1::Error {
        code: Code::LimitExceeded as i32,
        message: "the commit of /home/alice/notes.txt is too large".into(),
        ..Default::default()
    };
    failures::take_counted_here();
    let step = a.folder.sent(Err(CallError::Server(ServerError(refusal))));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert_eq!(
        failures::take_counted_here(),
        [
            (Failure::Refused(Refusal::LimitExceeded), 1),
            (Failure::ProblemNode(Refusal::LimitExceeded), 1)
        ]
        .into()
    );
}

#[test]
fn an_upload_that_fails_counts_how_it_failed() {
    use crate::failures::{self, Failure, Transfer as Failed};

    let unavailable = Failure::Upload(Failed::Unavailable);
    let before = failures::count(unavailable);
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    let error = ChunkError::Unavailable {
        hash: upload.chunks[0].hash.clone(),
        message: "connection reset".into(),
        cause: Some(koidrive_net::Cause::Reset),
    };
    let step = a.folder.uploaded(Err(error));
    a.take(step).unwrap();
    assert!(failures::count(unavailable) > before);
}

#[test]
fn a_download_the_server_refuses_counts_how_it_failed_and_the_code() {
    use crate::failures::{self, Failure, Refusal, Transfer as Failed};

    let counted = [
        Failure::Download(Failed::Refused),
        Failure::Refused(Refusal::QuotaExceeded),
    ];
    let before = counted.map(failures::count);
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    *remote.chunks.refusal.lock().unwrap() = Some(ServerError(koidrive_proto::v1::Error {
        code: koidrive_proto::v1::error::Code::QuotaExceeded as i32,
        message: "the daily limit is used up".into(),
        retry_after_ms: 3_600_000,
        ..Default::default()
    }));
    let pending = loop {
        match b.work.pop_front().expect("an effect is staged") {
            Work::Stage(pending) => break pending,
            other => b.perform(other, &mut remote).unwrap(),
        }
    };
    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    let after = counted.map(failures::count);
    assert!(after[0] > before[0] && after[1] > before[1], "{after:?}");
}

#[test]
fn a_conflict_copy_is_counted_as_a_question_asked() {
    use crate::failures::{self, Asked, Failure};

    let asked = Failure::Asked(Asked::Conflict);
    let before = failures::count(asked);
    let mut remote = Remote::new();
    let (_, (_, mut b)) = edited_on_both(&mut remote);
    assert!(b.folder.take_questions_changed());
    assert!(failures::count(asked) > before);
}

#[test]
fn a_commit_too_large_to_send_counts_as_too_large_and_never_as_refused() {
    use crate::failures::{self, Failure, TooLarge};

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    failures::take_counted_here();
    let step = a.folder.sent(Err(CallError::TooLarge { bytes: 9 << 20 }));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert_eq!(
        failures::take_counted_here(),
        [
            (Failure::TooLarge(TooLarge::Commit), 1),
            (Failure::TooLarge(TooLarge::ProblemNode), 1)
        ]
        .into()
    );
    let problem = a.folder.view().problem.unwrap();
    assert!(
        problem.contains("\"notes.txt\" (it is too large to send"),
        "{problem}"
    );
}

#[test]
fn an_upload_that_halts_is_counted_by_its_cause() {
    use crate::failures::{self, Halted};

    let cases = [
        (ChunkError::NoTarget { hash: Vec::new() }, Halted::Protocol),
        (
            ChunkError::Call(CallError::UnexpectedResult),
            Halted::Protocol,
        ),
        (
            ChunkError::Call(CallError::Server(forbidden())),
            Halted::Refused,
        ),
        (
            ChunkError::Refused {
                hash: Vec::new(),
                status: 403,
                error: Some(forbidden()),
            },
            Halted::Refused,
        ),
    ];
    for (error, halted) in cases {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        let Some(Work::Upload(_)) = a.work.pop_front() else {
            panic!("the new file is uploaded");
        };
        let shown = error.to_string();
        let Err(stop) = a.folder.uploaded(Err(error)) else {
            panic!("{shown} halts the upload");
        };
        assert_eq!(failures::halted(&stop), halted, "{shown}");
    }
}

#[test]
fn a_download_that_halts_is_counted_by_its_cause() {
    use crate::failures::{self, Halted};

    let cases = [
        (
            CallError::Stopped(StopReason::CredentialsRefused),
            Halted::CredentialsRefused,
        ),
        (CallError::Server(forbidden()), Halted::Refused),
        (CallError::UnexpectedResult, Halted::Protocol),
    ];
    for (error, halted) in cases {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
        let site = Site::new(&remote, "b");
        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        *remote.chunks.call_error.lock().unwrap() = Some(error.clone());
        let pending = loop {
            match b.work.pop_front().expect("an effect is staged") {
                Work::Stage(pending) => break pending,
                other => b.perform(other, &mut remote).unwrap(),
            }
        };
        let staged = ready(pending.stage(&remote.chunks));
        let Err(stop) = b.folder.staged(pending, staged) else {
            panic!("{error} halts the download");
        };
        assert_eq!(failures::halted(&stop), halted, "{error}");
    }
}

#[test]
fn a_download_refused_for_good_halts_as_refused() {
    use crate::failures::{self, Halted};

    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    *remote.chunks.refusal.lock().unwrap() = Some(forbidden());
    let pending = loop {
        match b.work.pop_front().expect("an effect is staged") {
            Work::Stage(pending) => break pending,
            other => b.perform(other, &mut remote).unwrap(),
        }
    };
    let staged = ready(pending.stage(&remote.chunks));
    let Err(stop) = b.folder.staged(pending, staged) else {
        panic!("a refusal for good halts the download");
    };
    assert_eq!(failures::halted(&stop), Halted::Refused);
}

#[test]
fn a_listing_that_halts_is_counted_by_its_cause() {
    use crate::failures::{self, Halted};

    let cases = [
        (
            CallError::Stopped(StopReason::DeviceRevoked),
            Halted::DeviceRevoked,
        ),
        (CallError::Server(forbidden()), Halted::Refused),
        (CallError::UnexpectedResult, Halted::Protocol),
    ];
    for (error, halted) in cases {
        let stop = super::later("s1", "tree", Listing::default(), &error).unwrap_err();
        assert_eq!(failures::halted(&stop), halted, "{error}");
    }
}

/// GUN-388, for an effect that failed: the node is named with why the effect failed.
#[test]
fn the_status_names_a_node_whose_effect_failed_and_why() {
    let mut remote = Remote::new();
    let (_site, b, _) = failed_at_a_taken_name(&mut remote);
    assert_eq!(
        b.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: \"notes.txt\" (the server's change could not be made here: another entry holds its name); run koi folder restart once it is fixed"
        )
    );
}

/// GUN-388, for a node L does not hold yet: a write the server made in a folder fails, and the status names the
/// write by its whole path, not by its name alone.
#[test]
fn the_status_names_a_failed_write_in_a_folder_by_its_whole_path() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("docs")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    a.create(
        &mut remote,
        &site_a.root.join("docs/notes.txt"),
        b"written on a",
    );
    b.pull(&remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the effect that writes docs/notes.txt is staged");
    };
    fs::write(site.root.join("docs/notes.txt"), b"the user's").unwrap();
    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    assert_eq!(b.folder.engine.effects.blocked().len(), 1);
    assert_eq!(
        b.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: \"docs/notes.txt\" (the server's change could not be made here: another entry \
             holds its name); run koi folder restart once it is fixed"
        )
    );
}

/// GUN-388. A node whose effect failed and that the server also left out of commits is one item, with both reasons.
#[test]
fn the_status_names_a_node_stuck_for_two_reasons_once() {
    let mut remote = Remote::new();
    let (_site, mut b, _) = failed_at_a_taken_name(&mut remote);
    let id = b
        .folder
        .engine
        .effects
        .blocked()
        .keys()
        .next()
        .unwrap()
        .clone();
    let mut state = b.folder.engine.commits.state();
    state.problems.insert(id.clone());
    b.folder.engine.commits = koidrive_engine::commit::Commits::from_state(state).unwrap();
    (b.folder.refused_nodes).insert(id, "the server rejected it: INVALID_NAME".into());
    assert_eq!(
        b.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: \"notes.txt\" (the server's change could not be made here: another entry holds \
             its name; the server rejected it: INVALID_NAME); run koi folder restart once it is fixed"
        )
    );
}

/// GUN-388. A result that waits for the log keeps the server's reason through the first scan of a start, so the
/// node it leaves out of commits once the log reaches it is named with that reason.
#[test]
fn the_reason_of_a_result_that_waits_for_the_log_outlasts_the_first_scan() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    a.folder.scanned = false;
    let seq = a.folder.engine.cursor.as_ref().unwrap().last_seq;
    let result = CommitResult {
        seq: seq + 2,
        results: vec![koidrive_proto::v1::OpResult {
            code: op_result::Code::Rejected as i32,
            reject_reason: "INVALID_NAME".into(),
            ..Default::default()
        }],
        changes: Vec::new(),
        ..Default::default()
    };
    let step = a.folder.sent(Ok(result.clone()));
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.problems().is_empty());
    let step = a.folder.scanned();
    a.take(step).unwrap();
    let record = CommitRecord {
        seq: seq + 1,
        ..Default::default()
    };
    a.event(Event::Changes(vec![record])).unwrap();
    let committed = Committed {
        commit_id: commit.commit_id,
        result,
    };
    a.event(Event::Committed(committed)).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert_eq!(
        a.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: \"notes.txt\" (the server rejected it: INVALID_NAME); run koi folder restart once \
             it is fixed"
        )
    );
}


fn unreachable_server() -> koidrive_net::DisconnectReason {
    koidrive_net::DisconnectReason::Transport(koidrive_net::Transport {
        step: koidrive_net::Step::Connect,
        cause: koidrive_net::Cause::Dns,
        detail: "failed to lookup address information".into(),
    })
}

fn offline_cause(view: &View) -> Option<(Unreachable, i64)> {
    let wait = (view.wait.as_ref()).filter(|_| view.state == FolderState::Offline)?;
    Some((wait.unreachable.clone()?, wait.until_ms))
}

#[test]
fn a_server_that_cannot_be_reached_shows_offline_with_the_cause_and_the_next_try() {
    let mut remote = Remote::new();
    let (_site, mut a) = linked(&mut remote, "a");
    a.event(Event::Disconnected {
        reason: unreachable_server(),
        retry_in: Duration::from_secs(30),
    })
    .unwrap();
    let view = a.folder.view();
    assert_eq!(view.state, FolderState::Offline);
    let (unreachable, _) = offline_cause(&view).expect("the status says why");
    assert_eq!(unreachable.why, "the server's address could not be found");
    assert!(!unreachable.no_internet);
    let problem = view.problem.unwrap_or_default();
    assert!(
        problem.starts_with("can't connect to the server: the server's address could not be found; trying again at "),
        "{problem}"
    );

    a.connect(&remote);
    let view = a.folder.view();
    assert_ne!(view.state, FolderState::Offline);
    assert_eq!(view.wait, None);
    assert_eq!(view.problem, None);
}

#[test]
fn a_quick_reconnect_shows_connecting_until_it_fails_again() {
    let mut remote = Remote::new();
    let (_site, mut a) = linked(&mut remote, "a");
    let disconnected = || Event::Disconnected {
        reason: unreachable_server(),
        retry_in: Duration::from_secs(1),
    };
    a.event(disconnected()).unwrap();
    let view = a.folder.view();
    assert_eq!(view.state, FolderState::Connecting);
    assert_eq!(offline_cause(&view), None);

    a.event(disconnected()).unwrap();
    assert_eq!(a.folder.view().state, FolderState::Offline);
}

#[test]
fn a_transfer_that_cannot_reach_the_server_shows_offline() {
    for (cause, offline) in [
        (koidrive_net::Cause::Unreachable, true),
        (koidrive_net::Cause::Status(500), false),
    ] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        let Some(Work::Upload(upload)) = a.work.pop_front() else {
            panic!("the new file is uploaded");
        };
        let error = ChunkError::Unavailable {
            hash: upload.chunks[0].hash.clone(),
            message: "error sending request".into(),
            cause: Some(cause),
        };
        let step = a.folder.uploaded(Err(error));
        a.take(step).unwrap();
        let view = a.folder.view();
        assert_eq!(view.state == FolderState::Offline, offline, "{cause:?}");
        assert_eq!(
            offline_cause(&view).map(|(unreachable, _)| unreachable.why),
            offline.then(|| "the network or the server is unreachable".to_owned()),
            "{cause:?}"
        );
        if offline {
            let problem = view.problem.unwrap_or_default();
            assert!(
                problem.starts_with("can't connect to the server: the network or the server is unreachable; trying again at "),
                "{problem}"
            );
        }
    }
}

#[test]
fn a_connection_the_server_refused_or_closed_is_not_offline_and_says_what_happened() {
    use koidrive_net::DisconnectReason;

    for (reason, said) in [
        (
            DisconnectReason::Refused { status: 503 },
            "the server answered HTTP 503; trying again at ",
        ),
        (
            DisconnectReason::Closed {
                code: Some(1011),
                reason: "internal error".into(),
            },
            "the server closed the connection (code 1011); trying again at ",
        ),
        (
            DisconnectReason::ProtocolViolation("an unknown message".into()),
            "the server broke the protocol: an unknown message; trying again at ",
        ),
    ] {
        let mut remote = Remote::new();
        let (_site, mut a) = linked(&mut remote, "a");
        a.event(Event::Disconnected {
            reason: reason.clone(),
            retry_in: Duration::from_secs(30),
        })
        .unwrap();
        let view = a.folder.view();
        assert_eq!(view.state, FolderState::Connecting, "{reason:?}");
        assert_eq!(offline_cause(&view), None, "{reason:?}");
        let problem = view.problem.unwrap_or_default();
        assert!(problem.starts_with(said), "{problem}");
    }
}

#[test]
fn a_device_without_internet_says_so() {
    let mut remote = Remote::new();
    let (_site, mut a) = linked(&mut remote, "a");
    a.event(Event::Disconnected {
        reason: koidrive_net::DisconnectReason::Transport(koidrive_net::Transport {
            step: koidrive_net::Step::Connect,
            cause: koidrive_net::Cause::NoInternet,
            detail: "failed to lookup address information".into(),
        }),
        retry_in: Duration::from_secs(30),
    })
    .unwrap();
    let view = a.folder.view();
    assert_eq!(view.state, FolderState::Offline);
    let (unreachable, _) = offline_cause(&view).expect("the status says why");
    assert!(unreachable.no_internet);
    let problem = view.problem.unwrap_or_default();
    assert!(problem.starts_with("no internet connection; trying again at "), "{problem}");
}

#[test]
fn an_upload_sent_again_is_not_shown_offline_while_it_runs() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    let error = ChunkError::Unavailable {
        hash: upload.chunks[0].hash.clone(),
        message: "error sending request".into(),
        cause: Some(koidrive_net::Cause::Unreachable),
    };
    let step = a.folder.uploaded(Err(error));
    a.take(step).unwrap();
    assert_eq!(a.folder.view().state, FolderState::Offline);
    while !matches!(a.work.front(), Some(Work::ResendAfter(_)) | None) {
        a.work.pop_front();
    }
    assert!(matches!(a.work.pop_front(), Some(Work::ResendAfter(_))), "the upload waits");
    let step = a.folder.resend();
    a.take(step).unwrap();
    assert!(
        a.work.iter().any(|work| matches!(work, Work::Upload(_))),
        "the wait is over and the upload runs again"
    );
    let view = a.folder.view();
    assert_ne!(
        view.state,
        FolderState::Offline,
        "the upload is under way again, yet the folder reads {:?}: {:?}",
        view.state,
        view.problem
    );
}

#[test]
fn a_next_try_that_has_passed_is_not_shown_as_the_next_try() {
    let mut remote = Remote::new();
    let (_site, mut a) = linked(&mut remote, "a");
    for _ in 0..2 {
        a.event(Event::Disconnected {
            reason: koidrive_net::DisconnectReason::Timeout,
            retry_in: Duration::from_millis(300),
        })
        .unwrap();
    }
    assert_eq!(a.folder.view().state, FolderState::Offline);
    std::thread::sleep(Duration::from_millis(600));
    let view = a.folder.view();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let next = offline_cause(&view).map(|(_, retry_at_ms)| retry_at_ms);
    assert!(
        view.state != FolderState::Offline || next.is_some_and(|at| at >= now_ms),
        "the attempt is under way, yet the folder reads {:?} with its next try {:?} ms in the past: {:?}",
        view.state,
        next.map(|at| now_ms - at),
        view.problem
    );
}

