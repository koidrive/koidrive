// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

pub fn write(path: &Path, text: &str) -> Result<bool, String> {
    if fs::read(path).is_ok_and(|old| old == text.as_bytes()) {
        return Ok(false);
    }
    let failed = |error: io::Error| format!("cannot write {}: {error}", path.display());
    let dir = path.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir).map_err(failed)?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.persist(path).map_err(|error| failed(error.error))?;
    Ok(true)
}

pub fn create(file: tempfile::NamedTempFile, path: &Path) -> Result<File, tempfile::PersistError> {
    file.persist_noclobber(path)
}

pub fn persist_path(
    file: tempfile::TempPath,
    path: &Path,
) -> Result<(), tempfile::PathPersistError> {
    file.persist(path)
}

pub fn create_path(
    file: tempfile::TempPath,
    path: &Path,
) -> Result<(), tempfile::PathPersistError> {
    file.persist_noclobber(path)
}
