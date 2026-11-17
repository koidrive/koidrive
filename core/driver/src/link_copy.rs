// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use koidrive_fs::VolumeId;
use koidrive_store::Link;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Serialize, Deserialize)]
struct Copy {
    space_id: String,
    #[serde(default, deserialize_with = "lenient")]
    space_name: String,
    root: Root,
    #[serde(default, deserialize_with = "lenient")]
    root_dev: u64,
    #[serde(default, deserialize_with = "lenient")]
    root_volume: Option<Vec<u8>>,
    server_url: String,
    device_id: String,
    #[serde(default, deserialize_with = "lenient")]
    linked_at_ms: i64,
    #[serde(default, deserialize_with = "lenient")]
    found_marker: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Root {
    Text(String),
    Bytes(Vec<u8>),
}

fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    deserializer: D,
) -> Result<T, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

pub(crate) fn path(store: &Path) -> PathBuf {
    store.with_file_name("link.json")
}

pub(crate) fn write(store: &Path, link: &Link) -> io::Result<()> {
    let Link {
        space_id,
        space_name,
        root,
        root_dev,
        root_volume,
        server_url,
        device_id,
        linked_at_ms,
        found_marker,
    } = link.clone();
    let copy = Copy {
        space_id,
        space_name,
        root: match root.into_os_string().into_string() {
            Ok(text) => Root::Text(text),
            Err(bytes) => Root::Bytes(bytes.into_vec()),
        },
        root_dev,
        root_volume: root_volume.map(|volume| volume.as_bytes().to_vec()),
        server_url,
        device_id,
        linked_at_ms,
        found_marker,
    };
    koidrive_state_file::write(&path(store), &serde_json::to_vec(&copy)?)
}

pub(crate) fn read(store: &Path, space_id: &str) -> Option<Link> {
    let copy: Copy = serde_json::from_slice(&std::fs::read(path(store)).ok()?).ok()?;
    let root = match copy.root {
        Root::Text(text) => PathBuf::from(text),
        Root::Bytes(bytes) => PathBuf::from(OsString::from_vec(bytes)),
    };
    (copy.space_id == space_id && root.is_absolute()).then(|| Link {
        space_id: copy.space_id,
        space_name: copy.space_name,
        root,
        root_dev: copy.root_dev,
        root_volume: (copy.root_volume.as_deref()).and_then(VolumeId::from_bytes),
        server_url: copy.server_url,
        device_id: copy.device_id,
        linked_at_ms: copy.linked_at_ms,
        found_marker: copy.found_marker,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn link(root: PathBuf) -> Link {
        Link {
            space_id: "s1".into(),
            space_name: "Work".into(),
            root,
            root_dev: 7,
            root_volume: Some(VolumeId::Uuid([3; 16])),
            server_url: "https://files.example.com/".into(),
            device_id: "d1".into(),
            linked_at_ms: 5,
            found_marker: true,
        }
    }

    #[test]
    fn a_copy_reads_back_as_written_also_for_a_path_that_is_not_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        for root in [
            PathBuf::from("/Users/a/Work"),
            PathBuf::from(OsString::from_vec(b"/Users/a/W\xffrk".to_vec())),
        ] {
            let link = link(root);
            write(&store, &link).unwrap();
            assert_eq!(read(&store, "s1"), Some(link));
        }
        assert_eq!(read(&store, "s2"), None);
    }

    #[test]
    fn a_copy_is_read_leniently_but_needs_its_space_an_absolute_root_and_its_device() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let read_as = |copy: &[u8]| {
            std::fs::write(path(&store), copy).unwrap();
            read(&store, "s1")
        };
        let partial = read_as(
            json!({
                "space_id": "s1", "root": "/Work", "server_url": "u", "device_id": "d1",
                "root_volume": "not bytes", "root_dev": -1, "linked_at_ms": "soon", "from_later": 1,
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            (partial.root, partial.root_volume, partial.root_dev),
            (PathBuf::from("/Work"), None, 0)
        );
        assert_eq!(
            (partial.space_name, partial.found_marker),
            (String::new(), false)
        );
        for unusable in [
            json!({ "space_id": "s1", "root": "Work", "server_url": "u", "device_id": "d1" }),
            json!({ "space_id": "s1", "server_url": "u", "device_id": "d1" }),
            json!({ "space_id": "s1", "root": "/Work", "server_url": "u" }),
        ] {
            assert_eq!(read_as(unusable.to_string().as_bytes()), None, "{unusable}");
        }
        assert_eq!(read_as(b"{\"space_id\":"), None);
    }
}
