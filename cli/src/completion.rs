// SPDX-License-Identifier: Apache-2.0

use crate::error::CliError;
use crate::formatting::printable;
use crate::report::{Decision, Done, Envelope};
use crate::resolve::banner;
use crate::{output, report};
use koidrive_driver::Question;
use koidrive_ipc::Code;
use std::io::{self, Write};
use std::process::ExitCode;

/// Prints what the command produced and returns its exit status. Without `--json` the text goes to stdout and an error
/// to stderr; with it, one envelope goes to stdout. Pending decisions follow as a banner on stderr, or as the
/// envelope's decisions.
pub(crate) fn finish(
    command: &str,
    json: bool,
    verbose: bool,
    outcome: Result<Done, CliError>,
    pending: Vec<Question>,
) -> ExitCode {
    let concise = !json && !verbose && output::concise(command);
    let (exit, envelope, text, stderr, presentation, outcome, tone) = match outcome {
        Ok(done) => {
            let exit = done.exit;
            let (text, note) = done.output(concise);
            let (text, note) = (text.to_owned(), note.to_owned());
            let (presentation, outcome, tone) = (done.presentation, done.outcome, done.tone);
            (
                exit,
                Envelope::of(command, done),
                text,
                note,
                presentation,
                outcome,
                tone,
            )
        }
        Err(mut error) => {
            let partial = error.partial.take().map(|partial| *partial);
            let text = (partial.as_ref())
                .map_or_else(String::new, |done| done.output(concise).0.to_owned());
            let presentation = partial
                .as_ref()
                .map_or(output::Presentation::Text, |done| done.presentation);
            let shown = concise
                .then_some(error.human.as_deref())
                .flatten()
                .filter(|human| !human.is_empty())
                .unwrap_or(&error.message);
            let message = if error.presented {
                String::new()
            } else {
                format!("{}\n", printable(shown))
            };
            let exit = error.code.exit();
            (
                exit,
                Envelope::failed(command, error, partial),
                text,
                message,
                presentation,
                report::Outcome::Error,
                output::Tone::Attention,
            )
        }
    };
    let shown = match json {
        true if command == "_daemon" => writeln!(
            io::stdout(),
            "{}",
            serde_json::to_string(&envelope).expect("the envelope serializes")
        )
        .and_then(|()| io::stdout().flush()),
        true => {
            let mut envelope = envelope;
            if envelope.decisions.is_empty() {
                envelope.decisions = pending.iter().map(Decision::of).collect();
            }
            writeln!(
                io::stdout(),
                "{}",
                serde_json::to_string_pretty(&envelope).expect("the envelope serializes")
            )
            .and_then(|()| io::stdout().flush())
        }
        false => output::print(
            command,
            presentation,
            tone,
            &text,
            match outcome {
                report::Outcome::Error => output::Note::Error(&stderr),
                _ => output::Note::Information(&stderr),
            },
            banner(&pending).as_deref(),
        ),
    };
    match output::delivered(shown) {
        Ok(()) => ExitCode::from(exit),
        Err(error) => {
            let _ = writeln!(io::stderr(), "koi: could not write the result: {error}");
            ExitCode::from(Code::Failed.exit())
        }
    }
}
