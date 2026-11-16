// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::path::Path;
use std::sync::Mutex;

macro_rules! quiet {
    ($($lint:tt)*) => {
        #[allow($($lint)*)]
        pub fn file(path: &Path) {
            let _ = std::fs::write(path, b"x");
        }
    };
}

quiet!(clippy::disallowed_methods);

macro_rules! quiet_lock {
    ($($lint:tt)*) => {
        #[allow($($lint)*)]
        pub async fn lock(value: &Mutex<u8>) {
            let guard = value.lock().unwrap();
            std::future::ready(()).await;
            drop(guard);
        }
    };
}

quiet_lock!(clippy::await_holding_lock);

macro_rules! quiet_borrow {
    ($($lint:tt)*) => {
        #[expect($($lint)*)]
        pub async fn borrow(value: &RefCell<u8>) {
            let guard = value.borrow();
            std::future::ready(()).await;
            drop(guard);
        }
    };
}

quiet_borrow!(clippy::await_holding_refcell_ref);
