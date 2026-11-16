// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::sync::Mutex;

pub async fn mutex(lock: &Mutex<usize>) {
    let mut value = lock.lock().unwrap();
    std::future::pending::<()>().await;
    *value += 1;
}

pub async fn refcell(cell: &RefCell<usize>) {
    let mut value = cell.borrow_mut();
    std::future::pending::<()>().await;
    *value += 1;
}
