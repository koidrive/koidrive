// SPDX-License-Identifier: Apache-2.0

use koidrive_cloudflare::Jurisdiction;
use koidrive_driver::Paths;
use url::Url;

use super::deploy::unfinished;
use super::session::confirm;
use super::state::{SetupState, StateFile, Step, UninstallProgress};
use super::uninstall;
use super::{Cloud, Session};
use crate::error::CliError;

impl Session {
    /// Deletes the Worker, then empties and deletes its bucket unless another Worker or application uses it (section 5).
    /// A state file of an unfinished uninstall in this account continues it, unless `bucket` names one.
    pub(super) async fn uninstall(
        &mut self,
        paths: &Paths,
        name: Option<&str>,
        bucket: Option<&str>,
        jurisdiction: Jurisdiction,
        yes: bool,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        match file.load()? {
            Some(mut state)
                if state.account == self.account()
                    && state.step == Step::Uninstalling
                    && state.script.is_empty()
                    && state.uninstall.is_none()
                    && bucket.is_some_and(|bucket| state.matches_bucket(bucket, jurisdiction)) =>
            {
                confirm(&self.terminal, &format!("Continue deleting only the saved bucket {} in the {} jurisdiction? This does not delete a Worker or change this device's background service.", state.bucket, state.jurisdiction.as_str()), yes, "--yes").await?;
                state.uninstall = Some(UninstallProgress::Cloud { server: None });
                file.save(&state)?;
                return self.finish_uninstall(paths, &file, state, None).await;
            }
            Some(state)
                if state.account == self.account()
                    && state.step == Step::Uninstalling
                    && (bucket.is_none()
                        || state.script.is_empty()
                            && state.uninstall.is_some()
                            && bucket.is_some_and(|bucket| {
                                state.matches_bucket(bucket, jurisdiction)
                            })) =>
            {
                println!(
                    "Continuing the uninstall that stopped earlier ({}).",
                    state.bucket
                );
                return self.finish_uninstall(paths, &file, state, name).await;
            }
            Some(other) => return Err(unfinished(&other, self.account(), file.path())),
            None => {}
        }
        let (mut state, contents) = match bucket {
            Some(bucket) => uninstall::plan(self, None, bucket, jurisdiction).await?,
            None => {
                let server = self.pick(name).await?;
                let bound = server
                    .bucket
                    .clone()
                    .ok_or_else(|| format!("{} binds no R2 bucket", server.script.id))?;
                uninstall::plan(self, Some(&server.script), &bound.name, bound.jurisdiction).await?
            }
        };
        let worker = match state.script.as_str() {
            "" => String::new(),
            script => format!("the Worker {script} and "),
        };
        confirm(
            &self.terminal,
            &format!(
                "Delete {worker}the R2 bucket {} in the {} jurisdiction with its {} objects? Every file in the bucket is lost, also anything another application stored there through R2's S3 API.",
                state.bucket,
                state.jurisdiction.as_str(),
                contents.objects
            ),
            yes,
            "--yes",
        ).await?;
        let server = if state.script.is_empty() {
            None
        } else {
            Some(self.worker_url(&state.script).await?.to_string())
        };
        state.uninstall = Some(UninstallProgress::Cloud { server });
        file.save(&state)?;
        self.finish_uninstall(paths, &file, state, None).await
    }

    async fn finish_uninstall(
        &mut self,
        paths: &Paths,
        file: &StateFile,
        mut state: SetupState,
        legacy_worker: Option<&str>,
    ) -> Result<(), CliError> {
        if state.uninstall.is_none() {
            let script = if state.script.is_empty() {
                legacy_worker.ok_or_else(|| {
                    let jurisdiction = if state.jurisdiction == Jurisdiction::Default { String::new() } else { format!(" --jurisdiction {}", state.jurisdiction.as_str()) };
                    let recovery = format!("Saved uninstall progress does not say whether this operation included a Worker. Keep the saved progress. If it included a Worker, run koi server delete --interactive --worker <deleted-worker-name> with that Worker's name. For a bucket-only operation, run koi server delete --interactive --bucket {}{jurisdiction} and confirm that the background service should stay unchanged.", crate::pairing::shell_word(&state.bucket));
                    CliError::from(format!("{recovery} Saved progress: {}", file.path().display())).human(recovery)
                })?
            } else {
                &state.script
            };
            let server = self.worker_url(script).await?.to_string();
            state.uninstall = Some(UninstallProgress::Cloud {
                server: Some(server),
            });
            file.save(&state)?;
        }
        if matches!(state.uninstall, Some(UninstallProgress::Cloud { .. })) {
            state = uninstall::finish(self, file, state).await?;
        }
        finish_uninstall_service(paths, file, state).await
    }
}

pub(super) async fn finish_uninstall_service(
    paths: &Paths,
    file: &StateFile,
    mut state: SetupState,
) -> Result<(), CliError> {
    let bucket_kept = match &state.uninstall {
        Some(UninstallProgress::Service {
            server,
            bucket_kept,
        }) => {
            let kept = *bucket_kept;
            if let Some(server) = server {
                let url = Url::parse(server).map_err(|_| CliError::from("the saved uninstall server address is invalid; keep the saved progress and run koi diagnose"))?;
                let report = crate::service::forget(paths, &url)
                    .await
                    .map_err(CliError::from)?;
                if let Some(report) = report {
                    print!("{}", report.text());
                }
            }
            state.uninstall = Some(UninstallProgress::Complete { bucket_kept: kept });
            file.save(&state)?;
            kept
        }
        Some(UninstallProgress::Complete { bucket_kept }) => *bucket_kept,
        _ => return Err(
            "the cloud uninstall has not finished; run koi server delete --interactive to continue"
                .into(),
        ),
    };
    file.remove()?;
    if bucket_kept {
        println!(
            "Kept the bucket {} and its files. Local cleanup is complete.",
            state.bucket
        );
    } else {
        println!("The uninstall is complete.");
    }
    Ok(())
}
