// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io;
use std::path::Path;

pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}
