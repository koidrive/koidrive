// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::api::Jurisdiction;
use crate::secret::Secret;

pub const SERVER_TAG: &str = "koidrive-server";

pub const RECOVERY_KEY_SECRET: &str = "KOIDRIVE_RECOVERY_KEY";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The server version, such as `0.3.0`.
    pub version: String,
    /// The `schema_version` each Durable Object class reaches on this server's code (RFC 0025).
    pub storage: Storage,
    /// The file name of the Worker module under `worker/`.
    pub main_module: String,
    pub compatibility_date: String,
    pub(crate) durable_objects: Vec<DurableObject>,
    /// The name of the R2 bucket binding, such as `BUCKET`.
    pub r2_binding: String,
    /// The bucket a new deployment creates, such as `koidrive`.
    pub default_bucket: String,
    pub(crate) migrations: Vec<Migration>,
    pub(crate) assets: Option<AssetsBinding>,
    pub(crate) observability: Option<Map<String, Value>>,
    pub(crate) code_update_strategy: Option<Map<String, Value>>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub account: u32,
    pub space: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DurableObject {
    name: String,
    class_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Migration {
    tag: String,
    #[serde(flatten)]
    step: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetsBinding {
    binding: String,
}

/// One static asset, with its path under the assets root such as `/viewer/index.html`.
#[derive(Clone, Debug)]
pub struct Asset {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Bundle {
    pub manifest: Manifest,
    pub module: Vec<u8>,
    pub assets: Vec<Asset>,
    /// `assets/_headers`, which the upload sends as asset configuration instead of as a file.
    pub headers: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("could not read {} from the server bundle", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not a server bundle manifest that this koi release can read", path.display())]
    Manifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("the asset {} has a file name that is not UTF-8", path.display())]
    AssetName { path: PathBuf },
    #[error(
        "the manifest names the Worker module {name:?}, which is not a file name inside worker/"
    )]
    ModulePath { name: String },
    #[error("{} in the server bundle is not a regular file or directory; a bundle holds no links", path.display())]
    NotRegular { path: PathBuf },
}

impl Bundle {
    /// Reads `manifest.json`, `worker/<main_module>` and every file under `assets/` in `dir`. A bundle without an
    /// `assets/` directory has no assets. Nothing outside `dir` is read: the module must be a plain file name, and a
    /// symbolic link or any other entry that is not a regular file or directory stops the read.
    pub fn from_dir(dir: &Path) -> Result<Self, BundleError> {
        let manifest_path = dir.join("manifest.json");
        let manifest: Manifest =
            serde_json::from_slice(&read(&manifest_path)?).map_err(|source| {
                BundleError::Manifest {
                    path: manifest_path,
                    source,
                }
            })?;
        let mut components = Path::new(&manifest.main_module).components();
        if !matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        ) {
            return Err(BundleError::ModulePath {
                name: manifest.main_module,
            });
        }
        let worker = dir.join("worker");
        if fs::symlink_metadata(&worker).is_ok() {
            directory(&worker)?;
        }
        let module = read(&worker.join(&manifest.main_module))?;
        let root = dir.join("assets");
        let mut assets = Vec::new();
        if fs::symlink_metadata(&root).is_ok() {
            directory(&root)?;
            collect_assets(&root, "", &mut assets)?;
        }
        assets.sort_by(|a, b| a.path.cmp(&b.path));
        let headers = match assets.iter().position(|asset| asset.path == "/_headers") {
            Some(index) => {
                let bytes = assets.remove(index).bytes;
                Some(String::from_utf8(bytes).map_err(|error| BundleError::Read {
                    path: root.join("_headers"),
                    source: io::Error::new(io::ErrorKind::InvalidData, error),
                })?)
            }
            None => None,
        };
        Ok(Bundle {
            manifest,
            module,
            assets,
            headers,
        })
    }
}

fn read(path: &Path) -> Result<Vec<u8>, BundleError> {
    let unreadable = |source| BundleError::Read {
        path: path.to_owned(),
        source,
    };
    if !fs::symlink_metadata(path).map_err(unreadable)?.is_file() {
        return Err(BundleError::NotRegular {
            path: path.to_owned(),
        });
    }
    fs::read(path).map_err(unreadable)
}

fn directory(path: &Path) -> Result<(), BundleError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| BundleError::Read {
        path: path.to_owned(),
        source,
    })?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(BundleError::NotRegular {
            path: path.to_owned(),
        })
    }
}

fn collect_assets(dir: &Path, prefix: &str, assets: &mut Vec<Asset>) -> Result<(), BundleError> {
    let unreadable = |source| BundleError::Read {
        path: dir.to_owned(),
        source,
    };
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(BundleError::AssetName { path });
        };
        let asset_path = format!("{prefix}/{name}");
        let kind = entry.file_type().map_err(unreadable)?;
        if kind.is_dir() {
            collect_assets(&path, &asset_path, assets)?;
        } else if kind.is_file() {
            assets.push(Asset {
                bytes: read(&path)?,
                path: asset_path,
            });
        } else {
            return Err(BundleError::NotRegular { path });
        }
    }
    Ok(())
}

/// The metadata and module of one script upload.
pub struct UploadPlan {
    pub(crate) metadata: Metadata,
    pub(crate) main_module: String,
    pub(crate) module: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "the deployed server is at storage migration {deployed_tag:?}, which this koi release does not know, so the \
     deployment is newer than this CLI; install a newer koi and run the command again"
)]
pub struct NewerDeployment {
    pub deployed_tag: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Metadata {
    main_module: String,
    compatibility_date: String,
    bindings: Vec<Binding>,
    tags: [&'static str; 1],
    keep_bindings: [&'static str; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    migrations: Option<Migrations>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observability: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_update_strategy: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assets: Option<Assets>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Binding {
    DurableObjectNamespace {
        name: String,
        class_name: String,
    },
    R2Bucket {
        name: String,
        bucket_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        jurisdiction: Option<Jurisdiction>,
    },
    SecretText {
        name: &'static str,
        #[serde(serialize_with = "expose")]
        text: Secret,
    },
    Assets {
        name: String,
    },
}

#[derive(Debug, Serialize)]
struct Migrations {
    #[serde(skip_serializing_if = "Option::is_none")]
    old_tag: Option<String>,
    new_tag: String,
    steps: Vec<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
struct Assets {
    #[serde(serialize_with = "expose")]
    jwt: Secret,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<AssetsConfig>,
}

#[derive(Debug, Serialize)]
struct AssetsConfig {
    #[serde(rename = "_headers")]
    headers: String,
}

fn expose<S: Serializer>(secret: &Secret, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(secret.expose())
}

impl UploadPlan {
    /// `bucket` is the R2 bucket to bind, in `jurisdiction`, which an upgrade keeps from the deployed binding.
    /// `deployed_tag` is the script's current migration tag, if it exists.
    /// `secret` is a one-time `KOIDRIVE_RECOVERY_KEY` sent in the same upload. `assets_jwt` is the completion token of
    /// the asset upload, which a bundle with assets needs.
    pub fn new(
        bundle: &Bundle,
        bucket: &str,
        jurisdiction: Jurisdiction,
        deployed_tag: Option<&str>,
        secret: Option<&Secret>,
        assets_jwt: Option<&Secret>,
    ) -> Result<Self, NewerDeployment> {
        let manifest = &bundle.manifest;
        let mut bindings: Vec<Binding> = manifest
            .durable_objects
            .iter()
            .map(|object| Binding::DurableObjectNamespace {
                name: object.name.clone(),
                class_name: object.class_name.clone(),
            })
            .collect();
        bindings.push(Binding::R2Bucket {
            name: manifest.r2_binding.clone(),
            bucket_name: bucket.to_owned(),
            jurisdiction: (jurisdiction != Jurisdiction::Default).then_some(jurisdiction),
        });
        if let Some(secret) = secret {
            bindings.push(Binding::SecretText {
                name: RECOVERY_KEY_SECRET,
                text: secret.clone(),
            });
        }
        if let Some(assets) = &manifest.assets {
            bindings.push(Binding::Assets {
                name: assets.binding.clone(),
            });
        }
        let metadata = Metadata {
            main_module: manifest.main_module.clone(),
            compatibility_date: manifest.compatibility_date.clone(),
            bindings,
            tags: [SERVER_TAG],
            keep_bindings: ["secret_text", "secret_key"],
            migrations: migrations(&manifest.migrations, deployed_tag)?,
            observability: manifest.observability.clone(),
            code_update_strategy: manifest.code_update_strategy.clone(),
            assets: assets_jwt.map(|jwt| Assets {
                jwt: jwt.clone(),
                config: bundle
                    .headers
                    .clone()
                    .map(|headers| AssetsConfig { headers }),
            }),
        };
        Ok(UploadPlan {
            metadata,
            main_module: manifest.main_module.clone(),
            module: bundle.module.clone(),
        })
    }
}

/// Wrangler's rule: a script without a tag gets every step, a script at an earlier tag gets the steps after it, and a
/// script at the last tag gets none.
fn migrations(
    list: &[Migration],
    deployed_tag: Option<&str>,
) -> Result<Option<Migrations>, NewerDeployment> {
    let (old_tag, pending) = match deployed_tag {
        None => (None, list),
        Some(tag) => {
            let index = list
                .iter()
                .position(|migration| migration.tag == tag)
                .ok_or_else(|| NewerDeployment {
                    deployed_tag: tag.to_owned(),
                })?;
            (Some(tag.to_owned()), &list[index + 1..])
        }
    };
    let Some(last) = pending.last() else {
        return Ok(None);
    };
    Ok(Some(Migrations {
        old_tag,
        new_tag: last.tag.clone(),
        steps: pending
            .iter()
            .map(|migration| migration.step.clone())
            .collect(),
    }))
}

impl fmt::Debug for UploadPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadPlan")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn manifest() -> Value {
        json!({
            "version": "0.3.0",
            "storage": { "account": 10, "space": 10 },
            "main_module": "worker.js",
            "compatibility_date": "2026-11-01",
            "durable_objects": [
                { "name": "ACCOUNT", "class_name": "AccountDO" },
                { "name": "SPACE", "class_name": "SpaceDO" }
            ],
            "r2_binding": "BUCKET",
            "default_bucket": "koidrive",
            "migrations": [
                { "tag": "v1", "new_sqlite_classes": ["AccountDO", "SpaceDO"] },
                { "tag": "v2", "renamed_classes": [{ "from": "SpaceDO", "to": "Space" }] },
                { "tag": "v3", "deleted_classes": ["Old"] }
            ],
            "assets": { "binding": "ASSETS" },
            "observability": { "enabled": true, "redact_query_string": true },
            "code_update_strategy": { "mode": "deferred", "max_delay": 300 }
        })
    }

    fn bundle() -> Bundle {
        Bundle {
            manifest: serde_json::from_value(manifest()).unwrap(),
            module: b"export default {};".to_vec(),
            assets: Vec::new(),
            headers: Some("/viewer/*\n  X-Robots-Tag: noindex\n".to_owned()),
        }
    }

    fn metadata(
        deployed_tag: Option<&str>,
        secret: Option<&str>,
        assets_jwt: Option<&str>,
    ) -> Value {
        let secret = secret.map(|value| Secret::new(value.to_owned()));
        let assets_jwt = assets_jwt.map(|value| Secret::new(value.to_owned()));
        let plan = UploadPlan::new(
            &bundle(),
            "my-bucket",
            Jurisdiction::Default,
            deployed_tag,
            secret.as_ref(),
            assets_jwt.as_ref(),
        )
        .unwrap();
        serde_json::to_value(&plan.metadata).unwrap()
    }

    #[test]
    fn a_new_script_gets_every_migration_the_secret_and_the_assets() {
        assert_eq!(
            metadata(None, Some("one-time-key"), Some("completion-jwt")),
            json!({
                "main_module": "worker.js",
                "compatibility_date": "2026-11-01",
                "bindings": [
                    { "type": "durable_object_namespace", "name": "ACCOUNT", "class_name": "AccountDO" },
                    { "type": "durable_object_namespace", "name": "SPACE", "class_name": "SpaceDO" },
                    { "type": "r2_bucket", "name": "BUCKET", "bucket_name": "my-bucket" },
                    { "type": "secret_text", "name": "KOIDRIVE_RECOVERY_KEY", "text": "one-time-key" },
                    { "type": "assets", "name": "ASSETS" }
                ],
                "tags": ["koidrive-server"],
                "keep_bindings": ["secret_text", "secret_key"],
                "migrations": {
                    "new_tag": "v3",
                    "steps": [
                        { "new_sqlite_classes": ["AccountDO", "SpaceDO"] },
                        { "renamed_classes": [{ "from": "SpaceDO", "to": "Space" }] },
                        { "deleted_classes": ["Old"] }
                    ]
                },
                "observability": { "enabled": true, "redact_query_string": true },
                "code_update_strategy": { "mode": "deferred", "max_delay": 300 },
                "assets": {
                    "jwt": "completion-jwt",
                    "config": { "_headers": "/viewer/*\n  X-Robots-Tag: noindex\n" }
                }
            })
        );
    }

    #[test]
    fn a_script_at_an_earlier_tag_gets_the_steps_after_it() {
        let metadata = metadata(Some("v1"), None, None);
        assert_eq!(
            metadata["migrations"],
            json!({
                "old_tag": "v1",
                "new_tag": "v3",
                "steps": [
                    { "renamed_classes": [{ "from": "SpaceDO", "to": "Space" }] },
                    { "deleted_classes": ["Old"] }
                ]
            })
        );
        assert_eq!(
            metadata["bindings"],
            json!([
                { "type": "durable_object_namespace", "name": "ACCOUNT", "class_name": "AccountDO" },
                { "type": "durable_object_namespace", "name": "SPACE", "class_name": "SpaceDO" },
                { "type": "r2_bucket", "name": "BUCKET", "bucket_name": "my-bucket" },
                { "type": "assets", "name": "ASSETS" }
            ])
        );
        assert_eq!(metadata.get("assets"), None);
    }

    #[test]
    fn a_script_at_the_last_tag_gets_no_migrations() {
        assert_eq!(metadata(Some("v3"), None, None).get("migrations"), None);
    }

    #[test]
    fn an_unknown_deployed_tag_means_the_deployment_is_newer() {
        let error = UploadPlan::new(
            &bundle(),
            "my-bucket",
            Jurisdiction::Default,
            Some("v9"),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.deployed_tag, "v9");
        assert!(error.to_string().contains("newer than this CLI"), "{error}");
    }

    #[test]
    fn a_manifest_without_assets_binds_none() {
        let mut manifest = manifest();
        let object = manifest.as_object_mut().unwrap();
        object.remove("assets");
        object.remove("observability");
        object.remove("code_update_strategy");
        let bundle = Bundle {
            manifest: serde_json::from_value(manifest).unwrap(),
            headers: None,
            ..bundle()
        };
        let plan = UploadPlan::new(
            &bundle,
            "my-bucket",
            Jurisdiction::Default,
            None,
            None,
            None,
        )
        .unwrap();
        let metadata = serde_json::to_value(&plan.metadata).unwrap();
        assert!(
            metadata["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|binding| binding["type"] != "assets")
        );
        assert_eq!(metadata.get("observability"), None);
        assert_eq!(metadata.get("code_update_strategy"), None);
    }

    #[test]
    fn debug_output_hides_the_secret_and_the_assets_token() {
        let secret = Secret::new("one-time-key".to_owned());
        let jwt = Secret::new("completion-jwt".to_owned());
        let plan = UploadPlan::new(
            &bundle(),
            "my-bucket",
            Jurisdiction::Default,
            None,
            Some(&secret),
            Some(&jwt),
        )
        .unwrap();
        let debug = format!("{plan:?}");
        assert!(
            !debug.contains("one-time-key") && !debug.contains("completion-jwt"),
            "{debug}"
        );
    }

    #[test]
    fn a_manifest_with_an_unknown_field_is_refused() {
        let mut manifest = manifest();
        manifest["compatibility_flags"] = json!(["nodejs_compat"]);
        let error = serde_json::from_value::<Manifest>(manifest).unwrap_err();
        assert!(error.to_string().contains("compatibility_flags"), "{error}");
    }

    #[test]
    fn from_dir_reads_the_manifest_the_module_and_the_assets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("manifest.json"), manifest().to_string()).unwrap();
        fs::create_dir_all(root.join("worker")).unwrap();
        fs::write(root.join("worker/worker.js"), "export default {};").unwrap();
        fs::create_dir_all(root.join("assets/viewer")).unwrap();
        fs::write(
            root.join("assets/_headers"),
            "/viewer/*\n  X-Robots-Tag: noindex\n",
        )
        .unwrap();
        fs::write(root.join("assets/viewer/index.html"), "<!doctype html>").unwrap();
        fs::write(root.join("assets/viewer/viewer.js"), "console.log(1);").unwrap();

        let bundle = Bundle::from_dir(root).unwrap();

        assert_eq!(bundle.manifest.version, "0.3.0");
        assert_eq!(bundle.module, b"export default {};");
        let assets: Vec<(&str, &[u8])> = bundle
            .assets
            .iter()
            .map(|a| (a.path.as_str(), a.bytes.as_slice()))
            .collect();
        assert_eq!(
            assets,
            [
                ("/viewer/index.html", &b"<!doctype html>"[..]),
                ("/viewer/viewer.js", &b"console.log(1);"[..])
            ]
        );
        assert_eq!(
            bundle.headers.as_deref(),
            Some("/viewer/*\n  X-Robots-Tag: noindex\n")
        );
    }

    #[test]
    fn from_dir_without_assets_has_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("manifest.json"), manifest().to_string()).unwrap();
        fs::create_dir_all(dir.path().join("worker")).unwrap();
        fs::write(dir.path().join("worker/worker.js"), "export default {};").unwrap();

        let bundle = Bundle::from_dir(dir.path()).unwrap();

        assert!(bundle.assets.is_empty());
        assert_eq!(bundle.headers, None);
    }

    #[test]
    fn an_upgrade_keeps_the_jurisdiction_of_the_bucket() {
        let plan = UploadPlan::new(
            &bundle(),
            "my-bucket",
            Jurisdiction::Eu,
            Some("v3"),
            None,
            None,
        )
        .unwrap();
        let metadata = serde_json::to_value(&plan.metadata).unwrap();
        assert_eq!(
            metadata["bindings"][2],
            json!({ "type": "r2_bucket", "name": "BUCKET", "bucket_name": "my-bucket", "jurisdiction": "eu" })
        );
    }

    fn bundle_dir(main_module: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bundle");
        let mut manifest = manifest();
        manifest["main_module"] = json!(main_module);
        fs::create_dir_all(root.join("worker")).unwrap();
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("manifest.json"), manifest.to_string()).unwrap();
        fs::write(root.join("worker/worker.js"), "export default {};").unwrap();
        fs::write(dir.path().join("outside.txt"), "outside").unwrap();
        dir
    }

    #[test]
    fn a_module_path_that_leaves_worker_is_refused() {
        for name in [
            "../../outside.txt",
            "../manifest.json",
            "/etc/hosts",
            "sub/worker.js",
            "",
        ] {
            let dir = bundle_dir(name);
            let error = Bundle::from_dir(&dir.path().join("bundle")).unwrap_err();
            assert!(
                matches!(&error, BundleError::ModulePath { name: refused } if refused == name),
                "{name}: {error:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_in_the_bundle_are_refused() {
        use std::os::unix::fs::symlink;
        let dir = bundle_dir("worker.js");
        let root = dir.path().join("bundle");
        let outside = dir.path().join("outside.txt");
        symlink(&outside, root.join("assets/leak.txt")).unwrap();
        let error = Bundle::from_dir(&root).unwrap_err();
        assert!(
            matches!(&error, BundleError::NotRegular { path } if path.ends_with("assets/leak.txt")),
            "{error:?}"
        );

        let dir = bundle_dir("worker.js");
        let root = dir.path().join("bundle");
        fs::remove_file(root.join("worker/worker.js")).unwrap();
        symlink(
            dir.path().join("outside.txt"),
            root.join("worker/worker.js"),
        )
        .unwrap();
        let error = Bundle::from_dir(&root).unwrap_err();
        assert!(
            matches!(&error, BundleError::NotRegular { path } if path.ends_with("worker/worker.js")),
            "{error:?}"
        );

        let dir = bundle_dir("worker.js");
        let root = dir.path().join("bundle");
        fs::remove_dir_all(root.join("assets")).unwrap();
        fs::create_dir(dir.path().join("elsewhere")).unwrap();
        fs::write(dir.path().join("elsewhere/page.html"), "outside").unwrap();
        symlink(dir.path().join("elsewhere"), root.join("assets")).unwrap();
        let error = Bundle::from_dir(&root).unwrap_err();
        assert!(
            matches!(&error, BundleError::NotRegular { path } if path.ends_with("bundle/assets")),
            "{error:?}"
        );
    }

    #[test]
    fn from_dir_names_the_missing_module() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("manifest.json"), manifest().to_string()).unwrap();

        let error = Bundle::from_dir(dir.path()).unwrap_err();

        assert!(
            matches!(&error, BundleError::Read { path, .. } if path.ends_with("worker/worker.js")),
            "{error:?}"
        );
    }
}
