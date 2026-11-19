// SPDX-License-Identifier: Apache-2.0

//! Serde for a path that keeps its bytes: a UTF-8 path as a string, any other path as an array of its bytes.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Deserialize)]
#[serde(untagged)]
enum Wire {
    Text(String),
    Bytes(Vec<u8>),
}

pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    match path.to_str() {
        Some(text) => serializer.serialize_str(text),
        None => path.as_os_str().as_bytes().serialize(serializer),
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    Ok(match Wire::deserialize(deserializer)? {
        Wire::Text(text) => text.into(),
        Wire::Bytes(bytes) => OsString::from_vec(bytes).into(),
    })
}

/// A list of paths, each written as [`serialize`] writes a path.
pub mod list {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    struct Raw<'a>(&'a Path);

    impl Serialize for Raw<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            super::serialize(self.0, serializer)
        }
    }

    #[derive(Deserialize)]
    struct Owned(#[serde(with = "crate::raw_path")] PathBuf);

    pub fn serialize<S: Serializer>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(paths.iter().map(|path| Raw(path)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Ok((Vec::<Owned>::deserialize(deserializer)?.into_iter())
            .map(|Owned(path)| path)
            .collect())
    }
}

/// A path that may be unknown, which an empty path stands for.
pub mod unknown {
    use std::path::{Path, PathBuf};

    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        super::serialize(path.as_deref().unwrap_or(Path::new("")), serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Ok(Some(super::deserialize(deserializer)?).filter(|path| !path.as_os_str().is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    use serde_json::json;

    use crate::FolderStatus;

    #[test]
    fn a_folder_status_names_its_folder_as_text_as_bytes_or_as_unknown() {
        let mut folder: FolderStatus = serde_json::from_value(json!({
            "space_id": "s1", "space_name": "Home", "path": "/a", "state": "stopped",
            "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
            "local_entries": null, "questions": 0, "problem": null
        }))
        .unwrap();
        assert_eq!(folder.path, Some(PathBuf::from("/a")));
        for (path, wire) in [
            (Some(PathBuf::from("/a")), json!("/a")),
            (
                Some(OsStr::from_bytes(b"/a\xff").into()),
                json!([47, 97, 255]),
            ),
            (None, json!("")),
        ] {
            folder.path = path;
            let sent = serde_json::to_value(&folder).unwrap();
            assert_eq!(sent["path"], wire);
            assert_eq!(
                serde_json::from_value::<FolderStatus>(sent).unwrap(),
                folder
            );
        }
    }
}
