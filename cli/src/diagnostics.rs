// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use cliclack::Theme as _;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::registry::LookupSpan;

pub struct Human {
    pub command: String,
}

#[derive(Default)]
struct Message(Option<String>);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = Some(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = Some(value.to_owned());
        }
    }
}

struct Theme;

impl cliclack::Theme for Theme {}

impl<S, N> FormatEvent<S, N> for Human
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let error = *event.metadata().level() == Level::ERROR;
        let mut message = Message::default();
        event.record(&mut message);
        let message = message.0.unwrap_or_else(|| {
            format!(
                "Koidrive reported {}. Run this command with --verbose for details.",
                if error { "an error" } else { "a warning" }
            )
        });
        let message = crate::printable(&message);
        if crate::output::rich(&self.command) {
            crate::terminal::colors(false);
            let theme = Theme;
            let symbol = if error {
                theme.error_symbol()
            } else {
                theme.warning_symbol()
            };
            write!(
                writer,
                "{}",
                theme.format_log_with_spacing(&message, &symbol, false)
            )
        } else {
            writeln!(
                writer,
                "{}: {message}",
                if error { "error" } else { "warning" }
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn human_diagnostics_keep_escaped_messages_and_omit_fields_and_metadata() {
        let capture = Capture(Arc::default());
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .event_format(Human {
                command: "status".into(),
            })
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                private_field = "fixture-only-secret",
                "Keep the files\nwhere they are\u{1b}[31m"
            );
            tracing::error!(private_field = "fixture-only-secret");
            tracing::warn!(private_field = "fixture-only-secret");
        });
        let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            text,
            "warning: Keep the files\\u{a}where they are\\u{1b}[31m\nerror: Koidrive reported an error. Run this command with --verbose for details.\nwarning: Koidrive reported a warning. Run this command with --verbose for details.\n"
        );
        assert!(!text.contains("fixture-only-secret"));
        assert!(!text.contains("koidrive_cli"));
    }
}
