// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn cli_status_update_metadata_is_unconsumed_by_the_app_fallback() {
    for update in [
        serde_json::Value::Null,
        serde_json::json!({ "state": "unknown", "version": "0.1.0" }),
        serde_json::json!({ "state": "off", "version": "0.1.0" }),
        serde_json::json!({
            "state": "up_to_date", "version": "0.1.0", "checked_at": "2026-11-13T00:00:00Z"
        }),
        serde_json::json!({
            "state": "available", "version": "0.1.0", "latest": "0.2.0",
            "url": "https://github.com/koidrive/koidrive/releases/tag/v0.2.0",
            "checked_at": "2026-11-13T00:00:00Z", "command": "koi update"
        }),
    ] {
        let mut data = serde_json::json!({
            "account": null, "running": false, "paused": true, "folders": []
        });
        if !update.is_null() {
            data["update"] = update;
        }
        let done = Done {
            outcome: Outcome::Ok,
            data,
            decisions: Vec::new(),
        };
        let (status, questions) = done.status().unwrap();
        assert_eq!(status.update, driver::Update::Unknown);
        assert!(status.paused);
        assert!(!status.running);
        assert!(questions.is_empty());
    }
}

fn child(script: &str, input: bool) -> Child {
    Command::new("/bin/sh")
        .args(["-c", script])
        .stdin(if input { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn times_out(child: &mut Child, input: Option<&str>) {
    let started = Instant::now();
    let error = finish(child, input, Duration::from_millis(100)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        matches!(
            rustix::process::waitpid(
                Some(rustix::process::Pid::from_child(child)),
                rustix::process::WaitOptions::NOHANG,
            ),
            Err(rustix::io::Errno::CHILD),
        ),
        "child was not reaped",
    );
}

#[test]
fn a_helper_that_never_exits_is_killed_and_reaped() {
    let mut child = child("exec sleep 30", false);
    times_out(&mut child, None);
    assert!(!child.try_wait().unwrap().unwrap().success());
}

#[test]
fn inherited_output_pipes_do_not_outlive_the_deadline() {
    let mut child = child(
        "sleep 3 & printf result; printf diagnostic >&2; exit 0",
        false,
    );
    assert!(child.wait().unwrap().success());
    times_out(&mut child, None);
    assert!(child.try_wait().unwrap().unwrap().success());
}

#[test]
fn a_helper_that_never_reads_input_is_killed_and_reaped() {
    let mut child = child("exec sleep 30", true);
    let input = "x".repeat(1024 * 1024);
    times_out(&mut child, Some(&input));
    assert!(!child.try_wait().unwrap().unwrap().success());
}

#[test]
fn continuous_output_cannot_extend_the_deadline() {
    let mut child = child("while :; do printf out; printf err >&2; done", false);
    times_out(&mut child, None);
}

#[test]
fn input_and_both_output_pipes_are_drained_without_deadlock_or_truncation() {
    let input = "private-code\n".repeat(16384);
    let mut child = child("cat; head -c 131072 /dev/zero >&2", true);
    let output = finish(&mut child, Some(&input), Duration::from_secs(5)).unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, input.as_bytes());
    assert_eq!(output.stderr, vec![0; 131072]);
}

#[test]
fn recovery_decisions_keep_the_stored_cause_and_accept_older_results() {
    let mut data = serde_json::json!({
        "id": "q1",
        "space_id": "s1",
        "kind": "recovery",
        "group": "changed_on_this_device",
        "asked_at_ms": 42,
    });
    let older: DecisionData = serde_json::from_value(data.clone()).unwrap();
    assert_eq!(older.driver().unwrap().recovery_cause, None);

    let cause = driver::RecoveryCause {
        trigger: driver::RecoveryTrigger::SpaceRewound,
        restore_point_ms: Some(7),
    };
    data["recovery_cause"] = serde_json::to_value(&cause).unwrap();
    let recorded: DecisionData = serde_json::from_value(data).unwrap();
    assert_eq!(recorded.driver().unwrap().recovery_cause, Some(cause));
}

#[test]
fn a_conflict_without_its_complete_payload_does_not_invent_revision_data() {
    let mut data = serde_json::json!({
        "id": "q1",
        "space_id": "s1",
        "kind": "conflict",
        "asked_at_ms": 42,
    });
    let older: DecisionData = serde_json::from_value(data.clone()).unwrap();
    assert!(older.driver().is_none());

    data["conflict"] = serde_json::json!({ "type": "ambiguous_disappearance" });
    let wrong: DecisionData = serde_json::from_value(data.clone()).unwrap();
    assert!(wrong.driver().is_none());

    data["conflict"] = serde_json::json!({
        "type": "conflict",
        "original": { "node_id": "original" },
        "copy": { "node_id": "copy" },
    });
    assert!(serde_json::from_value::<DecisionData>(data).is_err());
}
