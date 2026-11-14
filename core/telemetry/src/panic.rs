// SPDX-License-Identifier: Apache-2.0

use std::backtrace::Backtrace;
use std::panic;
use std::time::Duration;

use crate::consent::Mode;
use crate::recorder::Pending;
use crate::schema::{Event, SourceFile, Symbol};
use crate::store::Store;

const MAX_FRAMES: usize = 24;

/// Adds a hook that queues a [`Event::Panic`] while reports are on, and then runs the hook that was there before. It
/// gives up after a tenth of a second instead of waiting when the queue is busy.
pub fn install_panic_hook(store: Store) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let on = matches!(
            store
                .status(|name| std::env::var_os(name))
                .map(|status| status.mode),
            Ok(Mode::On | Mode::Debug)
        );
        if on && let Some(location) = info.location() {
            let event = Event::Panic {
                at: crate::now(),
                file: SourceFile::from_location(location.file()),
                line: location.line(),
                frames: frames(&Backtrace::force_capture().to_string()),
            };
            for _ in 0..20 {
                let pending = Pending {
                    events: vec![event.clone()],
                    ..Pending::default()
                };
                match store.try_absorb(pending) {
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    _ => break,
                }
            }
        }
        previous(info);
    }));
}

fn frames(backtrace: &str) -> Vec<Symbol> {
    (backtrace.lines())
        .filter_map(|line| {
            let (index, function) = line.trim_start().split_once(": ")?;
            index.parse::<usize>().ok()?;
            Symbol::ours(function)
        })
        .filter(|symbol| !symbol.as_str().starts_with("koidrive_telemetry::panic::"))
        .take(MAX_FRAMES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_keep_koidrive_functions_without_generics_paths_or_dependencies() {
        let printed = "   0: std::backtrace_rs::backtrace::libunwind::trace
             at /rustc/abc/library/std/src/../../backtrace/src/backtrace/libunwind.rs:117:9
   1: koidrive_telemetry::panic::install_panic_hook::{{closure}}
   2: <koidrive_driver::folder::Folder as core::ops::Drop>::drop
             at /Users/alice/koidrive/core/driver/src/folder.rs:12:5
   3: koidrive_driver::engine::apply::<alloc::string::String>
   4: tokio::runtime::task::harness::Harness<T,S>::poll
   5: koi::main::{{closure}}
   6: koidrive_net::upload(\"/home/alice/Notes.docx\")
   7: koi_helper::run";
        let frames: Vec<String> = frames(printed).into_iter().map(String::from).collect();
        assert_eq!(
            frames,
            [
                "koidrive_driver::folder::Folder::drop",
                "koidrive_driver::engine::apply",
                "koi::main::{{closure}}",
            ]
        );
    }

    #[test]
    fn a_symbol_from_a_file_is_checked_again() {
        for text in ["koidrive_driver::folder::run", "koi::main"] {
            let symbol: Symbol = serde_json::from_value(serde_json::json!(text)).unwrap();
            assert_eq!(symbol.as_str(), text);
        }
        for text in [
            "tokio::spawn",
            "koidrive_driver::run<T>",
            "/home/alice",
            "koidrive_x::a b",
        ] {
            assert!(
                serde_json::from_value::<Symbol>(serde_json::json!(text)).is_err(),
                "{text}"
            );
        }
    }
}
