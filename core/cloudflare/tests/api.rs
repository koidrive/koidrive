// SPDX-License-Identifier: Apache-2.0

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use koidrive_cloudflare::{
    ApiError, Asset, Bucket, Bundle, Cloudflare, Jurisdiction, Manifest, ObjectPage, Script,
    Secret, UploadPlan,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use url::Url;

const TOKEN: &str = "test-access-token";
const ACCOUNT: &str = "0123456789abcdef";

struct Reply {
    method: &'static str,
    path: String,
    status: u16,
    body: String,
}

fn reply(method: &'static str, path: &str, status: u16, body: impl ToString) -> Reply {
    Reply {
        method,
        path: format!("/client/v4{path}"),
        status,
        body: body.to_string(),
    }
}

fn ok(result: Value) -> Value {
    json!({ "success": true, "errors": [], "messages": [], "result": result })
}

fn refused(code: u64, message: &str) -> Value {
    json!({ "success": false, "errors": [{ "code": code, "message": message }], "messages": [], "result": null })
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    content_type: Option<String>,
    jurisdiction: Option<String>,
    body: Vec<u8>,
}

impl Seen {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

struct Fake {
    base: Url,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    async fn start(replies: Vec<Reply>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!(
            "http://{}/client/v4/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let replies = Arc::new(Mutex::new(replies));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (replies, seen) = (replies.clone(), recorder.clone());
                let service =
                    service_fn(move |request| answer(request, replies.clone(), seen.clone()));
                tokio::spawn(http1::Builder::new().serve_connection(TokioIo::new(stream), service));
            }
        });
        Fake { base, seen }
    }

    fn client(&self) -> Cloudflare {
        Cloudflare::new(
            reqwest::Client::new(),
            self.base.clone(),
            Secret::new(TOKEN.to_owned()),
        )
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<(String, String)> {
        let seen = self.seen();
        for request in &seen {
            assert_eq!(
                request.authorization.as_deref(),
                Some(&*format!("Bearer {TOKEN}")),
                "{request:?}"
            );
        }
        seen.into_iter()
            .map(|request| {
                (
                    request.method,
                    request.path.trim_start_matches("/client/v4").to_owned(),
                )
            })
            .collect()
    }
}

async fn answer(
    request: Request<Incoming>,
    replies: Arc<Mutex<Vec<Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = request.method().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_default();
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    };
    let (authorization, content_type, jurisdiction) = (
        header("authorization"),
        header("content-type"),
        header("cf-r2-jurisdiction"),
    );
    let body = request
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    seen.lock().unwrap().push(Seen {
        method: method.clone(),
        path: path.clone(),
        authorization,
        content_type,
        jurisdiction,
        body,
    });
    let mut replies = replies.lock().unwrap();
    let (status, body) = match replies
        .iter()
        .position(|reply| reply.method == method && reply.path == path)
    {
        Some(index) => {
            let reply = replies.remove(index);
            (reply.status, reply.body)
        }
        None => (
            500,
            refused(1, &format!("the fake has no reply for {method} {path}")).to_string(),
        ),
    };
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

fn parts(request: &Seen) -> Vec<Part> {
    let content_type = request.content_type.as_deref().unwrap();
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .unwrap();
    let delimiter = format!("\r\n--{boundary}");
    let body = [b"\r\n".as_slice(), &request.body].concat();
    let mut sections = Vec::new();
    let mut rest = body.as_slice();
    while let Some(index) = find(rest, delimiter.as_bytes()) {
        sections.push(&rest[..index]);
        rest = &rest[index + delimiter.len()..];
    }
    assert_eq!(rest, b"--\r\n");
    sections
        .into_iter()
        .skip(1)
        .map(|section| {
            let section = section.strip_prefix(b"\r\n").unwrap();
            let split = find(section, b"\r\n\r\n").unwrap();
            let headers = String::from_utf8(section[..split].to_vec()).unwrap();
            let mut part = Part {
                name: String::new(),
                filename: None,
                content_type: None,
                body: section[split + 4..].to_vec(),
            };
            for line in headers.split("\r\n") {
                let (name, value) = line.split_once(": ").unwrap();
                match name.to_ascii_lowercase().as_str() {
                    "content-type" => part.content_type = Some(value.to_owned()),
                    "content-disposition" => {
                        for field in value.split("; ").skip(1) {
                            let (key, quoted) = field.split_once('=').unwrap();
                            let unquoted = quoted.trim_matches('"').to_owned();
                            match key {
                                "name" => part.name = unquoted,
                                "filename" => part.filename = Some(unquoted),
                                _ => panic!("unexpected field {field}"),
                            }
                        }
                    }
                    _ => {}
                }
            }
            part
        })
        .collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[tokio::test]
async fn accounts_are_listed_with_the_bearer_token() {
    let fake = Fake::start(vec![reply(
        "GET",
        "/accounts?page=1&per_page=50",
        200,
        ok(json!([{ "id": ACCOUNT, "name": "Alice's account", "type": "standard" }])),
    )])
    .await;

    let accounts = fake.client().accounts().await.unwrap();

    assert_eq!(accounts.len(), 1);
    assert_eq!(
        (accounts[0].id.as_str(), accounts[0].name.as_str()),
        (ACCOUNT, "Alice's account")
    );
    assert_eq!(
        fake.calls(),
        [("GET".to_owned(), "/accounts?page=1&per_page=50".to_owned())]
    );
}

#[tokio::test]
async fn buckets_follow_the_cursor_while_pages_are_full() {
    let full: Vec<Value> = (0..1000)
        .map(|i| json!({ "name": format!("bucket-{i}") }))
        .collect();
    let fake = Fake::start(vec![
        reply(
            "GET",
            &format!("/accounts/{ACCOUNT}/r2/buckets?per_page=1000"),
            200,
            {
                let mut page = ok(json!({ "buckets": full }));
                page["result_info"] = json!({ "cursor": "next", "per_page": 1000 });
                page
            },
        ),
        reply(
            "GET",
            &format!("/accounts/{ACCOUNT}/r2/buckets?per_page=1000&cursor=next"),
            200,
            {
                let mut page =
                    ok(json!({ "buckets": [{ "name": "koidrive", "location": "apac" }] }));
                page["result_info"] =
                    json!({ "cursor": "ignored-on-a-short-page", "per_page": 1000 });
                page
            },
        ),
    ])
    .await;

    let buckets = fake
        .client()
        .buckets(ACCOUNT, Jurisdiction::Default)
        .await
        .unwrap();

    assert_eq!(buckets.len(), 1001);
    assert_eq!(buckets.last().map(String::as_str), Some("koidrive"));
    assert_eq!(fake.calls().len(), 2);
}

#[tokio::test]
async fn a_refusal_keeps_cloudflares_error_codes() {
    let fake = Fake::start(vec![reply(
        "GET",
        &format!("/accounts/{ACCOUNT}/r2/buckets?per_page=1000"),
        403,
        refused(10042, "Please enable R2 through the Cloudflare Dashboard."),
    )])
    .await;

    let error = fake
        .client()
        .buckets(ACCOUNT, Jurisdiction::Default)
        .await
        .unwrap_err();

    assert!(error.has_code(10042));
    assert!(!error.has_code(10000));
    assert_eq!(error.status(), Some(403));
    let shown = error.to_string();
    assert_eq!(
        shown,
        "could not list R2 buckets: Cloudflare answered HTTP 403, Please enable R2 through the Cloudflare Dashboard. \
         [code: 10042]"
    );
    assert!(!format!("{error:?}").contains(TOKEN));
}

#[tokio::test]
async fn a_body_that_is_not_an_envelope_is_unreadable() {
    let fake = Fake::start(vec![
        reply(
            "GET",
            &format!("/accounts/{ACCOUNT}/workers/subdomain"),
            200,
            "<html>maintenance</html>",
        ),
        reply(
            "GET",
            &format!("/accounts/{ACCOUNT}/workers/scripts"),
            502,
            "<html>bad gateway</html>",
        ),
    ])
    .await;
    let client = fake.client();

    let error = client.workers_subdomain(ACCOUNT).await.unwrap_err();
    assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
    assert_eq!(error.status(), None);

    let error = client.scripts(ACCOUNT).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Failed { errors, .. } if errors.is_empty()),
        "{error:?}"
    );
    assert_eq!(error.status(), Some(502));
}

#[tokio::test]
async fn buckets_are_created_and_deleted() {
    let fake = Fake::start(vec![
        reply(
            "POST",
            &format!("/accounts/{ACCOUNT}/r2/buckets"),
            200,
            ok(json!({ "name": "koidrive" })),
        ),
        reply(
            "DELETE",
            &format!("/accounts/{ACCOUNT}/r2/buckets/koidrive"),
            200,
            ok(json!({})),
        ),
        reply(
            "DELETE",
            &format!("/accounts/{ACCOUNT}/r2/buckets/gone"),
            404,
            refused(10006, "The specified bucket does not exist."),
        ),
        reply(
            "DELETE",
            &format!("/accounts/{ACCOUNT}/r2/buckets/full"),
            409,
            refused(10008, "The bucket you tried to delete is not empty."),
        ),
    ])
    .await;
    let client = fake.client();

    let default = Jurisdiction::Default;
    client
        .create_bucket(ACCOUNT, "koidrive", default)
        .await
        .unwrap();
    client
        .delete_bucket(ACCOUNT, "koidrive", default)
        .await
        .unwrap();
    client
        .delete_bucket(ACCOUNT, "gone", default)
        .await
        .unwrap();
    let error = client
        .delete_bucket(ACCOUNT, "full", default)
        .await
        .unwrap_err();

    assert!(error.has_code(10008), "{error}");
    let seen = fake.seen();
    assert_eq!(seen[0].content_type.as_deref(), Some("application/json"));
    assert_eq!(seen[0].json(), json!({ "name": "koidrive" }));
    assert_eq!(
        fake.calls()[1..],
        [
            (
                "DELETE".to_owned(),
                format!("/accounts/{ACCOUNT}/r2/buckets/koidrive")
            ),
            (
                "DELETE".to_owned(),
                format!("/accounts/{ACCOUNT}/r2/buckets/gone")
            ),
            (
                "DELETE".to_owned(),
                format!("/accounts/{ACCOUNT}/r2/buckets/full")
            ),
        ]
    );
}

#[tokio::test]
async fn objects_are_listed_page_by_page() {
    let objects = format!("/accounts/{ACCOUNT}/r2/buckets/koidrive/objects");
    let fake = Fake::start(vec![
        reply("GET", &format!("{objects}?per_page=1000"), 200, {
            let mut page =
                ok(json!([{ "key": "spaces/a/meta/1/space.json", "size": 10 }, { "key": "b" }]));
            page["result_info"] =
                json!({ "cursor": "page-2", "is_truncated": true, "per_page": 1000 });
            page
        }),
        reply(
            "GET",
            &format!("{objects}?per_page=1000&cursor=page-2"),
            200,
            {
                let mut page = ok(json!([{ "key": "c" }]));
                page["result_info"] =
                    json!({ "cursor": "", "is_truncated": false, "per_page": 1000 });
                page
            },
        ),
    ])
    .await;
    let client = fake.client();

    let first = client
        .objects(ACCOUNT, "koidrive", Jurisdiction::Default, None)
        .await
        .unwrap();
    let second = client
        .objects(
            ACCOUNT,
            "koidrive",
            Jurisdiction::Default,
            first.cursor.as_deref(),
        )
        .await
        .unwrap();

    assert_eq!(
        first,
        ObjectPage {
            keys: vec!["spaces/a/meta/1/space.json".to_owned(), "b".to_owned()],
            cursor: Some("page-2".to_owned())
        }
    );
    assert_eq!(
        second,
        ObjectPage {
            keys: vec!["c".to_owned()],
            cursor: None
        }
    );
}

#[tokio::test]
async fn names_from_users_are_percent_encoded() {
    let fake = Fake::start(vec![
        reply(
            "DELETE",
            &format!(
                "/accounts/{ACCOUNT}/r2/buckets/koidrive/objects/spaces%2Fa%20b%2F%3Fq%3D1%23x%25"
            ),
            404,
            refused(10007, "The specified key does not exist."),
        ),
        reply(
            "DELETE",
            &format!("/accounts/{ACCOUNT}/workers/scripts/a%2F..%2Fb?force=true"),
            200,
            ok(Value::Null),
        ),
    ])
    .await;
    let client = fake.client();

    client
        .delete_object(
            ACCOUNT,
            "koidrive",
            Jurisdiction::Default,
            "spaces/a b/?q=1#x%",
        )
        .await
        .unwrap();
    client.delete_script(ACCOUNT, "a/../b").await.unwrap();
    let error = client
        .delete_object(ACCOUNT, "koidrive", Jurisdiction::Default, "..")
        .await
        .unwrap_err();

    assert!(
        matches!(&error, ApiError::Name { name } if name == ".."),
        "{error:?}"
    );
    assert_eq!(fake.calls().len(), 2);
}

#[tokio::test]
async fn scripts_are_listed_with_their_tags() {
    let fake = Fake::start(vec![reply(
        "GET",
        &format!("/accounts/{ACCOUNT}/workers/scripts"),
        200,
        ok(json!([
            { "id": "koidrive", "tags": ["koidrive-server"], "migration_tag": "v1", "has_assets": true, "etag": "x", "created_on": "2026-11-10T05:00:00.000000Z" },
            { "id": "other", "tags": null, "has_modules": true }
        ])),
    )])
    .await;

    let scripts = fake.client().scripts(ACCOUNT).await.unwrap();

    assert_eq!(
        scripts,
        [
            Script {
                id: "koidrive".to_owned(),
                created_on: Some("2026-11-10T05:00:00.000000Z".to_owned()),
                tags: vec!["koidrive-server".to_owned()],
                migration_tag: Some("v1".to_owned()),
                has_assets: true
            },
            Script {
                id: "other".to_owned(),
                created_on: None,
                tags: Vec::new(),
                migration_tag: None,
                has_assets: false
            },
        ]
    );
}

#[tokio::test]
async fn script_settings_name_the_bound_bucket() {
    let fake = Fake::start(vec![reply(
        "GET",
        &format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/settings"),
        200,
        ok(json!({
            "bindings": [
                { "type": "durable_object_namespace", "name": "ACCOUNT", "class_name": "AccountDO", "namespace_id": "n1" },
                { "type": "r2_bucket", "name": "BUCKET", "bucket_name": "koidrive-2" },
                { "type": "r2_bucket", "name": "ARCHIVE", "bucket_name": "koidrive-2", "jurisdiction": "eu" },
                { "type": "secret_text", "name": "KOIDRIVE_RECOVERY_KEY" },
                { "type": "assets", "name": "ASSETS" }
            ],
            "compatibility_date": "2026-11-01",
            "observability": { "enabled": true }
        })),
    )])
    .await;

    let settings = fake
        .client()
        .script_settings(ACCOUNT, "koidrive")
        .await
        .unwrap();

    let bucket = settings.r2_bucket("BUCKET").unwrap();
    assert_eq!(
        (bucket.bucket_name.as_deref(), bucket.jurisdiction),
        (Some("koidrive-2"), None)
    );
    let archive = settings.r2_bucket("ARCHIVE").unwrap();
    assert_eq!(
        (archive.bucket_name.as_deref(), archive.jurisdiction),
        (Some("koidrive-2"), Some(Jurisdiction::Eu))
    );
    assert!(settings.r2_bucket("ASSETS").is_none());
    assert_eq!(
        settings.bindings[0].class_name.as_deref(),
        Some("AccountDO")
    );
    let names: Vec<&str> = settings
        .bindings
        .iter()
        .map(|binding| binding.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "ACCOUNT",
            "BUCKET",
            "ARCHIVE",
            "KOIDRIVE_RECOVERY_KEY",
            "ASSETS"
        ]
    );
}

fn bundle() -> Bundle {
    let manifest: Manifest = serde_json::from_value(json!({
        "version": "0.3.0",
        "storage": { "account": 10, "space": 10 },
        "main_module": "worker.js",
        "compatibility_date": "2026-11-01",
        "durable_objects": [{ "name": "ACCOUNT", "class_name": "AccountDO" }],
        "r2_binding": "BUCKET",
        "default_bucket": "koidrive",
        "migrations": [{ "tag": "v1", "new_sqlite_classes": ["AccountDO"] }],
        "assets": { "binding": "ASSETS" }
    }))
    .unwrap();
    Bundle {
        manifest,
        module: b"export default {};".to_vec(),
        assets: Vec::new(),
        headers: None,
    }
}

#[tokio::test]
async fn a_script_upload_sends_the_metadata_and_the_module() {
    let fake = Fake::start(vec![reply(
        "PUT",
        &format!("/accounts/{ACCOUNT}/workers/scripts/koidrive"),
        200,
        ok(json!({ "id": "koidrive", "tags": ["koidrive-server"], "migration_tag": "v1", "has_assets": true, "startup_time_ms": 12 })),
    )])
    .await;
    let secret = Secret::new("one-time-key".to_owned());
    let jwt = Secret::new("completion-jwt".to_owned());
    let plan = UploadPlan::new(
        &bundle(),
        "koidrive",
        Jurisdiction::Default,
        None,
        Some(&secret),
        Some(&jwt),
    )
    .unwrap();

    let script = fake
        .client()
        .upload_script(ACCOUNT, "koidrive", &plan)
        .await
        .unwrap();

    assert_eq!(script.migration_tag.as_deref(), Some("v1"));
    let seen = fake.seen();
    assert_eq!(seen[0].method, "PUT");
    let parts = parts(&seen[0]);
    assert_eq!(parts.len(), 2);
    assert_eq!(
        (parts[0].name.as_str(), parts[0].filename.as_deref()),
        ("metadata", None)
    );
    assert_eq!(parts[0].content_type.as_deref(), Some("application/json"));
    let metadata: Value = serde_json::from_slice(&parts[0].body).unwrap();
    assert_eq!(metadata["main_module"], "worker.js");
    assert_eq!(metadata["tags"], json!(["koidrive-server"]));
    assert_eq!(
        metadata["migrations"],
        json!({ "new_tag": "v1", "steps": [{ "new_sqlite_classes": ["AccountDO"] }] })
    );
    assert_eq!(
        metadata["bindings"][2],
        json!({ "type": "secret_text", "name": "KOIDRIVE_RECOVERY_KEY", "text": "one-time-key" })
    );
    assert_eq!(metadata["assets"], json!({ "jwt": "completion-jwt" }));
    assert_eq!(
        (parts[1].name.as_str(), parts[1].filename.as_deref()),
        ("worker.js", Some("worker.js"))
    );
    assert_eq!(
        parts[1].content_type.as_deref(),
        Some("application/javascript+module")
    );
    assert_eq!(parts[1].body, b"export default {};");
}

#[tokio::test]
async fn secrets_are_set_listed_and_deleted() {
    let secrets = format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/secrets");
    let fake = Fake::start(vec![
        reply(
            "PUT",
            &secrets,
            200,
            ok(json!({ "name": "KOIDRIVE_RECOVERY_KEY", "type": "secret_text" })),
        ),
        reply(
            "GET",
            &secrets,
            200,
            ok(json!([{ "name": "KOIDRIVE_RECOVERY_KEY", "type": "secret_text" }])),
        ),
        reply(
            "DELETE",
            &format!("{secrets}/KOIDRIVE_RECOVERY_KEY"),
            200,
            ok(Value::Null),
        ),
        reply(
            "DELETE",
            &format!("{secrets}/KOIDRIVE_RECOVERY_KEY"),
            404,
            refused(10056, "Secret not found"),
        ),
    ])
    .await;
    let client = fake.client();

    client
        .put_secret(
            ACCOUNT,
            "koidrive",
            "KOIDRIVE_RECOVERY_KEY",
            &Secret::new("one-time-key".to_owned()),
        )
        .await
        .unwrap();
    let names = client.secrets(ACCOUNT, "koidrive").await.unwrap();
    client
        .delete_secret(ACCOUNT, "koidrive", "KOIDRIVE_RECOVERY_KEY")
        .await
        .unwrap();
    client
        .delete_secret(ACCOUNT, "koidrive", "KOIDRIVE_RECOVERY_KEY")
        .await
        .unwrap();

    assert_eq!(names, ["KOIDRIVE_RECOVERY_KEY"]);
    assert_eq!(
        fake.seen()[0].json(),
        json!({ "name": "KOIDRIVE_RECOVERY_KEY", "text": "one-time-key", "type": "secret_text" })
    );
    assert_eq!(fake.calls().len(), 4);
}

#[tokio::test]
async fn a_script_is_deleted_with_force_and_a_missing_one_counts_as_deleted() {
    let script = format!("/accounts/{ACCOUNT}/workers/scripts/koidrive?force=true");
    let fake = Fake::start(vec![
        reply("DELETE", &script, 200, ok(Value::Null)),
        reply(
            "DELETE",
            &script,
            404,
            refused(10007, "This Worker does not exist on your account."),
        ),
        reply(
            "DELETE",
            &script,
            500,
            refused(10013, "An unknown error has occurred."),
        ),
    ])
    .await;
    let client = fake.client();

    client.delete_script(ACCOUNT, "koidrive").await.unwrap();
    client.delete_script(ACCOUNT, "koidrive").await.unwrap();
    let error = client.delete_script(ACCOUNT, "koidrive").await.unwrap_err();

    assert_eq!(error.status(), Some(500));
    assert_eq!(fake.calls().len(), 3);
}

#[tokio::test]
async fn workers_dev_is_enabled_and_the_subdomain_read() {
    let fake = Fake::start(vec![
        reply(
            "POST",
            &format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/subdomain"),
            200,
            ok(json!({ "enabled": true, "previews_enabled": false })),
        ),
        reply(
            "GET",
            &format!("/accounts/{ACCOUNT}/workers/subdomain"),
            200,
            ok(json!({ "subdomain": "alice" })),
        ),
    ])
    .await;
    let client = fake.client();

    client
        .enable_workers_dev(ACCOUNT, "koidrive")
        .await
        .unwrap();
    let subdomain = client.workers_subdomain(ACCOUNT).await.unwrap();

    assert_eq!(subdomain, "alice");
    assert_eq!(
        fake.seen()[0].json(),
        json!({ "enabled": true, "previews_enabled": false })
    );
    assert_eq!(fake.calls().len(), 2);
}

#[tokio::test]
async fn a_workers_dev_subdomain_is_created_with_the_requested_name() {
    let fake = Fake::start(vec![reply(
        "PUT",
        &format!("/accounts/{ACCOUNT}/workers/subdomain"),
        200,
        ok(json!({"subdomain":"hello-koidrive"})),
    )])
    .await;
    let name = fake
        .client()
        .create_workers_subdomain(ACCOUNT, "hello-koidrive")
        .await
        .unwrap();
    assert_eq!(name, "hello-koidrive");
    assert_eq!(fake.seen()[0].json(), json!({"subdomain":"hello-koidrive"}));
    assert_eq!(
        fake.seen()[0].authorization.as_deref(),
        Some(&format!("Bearer {TOKEN}")[..])
    );
}

fn prototype_hash(contents: &[u8], extension: &str) -> String {
    let digest = Sha256::digest(format!("{}{extension}", STANDARD.encode(contents)));
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn assets() -> Vec<Asset> {
    vec![
        Asset {
            path: "/viewer/index.html".to_owned(),
            bytes: b"<!doctype html>".to_vec(),
        },
        Asset {
            path: "/viewer/viewer.js".to_owned(),
            bytes: b"console.log(1);".to_vec(),
        },
    ]
}

#[tokio::test]
async fn assets_upload_the_files_cloudflare_asks_for() {
    let html = prototype_hash(b"<!doctype html>", "html");
    let js = prototype_hash(b"console.log(1);", "js");
    let fake = Fake::start(vec![
        reply(
            "POST",
            &format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/assets-upload-session"),
            200,
            ok(json!({ "jwt": "upload-jwt", "buckets": [[html]] })),
        ),
        reply(
            "POST",
            &format!("/accounts/{ACCOUNT}/workers/assets/upload?base64=true"),
            201,
            ok(json!({ "jwt": "completion-jwt" })),
        ),
    ])
    .await;

    let completion = fake
        .client()
        .upload_assets(ACCOUNT, "koidrive", &assets())
        .await
        .unwrap();

    assert_eq!(completion.expose(), "completion-jwt");
    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(&*format!("Bearer {TOKEN}"))
    );
    assert_eq!(
        seen[0].json(),
        json!({ "manifest": {
            "/viewer/index.html": { "hash": html, "size": 15 },
            "/viewer/viewer.js": { "hash": js, "size": 15 }
        } })
    );
    assert_eq!(seen[1].authorization.as_deref(), Some("Bearer upload-jwt"));
    let parts = parts(&seen[1]);
    assert_eq!(parts.len(), 1);
    assert_eq!(
        (parts[0].name.as_str(), parts[0].filename.as_deref()),
        (html.as_str(), Some(html.as_str()))
    );
    assert_eq!(
        parts[0].content_type.as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        parts[0].body,
        STANDARD.encode(b"<!doctype html>").as_bytes()
    );
}

#[tokio::test]
async fn assets_cloudflare_already_holds_need_no_upload() {
    let fake = Fake::start(vec![reply(
        "POST",
        &format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/assets-upload-session"),
        200,
        ok(json!({ "jwt": "completion-jwt", "buckets": [] })),
    )])
    .await;

    let completion = fake
        .client()
        .upload_assets(ACCOUNT, "koidrive", &assets())
        .await
        .unwrap();

    assert_eq!(completion.expose(), "completion-jwt");
    assert_eq!(fake.calls().len(), 1);
}

#[tokio::test]
async fn an_asset_upload_without_a_completion_token_fails() {
    let html = prototype_hash(b"<!doctype html>", "html");
    let fake = Fake::start(vec![
        reply(
            "POST",
            &format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/assets-upload-session"),
            200,
            ok(json!({ "jwt": "upload-jwt", "buckets": [[html]] })),
        ),
        reply(
            "POST",
            &format!("/accounts/{ACCOUNT}/workers/assets/upload?base64=true"),
            202,
            ok(Value::Null),
        ),
    ])
    .await;

    let error = fake
        .client()
        .upload_assets(ACCOUNT, "koidrive", &assets())
        .await
        .unwrap_err();

    assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
    assert!(!error.to_string().contains("upload-jwt"));
}

#[tokio::test]
async fn every_r2_call_names_the_jurisdiction() {
    let bucket = format!("/accounts/{ACCOUNT}/r2/buckets/koidrive");
    let fake = Fake::start(vec![
        reply("GET", &format!("/accounts/{ACCOUNT}/r2/buckets?per_page=1000"), 200, ok(json!({ "buckets": [] }))),
        reply("POST", &format!("/accounts/{ACCOUNT}/r2/buckets"), 200, ok(json!({ "name": "koidrive" }))),
        reply("GET", &bucket, 200, ok(json!({ "name": "koidrive", "creation_date": "2026-11-10T05:00:00.000Z", "jurisdiction": "eu", "location": "weur" }))),
        reply("GET", &format!("{bucket}/objects?per_page=1000"), 200, ok(json!([]))),
        reply("DELETE", &format!("{bucket}/objects/k"), 200, ok(json!({}))),
        reply("DELETE", &bucket, 200, ok(json!({}))),
    ])
    .await;
    let client = fake.client();
    let eu = Jurisdiction::Eu;

    client.buckets(ACCOUNT, eu).await.unwrap();
    client.create_bucket(ACCOUNT, "koidrive", eu).await.unwrap();
    let read = client.bucket(ACCOUNT, "koidrive", eu).await.unwrap();
    client.objects(ACCOUNT, "koidrive", eu, None).await.unwrap();
    client
        .delete_object(ACCOUNT, "koidrive", eu, "k")
        .await
        .unwrap();
    client.delete_bucket(ACCOUNT, "koidrive", eu).await.unwrap();

    assert_eq!(
        read,
        Bucket {
            name: "koidrive".to_owned(),
            creation_date: "2026-11-10T05:00:00.000Z".to_owned(),
            jurisdiction: Some(eu)
        }
    );
    let sent: Vec<_> = fake
        .seen()
        .into_iter()
        .map(|seen| seen.jurisdiction)
        .collect();
    assert_eq!(sent, vec![Some("eu".to_owned()); 6]);
}

#[tokio::test]
async fn an_unknown_jurisdiction_or_a_bucket_without_a_creation_date_is_unreadable() {
    let fake = Fake::start(vec![
        reply(
            "GET",
            &format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/settings"),
            200,
            ok(json!({ "bindings": [{ "type": "r2_bucket", "name": "BUCKET", "bucket_name": "koidrive", "jurisdiction": "mars" }] })),
        ),
        reply("GET", &format!("/accounts/{ACCOUNT}/r2/buckets/koidrive"), 200, ok(json!({ "name": "koidrive" }))),
    ])
    .await;
    let client = fake.client();

    let error = client
        .script_settings(ACCOUNT, "koidrive")
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
    let error = client
        .bucket(ACCOUNT, "koidrive", Jurisdiction::Default)
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
}

#[tokio::test]
async fn a_truncated_object_page_without_a_cursor_is_unreadable() {
    let fake = Fake::start(vec![reply(
        "GET",
        &format!("/accounts/{ACCOUNT}/r2/buckets/koidrive/objects?per_page=1000"),
        200,
        {
            let mut page = ok(json!([{ "key": "a" }]));
            page["result_info"] = json!({ "cursor": "", "is_truncated": true, "per_page": 1000 });
            page
        },
    )])
    .await;

    let error = fake
        .client()
        .objects(ACCOUNT, "koidrive", Jurisdiction::Default, None)
        .await
        .unwrap_err();

    assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
}

#[tokio::test]
async fn every_asset_path_is_in_the_manifest_even_with_the_same_contents() {
    let html = prototype_hash(b"<!doctype html>", "html");
    let fake = Fake::start(vec![reply(
        "POST",
        &format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/assets-upload-session"),
        200,
        ok(json!({ "jwt": "completion-jwt", "buckets": [] })),
    )])
    .await;
    let same = [
        Asset {
            path: "/a/index.html".to_owned(),
            bytes: b"<!doctype html>".to_vec(),
        },
        Asset {
            path: "/b/index.html".to_owned(),
            bytes: b"<!doctype html>".to_vec(),
        },
    ];

    fake.client()
        .upload_assets(ACCOUNT, "koidrive", &same)
        .await
        .unwrap();

    assert_eq!(
        fake.seen()[0].json(),
        json!({ "manifest": {
            "/a/index.html": { "hash": html, "size": 15 },
            "/b/index.html": { "hash": html, "size": 15 }
        } })
    );
}

#[tokio::test]
async fn a_session_without_a_buckets_list_is_not_a_completion() {
    let session = format!("/accounts/{ACCOUNT}/workers/scripts/koidrive/assets-upload-session");
    let fake = Fake::start(vec![
        reply("POST", &session, 200, ok(json!({ "jwt": "upload-jwt" }))),
        reply(
            "POST",
            &session,
            200,
            ok(json!({ "jwt": "upload-jwt", "buckets": null })),
        ),
    ])
    .await;
    let client = fake.client();

    for _ in 0..2 {
        let error = client
            .upload_assets(ACCOUNT, "koidrive", &assets())
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
        assert!(!error.to_string().contains("upload-jwt"));
    }
}

#[tokio::test]
async fn a_repeated_cursor_or_endless_full_pages_are_unreadable() {
    let full: Vec<Value> = (0..1000)
        .map(|i| json!({ "name": format!("bucket-{i}") }))
        .collect();
    let page = |cursor: &str| {
        let mut page = ok(json!({ "buckets": full }));
        page["result_info"] = json!({ "cursor": cursor, "per_page": 1000 });
        page
    };
    let buckets = format!("/accounts/{ACCOUNT}/r2/buckets?per_page=1000");
    let objects =
        format!("/accounts/{ACCOUNT}/r2/buckets/koidrive/objects?per_page=1000&cursor=same");
    let account = json!({ "id": ACCOUNT, "name": "Alice" });
    let mut replies = vec![
        reply("GET", &buckets, 200, page("same")),
        reply("GET", &format!("{buckets}&cursor=same"), 200, page("same")),
        reply("GET", &objects, 200, {
            let mut page = ok(json!([{ "key": "a" }]));
            page["result_info"] = json!({ "cursor": "same", "is_truncated": true });
            page
        }),
    ];
    for number in 1..=100 {
        replies.push(reply(
            "GET",
            &format!("/accounts?page={number}&per_page=50"),
            200,
            ok(json!(vec![account.clone(); 50])),
        ));
    }
    let fake = Fake::start(replies).await;
    let client = fake.client();

    let error = client
        .buckets(ACCOUNT, Jurisdiction::Default)
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
    let error = client
        .objects(ACCOUNT, "koidrive", Jurisdiction::Default, Some("same"))
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
    let error = client.accounts().await.unwrap_err();
    assert!(matches!(error, ApiError::Unreadable { .. }), "{error:?}");
    assert_eq!(fake.calls().len(), 103);
}
