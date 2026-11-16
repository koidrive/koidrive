// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::multipart::{Form, Part};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::bundle::{Asset, UploadPlan};
use crate::secret::Secret;

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

const R2_MAX_PAGE_SIZE: usize = 1000;

const ACCOUNTS_MAX_PER_PAGE: usize = 50;

const MAX_ACCOUNT_PAGES_READ: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CloudflareMessage {
    pub code: u64,
    pub message: String,
}

impl fmt::Display for CloudflareMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [code: {}]", self.message, self.code)
    }
}

/// A failed API call. No variant holds a request's body or headers, so an error never carries a token or a secret.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("could not {call}: the Cloudflare API did not answer")]
    Transport {
        call: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("could not {call}: Cloudflare answered HTTP {status}{}", messages(.errors))]
    Failed {
        call: &'static str,
        status: u16,
        errors: Vec<CloudflareMessage>,
    },
    #[error("could not {call}: {detail}; a newer koi release may read Cloudflare's answer")]
    Unreadable {
        call: &'static str,
        detail: &'static str,
    },
    #[error("{name:?} cannot be used as a name in a Cloudflare API path")]
    Name { name: String },
}

fn messages(errors: &[CloudflareMessage]) -> String {
    errors.iter().map(|error| format!(", {error}")).collect()
}

impl ApiError {
    /// The HTTP status of a call that Cloudflare refused.
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Failed { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Whether Cloudflare refused the call with this error code, such as `10042` when R2 is not enabled.
    pub fn has_code(&self, code: u64) -> bool {
        matches!(self, ApiError::Failed { errors, .. } if errors.iter().any(|error| error.code == code))
    }
}

/// An R2 jurisdiction. R2 allows buckets with one name in several jurisdictions, so every R2 call names one
/// ([Data location](https://developers.cloudflare.com/r2/reference/data-location/)). A value this release does not know
/// fails to parse, so a caller never acts on a bucket in a jurisdiction it cannot name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Jurisdiction {
    #[default]
    Default,
    Eu,
    Us,
    Fedramp,
    FedrampHigh,
}

impl Jurisdiction {
    /// The value of the `cf-r2-jurisdiction` header.
    pub fn as_str(self) -> &'static str {
        match self {
            Jurisdiction::Default => "default",
            Jurisdiction::Eu => "eu",
            Jurisdiction::Us => "us",
            Jurisdiction::Fedramp => "fedramp",
            Jurisdiction::FedrampHigh => "fedramp-high",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
}

/// A Worker script as `GET /workers/scripts` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Script {
    pub id: String,
    /// When the script was created. A script deleted and created again under the same name has a new time, so this
    /// tells the two apart; `None` when Cloudflare does not send it, which leaves the script's identity unknown.
    pub created_on: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub tags: Vec<String>,
    pub migration_tag: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub has_assets: bool,
}

/// `GET /workers/scripts/<name>/settings`.
#[derive(Clone, Debug, Deserialize)]
pub struct ScriptSettings {
    #[serde(default, deserialize_with = "null_as_default")]
    pub bindings: Vec<Binding>,
}

impl ScriptSettings {
    /// The R2 binding named `binding`, such as `BUCKET`, if it names a bucket.
    pub fn r2_bucket(&self, binding: &str) -> Option<&Binding> {
        self.bindings.iter().find(|candidate| {
            candidate.kind == "r2_bucket"
                && candidate.name == binding
                && candidate.bucket_name.is_some()
        })
    }
}

/// One binding of a deployed script. Cloudflare never returns a secret's value here.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Binding {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub bucket_name: Option<String>,
    /// The class of a Durable Object namespace binding, such as `AccountDO`.
    pub class_name: Option<String>,
    /// The jurisdiction of an R2 binding's bucket, `None` when the binding names none.
    pub jurisdiction: Option<Jurisdiction>,
}

/// An R2 bucket as `GET /r2/buckets/<bucket>` describes it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Bucket {
    pub name: String,
    /// When the bucket was created. A bucket deleted and created again under the same name has a new date.
    pub creation_date: String,
    pub jurisdiction: Option<Jurisdiction>,
}

/// One page of `GET /r2/buckets/<bucket>/objects`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectPage {
    pub keys: Vec<String>,
    /// The cursor of the next page, or `None` on the last page.
    pub cursor: Option<String>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    errors: Vec<CloudflareMessage>,
    #[serde(default)]
    result: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    result_info: ResultInfo,
}

#[derive(Default, Deserialize)]
struct ResultInfo {
    cursor: Option<String>,
    is_truncated: Option<bool>,
}

fn null_as_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A client for one access token. `base` is `https://api.cloudflare.com/client/v4/`.
#[derive(Clone, Debug)]
pub struct Cloudflare {
    http: reqwest::Client,
    base: Url,
    token: Secret,
}

impl Cloudflare {
    pub fn new(http: reqwest::Client, base: Url, token: Secret) -> Self {
        Cloudflare { http, base, token }
    }

    /// The accounts the token can reach. An OAuth grant reaches only the account picked on the consent page.
    pub async fn accounts(&self) -> Result<Vec<Account>, ApiError> {
        const CALL: &str = "list accounts";
        let mut accounts = Vec::new();
        for page in 1..=MAX_ACCOUNT_PAGES_READ {
            let mut url = self.url(&["accounts"])?;
            url.query_pairs_mut()
                .append_pair("page", &page.to_string())
                .append_pair("per_page", &ACCOUNTS_MAX_PER_PAGE.to_string());
            let (batch, _): (Vec<Account>, _) =
                self.send(CALL, self.request(Method::GET, url)).await?;
            let last = batch.len() < ACCOUNTS_MAX_PER_PAGE;
            accounts.extend(batch);
            if last {
                return Ok(accounts);
            }
        }
        Err(ApiError::Unreadable {
            call: CALL,
            detail: "Cloudflare kept sending full pages of accounts",
        })
    }

    /// The names of the account's R2 buckets. Error `10042` means R2 is not enabled on the account (section 4.1).
    pub async fn buckets(
        &self,
        account: &str,
        jurisdiction: Jurisdiction,
    ) -> Result<Vec<String>, ApiError> {
        #[derive(Deserialize)]
        struct Buckets {
            buckets: Vec<Named>,
        }
        #[derive(Deserialize)]
        struct Named {
            name: String,
        }
        let mut names = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut url = self.url(&["accounts", account, "r2", "buckets"])?;
            url.query_pairs_mut()
                .append_pair("per_page", &R2_MAX_PAGE_SIZE.to_string());
            if let Some(cursor) = &cursor {
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            let (page, info): (Buckets, _) = self
                .send("list R2 buckets", self.r2(Method::GET, url, jurisdiction))
                .await?;
            let full = page.buckets.len() == R2_MAX_PAGE_SIZE;
            names.extend(page.buckets.into_iter().map(|bucket| bucket.name));
            let next = info.cursor.filter(|next| full && !next.is_empty());
            if next.is_some() && next == cursor {
                return Err(ApiError::Unreadable {
                    call: "list R2 buckets",
                    detail: "Cloudflare sent the same cursor again",
                });
            }
            cursor = next;
            if cursor.is_none() {
                return Ok(names);
            }
        }
    }

    pub async fn create_bucket(
        &self,
        account: &str,
        bucket: &str,
        jurisdiction: Jurisdiction,
    ) -> Result<(), ApiError> {
        let url = self.url(&["accounts", account, "r2", "buckets"])?;
        let request = self
            .r2(Method::POST, url, jurisdiction)
            .json(&json!({ "name": bucket }));
        self.send::<IgnoredAny>("create an R2 bucket", request)
            .await
            .map(drop)
    }

    /// The bucket's creation date and jurisdiction. A bucket that does not exist is an `ApiError::Failed` with
    /// status 404.
    pub async fn bucket(
        &self,
        account: &str,
        bucket: &str,
        jurisdiction: Jurisdiction,
    ) -> Result<Bucket, ApiError> {
        let url = self.url(&["accounts", account, "r2", "buckets", bucket])?;
        Ok(self
            .send("read an R2 bucket", self.r2(Method::GET, url, jurisdiction))
            .await?
            .0)
    }

    /// Deletes an empty bucket. A bucket that does not exist counts as deleted.
    pub async fn delete_bucket(
        &self,
        account: &str,
        bucket: &str,
        jurisdiction: Jurisdiction,
    ) -> Result<(), ApiError> {
        let url = self.url(&["accounts", account, "r2", "buckets", bucket])?;
        self.delete(
            "delete an R2 bucket",
            self.r2(Method::DELETE, url, jurisdiction),
        )
        .await
    }

    /// One page of up to 1000 object keys, starting at `cursor` from the previous page.
    pub async fn objects(
        &self,
        account: &str,
        bucket: &str,
        jurisdiction: Jurisdiction,
        cursor: Option<&str>,
    ) -> Result<ObjectPage, ApiError> {
        #[derive(Deserialize)]
        struct Object {
            key: String,
        }
        let mut url = self.url(&["accounts", account, "r2", "buckets", bucket, "objects"])?;
        url.query_pairs_mut()
            .append_pair("per_page", &R2_MAX_PAGE_SIZE.to_string());
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        const CALL: &str = "list the objects in an R2 bucket";
        let (objects, info): (Vec<Object>, _) = self
            .send(CALL, self.r2(Method::GET, url, jurisdiction))
            .await?;
        let next = info.cursor.filter(|cursor| !cursor.is_empty());
        let cursor = match (info.is_truncated, next) {
            (Some(true), None) => {
                return Err(ApiError::Unreadable {
                    call: CALL,
                    detail: "Cloudflare said the list goes on but sent no cursor",
                });
            }
            (Some(false), _) => None,
            (_, next) if next.is_some() && next.as_deref() == cursor => {
                return Err(ApiError::Unreadable {
                    call: CALL,
                    detail: "Cloudflare sent the same cursor again",
                });
            }
            (_, next) => next,
        };
        Ok(ObjectPage {
            keys: objects.into_iter().map(|object| object.key).collect(),
            cursor,
        })
    }

    /// Deletes one object. An object that does not exist counts as deleted.
    pub async fn delete_object(
        &self,
        account: &str,
        bucket: &str,
        jurisdiction: Jurisdiction,
        key: &str,
    ) -> Result<(), ApiError> {
        let url = self.url(&["accounts", account, "r2", "buckets", bucket, "objects", key])?;
        self.delete(
            "delete an object in an R2 bucket",
            self.r2(Method::DELETE, url, jurisdiction),
        )
        .await
    }

    pub async fn scripts(&self, account: &str) -> Result<Vec<Script>, ApiError> {
        let url = self.url(&["accounts", account, "workers", "scripts"])?;
        Ok(self
            .send("list Workers", self.request(Method::GET, url))
            .await?
            .0)
    }

    pub async fn script_settings(
        &self,
        account: &str,
        script: &str,
    ) -> Result<ScriptSettings, ApiError> {
        let url = self.url(&[
            "accounts", account, "workers", "scripts", script, "settings",
        ])?;
        Ok(self
            .send("read a Worker's settings", self.request(Method::GET, url))
            .await?
            .0)
    }

    /// Uploads the assets that Cloudflare does not hold yet and returns the completion token for `UploadPlan::new`.
    ///
    /// A file's hash is the first 32 hex digits of the SHA-256 of its base64 contents followed by its extension.
    pub async fn upload_assets(
        &self,
        account: &str,
        script: &str,
        assets: &[Asset],
    ) -> Result<Secret, ApiError> {
        /// An empty list is what makes `jwt` a completion token.
        #[derive(Deserialize)]
        struct Session {
            jwt: String,
            buckets: Vec<Vec<String>>,
        }
        #[derive(Deserialize)]
        struct Uploaded {
            jwt: Option<String>,
        }
        const CALL: &str = "upload the server's static assets";
        let hashed: Vec<(String, &Asset)> = assets
            .iter()
            .map(|asset| (asset_hash(asset), asset))
            .collect();
        let manifest: BTreeMap<&str, Value> = hashed
            .iter()
            .map(|(hash, asset)| {
                (
                    asset.path.as_str(),
                    json!({ "hash": hash, "size": asset.bytes.len() }),
                )
            })
            .collect();
        let by_hash: HashMap<&str, &Asset> = hashed
            .iter()
            .map(|(hash, asset)| (hash.as_str(), *asset))
            .collect();
        let url = self.url(&[
            "accounts",
            account,
            "workers",
            "scripts",
            script,
            "assets-upload-session",
        ])?;
        let request = self
            .request(Method::POST, url)
            .json(&json!({ "manifest": manifest }));
        let (session, _): (Session, _) = self.send(CALL, request).await?;
        let upload_token = Secret::new(session.jwt);
        if session.buckets.is_empty() {
            return Ok(upload_token);
        }
        let mut url = self.url(&["accounts", account, "workers", "assets", "upload"])?;
        url.query_pairs_mut().append_pair("base64", "true");
        let mut completion = None;
        for bucket in session.buckets {
            let mut form = Form::new();
            for hash in bucket {
                let asset = by_hash.get(hash.as_str()).ok_or(ApiError::Unreadable {
                    call: CALL,
                    detail: "Cloudflare asked for a file that the upload did not list",
                })?;
                let part = Part::text(STANDARD.encode(&asset.bytes))
                    .file_name(hash.clone())
                    .mime_str(content_type(&asset.path))
                    .expect("the content type table holds valid MIME types");
                form = form.part(hash, part);
            }
            let request = self
                .http
                .post(url.clone())
                .bearer_auth(upload_token.expose())
                .multipart(form);
            let (uploaded, _): (Option<Uploaded>, _) = self.send(CALL, request).await?;
            if let Some(jwt) = uploaded.and_then(|uploaded| uploaded.jwt) {
                completion = Some(Secret::new(jwt));
            }
        }
        completion.ok_or(ApiError::Unreadable {
            call: CALL,
            detail: "Cloudflare took every file but sent no completion token",
        })
    }

    /// Uploads the Worker module with the plan's metadata, creating the script or replacing its code.
    pub async fn upload_script(
        &self,
        account: &str,
        script: &str,
        plan: &UploadPlan,
    ) -> Result<Script, ApiError> {
        let metadata =
            serde_json::to_string(&plan.metadata).expect("upload metadata serializes to JSON");
        let form = Form::new()
            .part(
                "metadata",
                Part::text(metadata)
                    .mime_str("application/json")
                    .expect("a valid MIME type"),
            )
            .part(
                plan.main_module.clone(),
                Part::bytes(plan.module.clone())
                    .file_name(plan.main_module.clone())
                    .mime_str("application/javascript+module")
                    .expect("a valid MIME type"),
            );
        let url = self.url(&["accounts", account, "workers", "scripts", script])?;
        let request = self.request(Method::PUT, url).multipart(form);
        Ok(self.send("upload the Koidrive server", request).await?.0)
    }

    /// The names of the script's secrets.
    pub async fn secrets(&self, account: &str, script: &str) -> Result<Vec<String>, ApiError> {
        #[derive(Deserialize)]
        struct Named {
            name: String,
        }
        let url = self.url(&["accounts", account, "workers", "scripts", script, "secrets"])?;
        let (secrets, _): (Vec<Named>, _) = self
            .send("list a Worker's secrets", self.request(Method::GET, url))
            .await?;
        Ok(secrets.into_iter().map(|secret| secret.name).collect())
    }

    pub async fn put_secret(
        &self,
        account: &str,
        script: &str,
        name: &str,
        value: &Secret,
    ) -> Result<(), ApiError> {
        let url = self.url(&["accounts", account, "workers", "scripts", script, "secrets"])?;
        let body = json!({ "name": name, "text": value.expose(), "type": "secret_text" });
        let request = self.request(Method::PUT, url).json(&body);
        self.send::<IgnoredAny>("set a Worker secret", request)
            .await
            .map(drop)
    }

    /// Deletes a secret. A secret that does not exist counts as deleted.
    pub async fn delete_secret(
        &self,
        account: &str,
        script: &str,
        name: &str,
    ) -> Result<(), ApiError> {
        let url = self.url(&[
            "accounts", account, "workers", "scripts", script, "secrets", name,
        ])?;
        self.delete("delete a Worker secret", self.request(Method::DELETE, url))
            .await
    }

    /// Serves the script on `<script>.<subdomain>.workers.dev`, without preview URLs.
    pub async fn enable_workers_dev(&self, account: &str, script: &str) -> Result<(), ApiError> {
        let url = self.url(&[
            "accounts",
            account,
            "workers",
            "scripts",
            script,
            "subdomain",
        ])?;
        let request = self
            .request(Method::POST, url)
            .json(&json!({ "enabled": true, "previews_enabled": false }));
        self.send::<IgnoredAny>("turn on the workers.dev URL", request)
            .await
            .map(drop)
    }

    /// The account's workers.dev subdomain, such as `alice` in `koidrive.alice.workers.dev`.
    pub async fn workers_subdomain(&self, account: &str) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct Subdomain {
            subdomain: String,
        }
        let url = self.url(&["accounts", account, "workers", "subdomain"])?;
        let (result, _): (Subdomain, _) = self
            .send(
                "read the workers.dev subdomain",
                self.request(Method::GET, url),
            )
            .await?;
        Ok(result.subdomain)
    }

    /// Registers the account's workers.dev subdomain.
    pub async fn create_workers_subdomain(
        &self,
        account: &str,
        subdomain: &str,
    ) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct Subdomain {
            subdomain: String,
        }
        let url = self.url(&["accounts", account, "workers", "subdomain"])?;
        let request = self
            .request(Method::PUT, url)
            .json(&json!({ "subdomain": subdomain }));
        let (result, _): (Subdomain, _) = self
            .send("create the workers.dev subdomain", request)
            .await?;
        Ok(result.subdomain)
    }

    /// Deletes the script and its Durable Object namespaces. A script that does not exist counts as deleted.
    pub async fn delete_script(&self, account: &str, script: &str) -> Result<(), ApiError> {
        let mut url = self.url(&["accounts", account, "workers", "scripts", script])?;
        url.query_pairs_mut().append_pair("force", "true");
        self.delete(
            "delete the Koidrive server",
            self.request(Method::DELETE, url),
        )
        .await
    }

    fn url(&self, segments: &[&str]) -> Result<Url, ApiError> {
        let mut path = self.base.path().trim_end_matches('/').to_owned();
        for segment in segments {
            if matches!(*segment, "" | "." | "..") {
                return Err(ApiError::Name {
                    name: (*segment).to_owned(),
                });
            }
            path.push('/');
            path.extend(utf8_percent_encode(segment, SEGMENT));
        }
        let mut url = self.base.clone();
        url.set_path(&path);
        Ok(url)
    }

    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(self.token.expose())
    }

    fn r2(&self, method: Method, url: Url, jurisdiction: Jurisdiction) -> RequestBuilder {
        self.request(method, url)
            .header("cf-r2-jurisdiction", jurisdiction.as_str())
    }

    async fn delete(&self, call: &'static str, request: RequestBuilder) -> Result<(), ApiError> {
        match self.send::<IgnoredAny>(call, request).await {
            Err(error) if error.status() == Some(StatusCode::NOT_FOUND.as_u16()) => Ok(()),
            result => result.map(drop),
        }
    }

    async fn send<T: DeserializeOwned>(
        &self,
        call: &'static str,
        request: RequestBuilder,
    ) -> Result<(T, ResultInfo), ApiError> {
        let transport = |source| ApiError::Transport { call, source };
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        let body = response.bytes().await.map_err(transport)?;
        let envelope = match serde_json::from_slice::<Envelope>(&body) {
            Ok(envelope) => envelope,
            Err(_) if !status.is_success() => {
                return Err(ApiError::Failed {
                    call,
                    status: status.as_u16(),
                    errors: Vec::new(),
                });
            }
            Err(_) => {
                return Err(ApiError::Unreadable {
                    call,
                    detail: "Cloudflare's answer is not an API envelope",
                });
            }
        };
        if !status.is_success() || !envelope.success {
            return Err(ApiError::Failed {
                call,
                status: status.as_u16(),
                errors: envelope.errors,
            });
        }
        let result = serde_json::from_value(envelope.result).map_err(|_| ApiError::Unreadable {
            call,
            detail: "Cloudflare's result does not have the expected fields",
        })?;
        Ok((result, envelope.result_info))
    }
}

fn asset_hash(asset: &Asset) -> String {
    let extension = Path::new(&asset.path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    let digest = Sha256::digest(format!("{}{extension}", STANDARD.encode(&asset.bytes)));
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `application/null` makes Cloudflare send no `Content-Type`.
fn content_type(path: &str) -> &'static str {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        _ => "application/null",
    }
}
