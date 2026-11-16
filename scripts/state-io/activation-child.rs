// SPDX-License-Identifier: Apache-2.0

pub fn save(path: &std::path::Path) {
    let _ = std::fs::write(path, b"state");
}

macro_rules! writer {
    () => {
        pub fn nested(path: &std::path::Path) {
            let _ = std::fs::write(path, b"macro state");
        }
    };
}

macro_rules! wrapper {
    () => {
        writer!();
    };
}

wrapper!();

pub async fn mutex(lock: &std::sync::Mutex<u8>) {
    let guard = lock.lock().unwrap();
    std::future::ready(()).await;
    drop(guard);
}

pub async fn refcell(cell: &std::cell::RefCell<u8>) {
    let guard = cell.borrow();
    std::future::ready(()).await;
    drop(guard);
}
