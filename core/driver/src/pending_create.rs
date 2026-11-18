// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, DirBuilder, File};
use std::future::Future;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use koidrive_net::{ApiError, Auth, Resend};
use koidrive_proto::v1::{CreateSpaceRequest, SpaceInfo};
use koidrive_state_file::{AtomicFile, Flock, Publish};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::DriverError;
use crate::driver::{io_error, now_ms};
use crate::paths::Paths;
use crate::share::Expiry;

pub(crate) enum Intent<'a> {
    Claim {
        code: &'a str,
    },
    Pair {
        code: &'a str,
    },
    Space {
        user_id: &'a str,
        name: &'a str,
    },
    ShareLink {
        space_id: &'a str,
        node_id: &'a str,
        expiry: Expiry,
        password: Option<&'a str>,
    },
}

const CLAIM: &str = "claim";
const PAIR: &str = "pair";

impl Intent<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Intent::Claim { .. } => CLAIM,
            Intent::Pair { .. } => PAIR,
            Intent::Space { .. } => "space",
            Intent::ShareLink { .. } => "share-link",
        }
    }

    fn key(&self) -> Vec<String> {
        let code = |code: &str| -> String {
            (code.chars())
                .filter(char::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_uppercase())
                .collect()
        };
        match self {
            Intent::Claim { code: given } | Intent::Pair { code: given } => vec![code(given)],
            Intent::Space { user_id, name } => vec![user_id.to_string(), name.to_string()],
            Intent::ShareLink {
                space_id,
                node_id,
                expiry,
                password,
            } => vec![
                space_id.to_string(),
                node_id.to_string(),
                serde_json::to_string(expiry).expect("an expiry serializes"),
                serde_json::to_string(password).expect("a password serializes"),
            ],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Saved {
    server: String,
    pub(crate) request_id: String,
    pub(crate) since_ms: i64,
}

pub(crate) struct PendingCreate {
    path: PathBuf,
    server: String,
}

impl PendingCreate {
    pub(crate) fn new(paths: &Paths, server: &Url, intent: Intent) -> Self {
        let mut key = vec![server.to_string()];
        key.extend(intent.key());
        let digest: String = (Sha256::digest(serde_json::to_vec(&key).expect("a key serializes"))
            .iter())
        .map(|byte| format!("{byte:02x}"))
        .collect();
        PendingCreate {
            path: paths
                .creates()
                .join(format!("{}-{digest}.json", intent.kind())),
            server: server.to_string(),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn hold(&self) -> io::Result<Held> {
        loop {
            self.save_if_absent()?;
            let file = match File::open(&self.path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let Ok(saved) = serde_json::from_reader::<_, Saved>(&file) else {
                continue;
            };
            match Flock::take(file)? {
                None => return Ok(Held { saved, owner: None }),
                Some(lock) if still_at(&lock, &self.path)? => {
                    return Ok(Held {
                        saved,
                        owner: Some(lock),
                    });
                }
                Some(_) => continue,
            }
        }
    }

    fn save_if_absent(&self) -> io::Result<()> {
        let publish = match load(&self.path) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => Publish::Create,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => Publish::Replace,
            Err(error) => return Err(error),
        };
        let fresh = Saved {
            server: self.server.clone(),
            request_id: koidrive_net::request_id(),
            since_ms: now_ms(),
        };
        let dir = self
            .path
            .parent()
            .expect("a pending create has a directory");
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let stage = AtomicFile::prepare(&self.path)?;
        stage
            .as_file()
            .write_all(&serde_json::to_vec(&fresh).expect("a pending create serializes"))?;
        match stage.finish(publish) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
            _ => Ok(()),
        }
    }

    pub(crate) fn pending(&self) -> bool {
        matches!(load(&self.path), Ok(Some(_)))
    }

    pub(crate) async fn send<T, U, F, Fut>(
        &self,
        held: Held,
        send: F,
        keep: impl FnOnce(T) -> Result<U, DriverError>,
    ) -> Result<U, DriverError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        let outcome = match koidrive_net::resend_lost(send).await {
            Ok(made) => {
                let kept = keep(made)?;
                self.forget();
                Ok(kept)
            }
            Err(error) => {
                if refused_for_good(&error) {
                    self.forget();
                }
                Err(error.into())
            }
        };
        drop(held);
        outcome
    }

    pub(crate) fn forget(&self) {
        forget(&self.path);
    }
}

pub(crate) struct Held {
    pub(crate) saved: Saved,
    owner: Option<Flock>,
}

impl Held {
    pub(crate) fn joined(&self) -> bool {
        self.owner.is_none()
    }
}

fn still_at(lock: &Flock, path: &Path) -> io::Result<bool> {
    let held = lock.metadata()?;
    match fs::metadata(path) {
        Ok(now) => Ok((now.dev(), now.ino()) == (held.dev(), held.ino())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn refused_for_good(error: &ApiError) -> bool {
    matches!(error, ApiError::Server { status, error }
        if (400..500).contains(status) && *status != 429 && error.resend() != Resend::Allowed)
}

fn load(path: &Path) -> io::Result<Option<Saved>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn forget(path: &Path) {
    if let Err(error) = koidrive_state_file::remove_with_legacy_matching(path, |_| false) {
        tracing::warn!(path = %koidrive_fs::quoted(path), %error, "could not forget a finished create");
    }
}

fn saved_of(paths: &Paths, kinds: &[&str]) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(paths.creates()) else {
        return Vec::new();
    };
    (entries.flatten())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            kinds
                .iter()
                .any(|kind| name.starts_with(&format!("{kind}-")))
        })
        .map(|entry| entry.path())
        .collect()
}

pub(crate) fn forget_sign_ins(paths: &Paths) {
    for path in saved_of(paths, &[CLAIM, PAIR]) {
        forget(&path);
    }
}

/// Whether this profile sent a claim to `server` that ended without an answer. Claiming again with the same code
/// within the code's hour finishes it.
pub fn claim_pending(paths: &Paths, server: &Url) -> bool {
    (saved_of(paths, &[CLAIM]).iter())
        .any(|path| matches!(load(path), Ok(Some(saved)) if saved.server == server.as_str()))
}

fn space_create(paths: &Paths, auth: &Auth, name: &str) -> PendingCreate {
    let user_id = auth.credentials().user_id;
    PendingCreate::new(
        paths,
        auth.api().base(),
        Intent::Space {
            user_id: &user_id,
            name,
        },
    )
}

/// The space [`create_space`] answered with.
#[derive(Clone, Debug, PartialEq)]
pub enum CreatedSpace {
    /// This run made the space, or finished a create of it that an earlier run left without an answer.
    Made(SpaceInfo),
    /// Another run was making the space under the same request ID, and this run waited for its answer.
    Joined(SpaceInfo),
}

/// Creates the space `name` for the user signed in with `auth`. A create of the same name that ended without an
/// answer is sent again with its first request ID, so the server answers with the space it made.
pub async fn create_space(
    paths: &Paths,
    auth: &Auth,
    name: &str,
) -> Result<CreatedSpace, DriverError> {
    let pending = space_create(paths, auth, name);
    let held = pending.hold().map_err(io_error(pending.path()))?;
    let joined = held.joined();
    let request = CreateSpaceRequest {
        name: name.to_owned(),
        request_id: held.saved.request_id.clone(),
    };
    let space = pending
        .send(held, || auth.api().create_space(auth, &request), Ok)
        .await?;
    Ok(match joined {
        true => CreatedSpace::Joined(space),
        false => CreatedSpace::Made(space),
    })
}

/// Whether a create of the space `name` for the user signed in with `auth` ended without an answer, so that
/// [`create_space`] would send it again.
pub fn space_create_pending(paths: &Paths, auth: &Auth, name: &str) -> bool {
    space_create(paths, auth, name).pending()
}

#[cfg(test)]
pub(crate) fn saved(paths: &Paths) -> usize {
    fs::read_dir(paths.creates()).map_or(0, |entries| entries.count())
}

#[cfg(test)]
pub(crate) mod fake {
    use std::time::Duration;

    use koidrive_proto::v1::{Error as ProtoError, error::Code};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) enum Answer {
        Lose,
        Refuse(u16, ProtoError),
        Reply(Vec<u8>),
    }

    pub(crate) fn long_wait() -> Answer {
        Answer::Refuse(
            429,
            ProtoError {
                code: Code::QuotaExceeded as i32,
                retry_after_ms: 6 * 3_600_000,
                ..ProtoError::default()
            },
        )
    }

    pub(crate) async fn serve(
        answers: Vec<Answer>,
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(30), async move {
                let mut bodies = Vec::new();
                for answer in answers {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    let head_end = loop {
                        let read = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0);
                        request.extend_from_slice(&buffer[..read]);
                        if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
                    let length: usize = (head.lines())
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    while request.len() < head_end + length {
                        let read = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0);
                        request.extend_from_slice(&buffer[..read]);
                    }
                    bodies.push(request[head_end..].to_vec());
                    let (status, body) = match answer {
                        Answer::Lose => continue,
                        Answer::Refuse(status, error) => (status, prost::Message::encode_to_vec(&error)),
                        Answer::Reply(body) => (200, body),
                    };
                    let head = format!(
                        "HTTP/1.1 {status} Answer\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
                bodies
            })
            .await
            .unwrap()
        });
        (server, served)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use koidrive_net::ServerError;
    use koidrive_proto::v1::{Error as ProtoError, error::Code};

    use super::*;

    fn server() -> Url {
        Url::parse("https://koidrive.alice.workers.dev/").unwrap()
    }

    fn refusal(status: u16, code: Code) -> ApiError {
        ApiError::Server {
            status,
            error: ServerError(ProtoError {
                code: code as i32,
                retry_after_ms: 6 * 3_600_000,
                ..ProtoError::default()
            }),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_definitive_refusal_forgets_the_request_id_and_any_other_failure_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let pending = PendingCreate::new(&paths, &server(), Intent::Pair { code: "ABCD-EFGH" });
        let failures = [
            (refusal(400, Code::InvalidArgument), false),
            (refusal(403, Code::Forbidden), false),
            (refusal(404, Code::NotFound), false),
            (refusal(409, Code::InvalidArgument), false),
            (refusal(429, Code::QuotaExceeded), true),
            (refusal(429, Code::RetryLater), true),
            (refusal(503, Code::RetryLater), true),
            (refusal(500, Code::Internal), true),
            (ApiError::Unreadable { status: 403 }, true),
            (ApiError::Status { status: 502 }, true),
            (ApiError::Unreadable { status: 520 }, true),
        ];
        for (failure, kept) in failures {
            let first = pending.hold().unwrap().saved;
            let failure = std::cell::RefCell::new(Some(failure));
            let sends = Cell::new(0);
            let outcome: Result<(), DriverError> = pending
                .send(
                    pending.hold().unwrap(),
                    || {
                        sends.set(sends.get() + 1);
                        let answer = match failure.borrow_mut().take() {
                            Some(failure) => Err(failure),
                            None => Err(ApiError::Status { status: 503 }),
                        };
                        async move { answer }
                    },
                    Ok,
                )
                .await;
            assert!(outcome.is_err());
            assert_eq!(pending.pending(), kept, "{outcome:?}");
            assert_eq!(pending.hold().unwrap().saved == first, kept, "{outcome:?}");
        }
        pending.forget();
        assert_eq!(saved(&paths), 0);
    }

    #[test]
    fn the_same_intent_resumes_one_request_id_and_another_intent_draws_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let other = Url::parse("https://koidrive.bob.workers.dev/").unwrap();
        let id = |server: &Url, intent: Intent| {
            PendingCreate::new(&paths, server, intent)
                .hold()
                .unwrap()
                .saved
                .request_id
        };
        let link = |node_id, expiry, password| Intent::ShareLink {
            space_id: "s1",
            node_id,
            expiry,
            password,
        };

        let claim = id(
            &server(),
            Intent::Claim {
                code: "ABCD-EFGH-JKLM",
            },
        );
        assert_eq!(
            id(
                &server(),
                Intent::Claim {
                    code: " abcdefghjklm "
                }
            ),
            claim
        );
        let space = id(
            &server(),
            Intent::Space {
                user_id: "u1",
                name: "Notes",
            },
        );
        let shared = id(
            &server(),
            link("n1", Expiry::After { ms: 86_400_000 }, Some("hunter22")),
        );
        let mut ids = vec![
            claim.clone(),
            id(
                &other,
                Intent::Claim {
                    code: "ABCD-EFGH-JKLM",
                },
            ),
            id(
                &server(),
                Intent::Claim {
                    code: "ABCD-EFGH-JKLN",
                },
            ),
            id(
                &server(),
                Intent::Pair {
                    code: "ABCD-EFGH-JKLM",
                },
            ),
            space.clone(),
            id(
                &server(),
                Intent::Space {
                    user_id: "u2",
                    name: "Notes",
                },
            ),
            id(
                &server(),
                Intent::Space {
                    user_id: "u1",
                    name: "Photos",
                },
            ),
            shared.clone(),
            id(
                &server(),
                link("n2", Expiry::After { ms: 86_400_000 }, Some("hunter22")),
            ),
            id(
                &server(),
                link("n1", Expiry::After { ms: 3_600_000 }, Some("hunter22")),
            ),
            id(&server(), link("n1", Expiry::Default, Some("hunter22"))),
            id(&server(), link("n1", Expiry::Never, Some("hunter22"))),
            id(
                &server(),
                link("n1", Expiry::After { ms: 86_400_000 }, Some("hunter23")),
            ),
            id(
                &server(),
                link("n1", Expiry::After { ms: 86_400_000 }, None),
            ),
        ];
        assert_eq!(
            id(
                &server(),
                Intent::Space {
                    user_id: "u1",
                    name: "Notes"
                }
            ),
            space
        );
        assert_eq!(
            id(
                &server(),
                link("n1", Expiry::After { ms: 86_400_000 }, Some("hunter22"))
            ),
            shared
        );
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count);
        assert_eq!(saved(&paths), count);

        for entry in fs::read_dir(paths.creates()).unwrap() {
            let text = fs::read_to_string(entry.unwrap().path()).unwrap();
            for secret in ["ABCDEFGHJKLM", "ABCD-EFGH", "hunter22", "Notes"] {
                assert!(!text.contains(secret), "{text}");
            }
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(paths.creates()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_create_is_forgotten_only_after_its_answer_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let pending = PendingCreate::new(&paths, &server(), Intent::Pair { code: "ABCD" });
        let first = pending.hold().unwrap().saved;
        let not_kept: Result<(), DriverError> = pending
            .send(
                pending.hold().unwrap(),
                || async { Ok(7) },
                |_| Err(DriverError::EmptyName),
            )
            .await;
        assert!(matches!(not_kept, Err(DriverError::EmptyName)));
        assert_eq!(pending.hold().unwrap().saved, first);

        let kept = (pending.send(pending.hold().unwrap(), || async { Ok(7) }, Ok))
            .await
            .unwrap();

        assert_eq!(kept, 7);
        assert_eq!(saved(&paths), 0);
    }

    #[test]
    fn runs_that_start_the_same_create_at_once_send_one_request_id_and_one_of_them_makes_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        for round in 0..20 {
            let name = format!("Notes {round}");
            let barrier = std::sync::Barrier::new(8);
            let held: Vec<Held> = std::thread::scope(|scope| {
                let racers: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            let intent = Intent::Space {
                                user_id: "u1",
                                name: &name,
                            };
                            let pending = PendingCreate::new(&paths, &server(), intent);
                            barrier.wait();
                            pending.hold().unwrap()
                        })
                    })
                    .collect();
                racers
                    .into_iter()
                    .map(|racer| racer.join().unwrap())
                    .collect()
            });
            let ids: Vec<&str> = (held.iter())
                .map(|held| held.saved.request_id.as_str())
                .collect();
            assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
            assert_eq!(held.iter().filter(|held| !held.joined()).count(), 1);
        }
        assert_eq!(saved(&paths), 20);
    }

    fn signed_in(server: &str) -> Auth {
        Auth::new(
            koidrive_net::Api::local_development(server.parse().unwrap()).unwrap(),
            koidrive_net::Credentials {
                user_id: "u1".into(),
                device_id: "d1".into(),
                refresh_credential: "refresh".into(),
            },
        )
        .with_token(koidrive_proto::v1::AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        })
    }

    fn notes() -> SpaceInfo {
        SpaceInfo {
            space_id: "0190a0b0-0000-7000-8000-000000000001".into(),
            name: "Notes".into(),
            ..SpaceInfo::default()
        }
    }

    fn space_request_ids(bodies: &[Vec<u8>]) -> Vec<String> {
        (bodies.iter())
            .map(|body| {
                <CreateSpaceRequest as prost::Message>::decode(body.as_slice())
                    .unwrap()
                    .request_id
            })
            .collect()
    }

    #[tokio::test]
    async fn of_two_runs_that_create_one_space_at_once_one_makes_it_and_the_other_joins() {
        use fake::{Answer, serve};

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let reply = || Answer::Reply(prost::Message::encode_to_vec(&notes()));
        let (server, served) = serve(vec![reply(), reply()]).await;
        let auth = signed_in(&server);

        let (left, right) = tokio::join!(
            create_space(&paths, &auth, "Notes"),
            create_space(&paths, &auth, "Notes"),
        );

        let answers = [left.unwrap(), right.unwrap()];
        assert!(
            answers.contains(&CreatedSpace::Made(notes())),
            "{answers:?}"
        );
        assert!(
            answers.contains(&CreatedSpace::Joined(notes())),
            "{answers:?}"
        );
        let ids = space_request_ids(&served.await.unwrap());
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
        assert_eq!(saved(&paths), 0);
    }

    #[tokio::test]
    async fn a_space_create_that_a_stopped_run_left_is_made_by_the_next_run() {
        use fake::{Answer, serve};

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let (server, served) =
            serve(vec![Answer::Reply(prost::Message::encode_to_vec(&notes()))]).await;
        let auth = signed_in(&server);
        let stopped = space_create(&paths, &auth, "Notes").hold().unwrap().saved;
        assert!(space_create_pending(&paths, &auth, "Notes"));

        let answer = create_space(&paths, &auth, "Notes").await.unwrap();

        assert_eq!(answer, CreatedSpace::Made(notes()));
        assert_eq!(
            space_request_ids(&served.await.unwrap()),
            [stopped.request_id]
        );
        assert_eq!(saved(&paths), 0);
    }

    #[test]
    fn a_profile_that_signs_in_forgets_its_claims_and_pairings_but_keeps_other_creates() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let claim = PendingCreate::new(&paths, &server(), Intent::Claim { code: "ABCD" });
        let pair = PendingCreate::new(&paths, &server(), Intent::Pair { code: "EFGH" });
        let space = PendingCreate::new(
            &paths,
            &server(),
            Intent::Space {
                user_id: "u1",
                name: "Notes",
            },
        );
        for pending in [&claim, &pair, &space] {
            pending.hold().unwrap();
        }
        assert!(claim_pending(&paths, &server()));
        assert!(!claim_pending(
            &paths,
            &Url::parse("https://other.example/").unwrap()
        ));

        forget_sign_ins(&paths);

        assert!(!claim_pending(&paths, &server()));
        assert!(!claim.pending() && !pair.pending() && space.pending());
        assert_eq!(saved(&paths), 1);
    }

    #[test]
    fn an_unreadable_saved_create_is_replaced_and_status_reads_rewrite_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let pending = PendingCreate::new(&paths, &server(), Intent::Claim { code: "ABCD" });
        pending.hold().unwrap();
        fs::write(pending.path(), b"{not json").unwrap();

        assert!(!claim_pending(&paths, &server()));
        assert_eq!(fs::read(pending.path()).unwrap(), b"{not json");
        let fresh = pending.hold().unwrap().saved;

        assert!(claim_pending(&paths, &server()));
        assert_eq!(pending.hold().unwrap().saved, fresh);
        let missing = Paths::under(&dir.path().join("missing"));
        assert!(!claim_pending(&missing, &server()));
        assert!(!missing.state_dir.exists());
    }
}
