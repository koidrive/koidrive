use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use koidrive_driver::{Driver, Question};

/// The decisions open when it is called are the baseline, so call it before the folders start.
pub fn decisions(driver: Driver) -> impl Future<Output = ()> {
    let mut watch = driver.watch();
    let mut open = driver.questions().unwrap_or_default();
    async move {
        while watch.changed().await.is_ok() {
            let Ok(questions) = driver.questions() else {
                continue;
            };
            let folders: BTreeMap<String, PathBuf> = (driver.status().folders.into_iter())
                .filter_map(|folder| Some((folder.space_id, folder.path?)))
                .collect();
            for question in opened(&open, &questions) {
                let folder = (folders.get(&question.space_id)).map_or_else(
                    || "A linked folder".to_owned(),
                    |path| crate::printable(&path.to_string_lossy()),
                );
                let body = format!("{folder}: {}", crate::question_notice(question));
                tokio::spawn(async move { send("Koidrive needs a decision", &body).await });
            }
            open = questions;
        }
    }
}

fn opened<'a>(before: &[Question], questions: &'a [Question]) -> Vec<&'a Question> {
    let lines: BTreeSet<&str> = before
        .iter()
        .map(|question| question.lineage.as_str())
        .collect();
    (questions.iter())
        .filter(|question| !lines.contains(question.lineage.as_str()))
        .collect()
}

#[cfg(target_os = "macos")]
async fn send(title: &str, body: &str) {
    let script = [
        "on run argv",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "end run",
    ];
    let mut command = crate::program::system_async("/usr/bin/osascript");
    for line in script {
        command.args(["-e", line]);
    }
    match command.args([title, body]).output().await {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::info!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "a desktop notification failed"
        ),
        Err(error) => tracing::info!(%error, "a desktop notification failed"),
    }
}

#[cfg(target_os = "linux")]
async fn send(title: &str, body: &str) {
    use std::collections::HashMap;
    let connection = match zbus::Connection::session().await {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(%error, "no session bus for a desktop notification");
            return;
        }
    };
    let body = markup_escaped(body);
    let hints: HashMap<&str, zbus::Variant<'_>> = HashMap::new();
    let call = connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Koidrive",
                0u32,
                "",
                title,
                body.as_str(),
                Vec::<&str>::new(),
                hints,
                -1i32,
            ),
        )
        .await;
    if let Err(error) = call {
        tracing::info!(%error, "a desktop notification failed");
    }
}

/// The body with the characters that notification servers read as markup written as entities, so a path such as
/// `<b>a&b` shows as it is.
#[cfg(any(target_os = "linux", test))]
fn markup_escaped(body: &str) -> String {
    body.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn send(_title: &str, _body: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use koidrive_driver::QuestionKind;

    fn decision(id: &str, lineage: &str, space_id: &str) -> Question {
        Question {
            id: id.into(),
            space_id: space_id.into(),
            asked_at_ms: 1,
            recovery_cause: None,
            kind: QuestionKind::MassDelete {
                deletions: 11,
                affected_files: 11,
                space_files: 20,
                exceeded: vec![koidrive_driver::Limit::Share],
            },
            items: vec![koidrive_driver::HeldItem {
                node_id: "n1".into(),
                path: "d".into(),
                kind: Some(koidrive_driver::ItemKind::Folder),
                files: 11,
                device_id: None,
                device_name: None,
                size: None,
                modified_at_ms: None,
                restore: None,
            }],
            lineage: lineage.into(),
        }
    }

    #[test]
    fn a_body_reaches_the_notification_server_without_markup() {
        assert_eq!(
            markup_escaped("Sync/<b>a&b</b>: held 3 deletions"),
            "Sync/&lt;b&gt;a&amp;b&lt;/b&gt;: held 3 deletions"
        );
    }

    fn ids(questions: Vec<&Question>) -> Vec<&str> {
        questions
            .into_iter()
            .map(|question| question.id.as_str())
            .collect()
    }

    #[test]
    fn a_decision_is_told_once_when_it_opens_and_not_when_it_continues_in_a_replacement() {
        let first = [decision("q1", "q1", "s1")];
        assert_eq!(ids(opened(&[], &first)), ["q1"]);
        assert_eq!(ids(opened(&first, &first)), Vec::<&str>::new());
        let replaced = [decision("q2", "q1", "s1")];
        assert_eq!(ids(opened(&first, &replaced)), Vec::<&str>::new());
        let again = [decision("q3", "q3", "s1")];
        assert_eq!(ids(opened(&replaced, &again)), ["q3"]);
    }

    #[test]
    fn a_new_decision_about_the_same_files_is_told_even_when_the_close_was_not_seen() {
        let answered = [decision("q1", "q1", "s1")];
        let reopened = [decision("q2", "q2", "s1")];
        assert_eq!(answered[0].items, reopened[0].items);
        assert_eq!(ids(opened(&answered, &reopened)), ["q2"]);
    }
}
