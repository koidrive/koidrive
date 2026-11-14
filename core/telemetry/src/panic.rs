// SPDX-License-Identifier: Apache-2.0

use std::panic;

use crate::consent::Mode;
use crate::recorder::Pending;
use crate::schema::{Event, SourceFile};
use crate::store::Store;

/// Adds a hook that queues a [`Event::Panic`] while reports are on, and then runs the hook that was there before. It
/// gives up instead of waiting when the queue is busy.
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
            };
            let _ = store.try_absorb(Pending {
                events: vec![event],
                ..Pending::default()
            });
        }
        previous(info);
    }));
}
