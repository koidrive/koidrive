// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Weak};

use crate::types::{
    Account, Answer, Device, FileVersions, LinkedFolder, NewShareLink, PairingCode, RestoreAsked,
    RestoreRequest, ServerInfo, SessionError, SessionEvent, SessionState, ShareExpiry, ShareLink,
    Space, SpaceUsage, TelemetrySetting, Trash,
};

pub(crate) fn pairing_code<'a>(text: &'a str, server_url: &str) -> Result<&'a str, SessionError> {
    let unquoted = |word: &'a str| word.trim_matches('\'');
    let words: Vec<&str> = text.split_whitespace().map(unquoted).collect();
    if let [code, "--server", url] = words.as_slice() {
        let server = server_url.trim_end_matches('/');
        if url.trim_end_matches('/') != server {
            return Err(SessionError::CodeForOtherServer {
                host: (*url).to_owned(),
                server: server.to_owned(),
            });
        }
        return Ok(code);
    }
    let text = text.trim();
    let text = text
        .strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
        .unwrap_or(text);
    let Some((code, host)) = text.rsplit_once('@') else {
        return Ok(text);
    };
    let server = url::Url::parse(server_url)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?;
            Some(match (url.scheme(), url.port()) {
                ("https", Some(port)) => format!("{host}:{port}"),
                ("https", None) => host.to_owned(),
                _ => url.origin().ascii_serialization(),
            })
        })
        .unwrap_or_else(|| server_url.to_owned());
    let host = host.trim();
    if !host.eq_ignore_ascii_case(&server) {
        return Err(SessionError::CodeForOtherServer {
            host: host.to_owned(),
            server,
        });
    }
    Ok(code.trim())
}

/// Receives every [`SessionEvent`] of a session, in order.
///
/// The core calls the listener on the thread that changed the state, while holding its lock,
/// which keeps the events in order. A listener that calls back into the session before
/// returning deadlocks.
///
/// The session drops the listener when the session itself is dropped, so a listener can treat
/// its own drop as the end of the stream.
#[uniffi::export(callback_interface)]
pub trait SessionListener: Send + Sync {
    fn on_event(&self, event: SessionEvent);
}

#[derive(Clone)]
pub struct Events(Weak<Box<dyn SessionListener>>);

impl Events {
    pub(crate) fn new(listener: &Arc<Box<dyn SessionListener>>) -> Self {
        Self(Arc::downgrade(listener))
    }

    pub fn send(&self, event: SessionEvent) {
        if let Some(listener) = self.0.upgrade() {
            listener.on_event(event);
        }
    }
}

/// What the app needs from the sync core. It maps onto the engine's inputs and outputs (design
/// §1): the core turns the person's actions into engine inputs (`UserDecision` for answers)
/// and session setup, and reports the engine's `AskUser` and `Problem` outputs, with sync
/// status, progress and recent changes, as [`SessionEvent`]s.
///
/// Calls that talk to the server block until it answers, so the app makes them off its main
/// thread. Every change a call makes to [`SessionState`] is also sent as an event before the
/// call returns.
///
/// A call that changes what this device keeps (its credentials, folders, answers, notices or
/// the pause) saves the change before it reports success, as the engine persists each input
/// before acting on it (design §1). If saving fails, the call changes nothing and returns
/// [`SessionError::NotSaved`], and [`ProblemKind::StateNotWritable`](crate::ProblemKind::StateNotWritable)
/// stays listed until a save succeeds.
pub trait Core: Send + Sync {
    /// The current state. Events sent before this call returns are already included.
    fn state(&self) -> SessionState;

    /// Starts syncing every linked folder. The core then asks every unanswered question again by
    /// sending [`SessionEvent::Questions`], as the engine does after a restart (design §2.8).
    /// Starting a running session does nothing.
    fn start(&self);
    /// Stops syncing. State on disk and in the store is kept. Stopping a stopped session does
    /// nothing.
    fn stop(&self);
    /// Pauses a running session until [`Core::resume`]. The pause outlives a restart. Pausing
    /// a stopped session does nothing.
    fn pause(&self) -> Result<(), SessionError>;
    /// Resumes a paused session.
    fn resume(&self) -> Result<(), SessionError>;

    /// Looks up the server at `url` and remembers it for signing in.
    fn connect(&self, url: &str) -> Result<ServerInfo, SessionError>;
    /// Claims an unclaimed server as its owner, with the one-time code from its Worker log, and
    /// signs this device in.
    fn claim(
        &self,
        code: &str,
        user_name: &str,
        device_name: &str,
    ) -> Result<Account, SessionError>;
    /// Signs this device in to a claimed server with a pairing code from a device already
    /// signed in.
    fn pair(&self, code: &str, device_name: &str) -> Result<Account, SessionError>;
    /// Revokes the device on the server, forgets its credentials and unlinks every folder. Files on
    /// disk stay. Unless `confirm_last_device` is set, it changes nothing and returns
    /// [`SessionError::LastDevice`] when this is the account's last active device, or
    /// [`SessionError::NotRevoked`] when the server cannot be reached, so the app can tell the person
    /// first. With it, a sign-out without the server finishes on the device.
    fn sign_out(&self, confirm_last_device: bool) -> Result<(), SessionError>;
    /// The account's devices that are not revoked.
    fn devices(&self) -> Result<Vec<Device>, SessionError>;
    /// Revokes another device of the account on the server. It can no longer sync, and the files already on it stay
    /// there. This device leaves with [`Core::sign_out`], so its own ID returns [`SessionError::ThisDevice`]. An ID
    /// unlike those the server issues (36 lowercase hexadecimal digits and hyphens) returns
    /// [`SessionError::DeviceNotFound`] without a request.
    fn revoke_device(&self, device_id: &str) -> Result<(), SessionError>;
    /// Creates a code that signs in another device.
    fn start_pairing(&self) -> Result<PairingCode, SessionError>;

    /// The account's spaces.
    fn spaces(&self) -> Result<Vec<Space>, SessionError>;
    /// Creates a space.
    fn create_space(&self, name: &str) -> Result<Space, SessionError>;
    /// What a space stores. The server answers only the space's owner.
    fn space_usage(&self, space_id: &str) -> Result<SpaceUsage, SessionError>;
    /// Links the folder at absolute `path` to a space and starts syncing it.
    fn link_folder(&self, space_id: &str, path: &str) -> Result<LinkedFolder, SessionError>;
    /// Stops syncing a space on this device. Files on disk stay.
    fn unlink_folder(&self, space_id: &str) -> Result<(), SessionError>;

    /// What was deleted in the folder at absolute `path`, a linked folder or a folder inside one, and is still in
    /// the trash of its space. A path that is not absolute returns [`SessionError::PathNotInLinkedFolder`].
    fn trash(&self, path: &str) -> Result<Trash, SessionError>;
    /// The versions the server keeps of the file at absolute `path` in a linked folder.
    fn versions(&self, path: &str) -> Result<FileVersions, SessionError>;
    /// Asks to bring the deleted item `node_id` of a [`Trash`] back to absolute `path`, the path it had. The folder
    /// sends the request with its next commit; [`Core::restore_requests`] says how it went.
    fn restore_deleted(&self, path: &str, node_id: &str) -> Result<RestoreAsked, SessionError>;
    /// Asks to make the version `version_id` of the file at absolute `path` its content again, as a new version.
    fn restore_version(&self, path: &str, version_id: &str) -> Result<RestoreAsked, SessionError>;
    /// The restores asked for in the linked folder that holds absolute `path`, a linked folder or a folder inside
    /// one: first those that ended, in the order they ended, then those that wait, in request order.
    fn restore_requests(&self, path: &str) -> Result<Vec<RestoreRequest>, SessionError>;

    /// Every share link of the spaces this device's user owns, newest first, ended ones included.
    fn share_links(&self) -> Result<Vec<ShareLink>, SessionError>;
    /// Makes a link to the file or folder at absolute `path` in a linked folder, which works for `expiry` and, with
    /// `password`, only after that password of 8 to 256 characters is given.
    fn create_share_link(
        &self,
        path: &str,
        expiry: ShareExpiry,
        password: Option<String>,
    ) -> Result<NewShareLink, SessionError>;
    /// Ends the link `link_id` at once. A link that was revoked already comes back as it is.
    fn revoke_share_link(&self, link_id: &str) -> Result<ShareLink, SessionError>;

    /// Answers a question. The answer must be one of the question's
    /// [`answers`](crate::Question::answers).
    fn answer(&self, question_id: &str, answer: Answer) -> Result<(), SessionError>;
    /// Dismisses a notice.
    fn dismiss_notice(&self, notice_id: &str) -> Result<(), SessionError>;

    /// The usage-report setting. It reads a small file and never talks to the network.
    fn telemetry(&self) -> TelemetrySetting;
    /// Stores the person's choice of usage reports, in the same file the CLI uses. Turning it on draws the install id;
    /// turning it off deletes the id and every report not sent yet. While [`TelemetrySetting::locked`], the choice is
    /// stored anyway and takes effect once the environment variable is gone.
    fn set_telemetry(&self, enabled: bool) -> Result<(), SessionError>;

    /// Stops syncing and releases what the core holds outside memory, such as the lock on this
    /// device's sync state, so that the state can be opened again at once. The session calls it
    /// when it is dropped; no other call follows.
    fn close(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_code_drops_the_host_of_its_server() {
        let server = "https://koidrive.alice.workers.dev/";
        assert_eq!(pairing_code(" K7QM-2XPD ", server), Ok("K7QM-2XPD"));
        assert_eq!(
            pairing_code("K7QM-2XPD@koidrive.alice.workers.dev", server),
            Ok("K7QM-2XPD")
        );
        assert_eq!(
            pairing_code("K7QM-2XPD@KoiDrive.Alice.Workers.Dev ", server),
            Ok("K7QM-2XPD")
        );
        assert_eq!(
            pairing_code(
                "K7QM-2XPD@files.example.com:8443",
                "https://files.example.com:8443"
            ),
            Ok("K7QM-2XPD")
        );
        assert_eq!(
            pairing_code("'K7QM-2XPD@[2001:db8::1]'", "https://[2001:db8::1]/"),
            Ok("K7QM-2XPD")
        );
        assert_eq!(
            pairing_code(
                " K7QM-2XPD --server http://127.0.0.1:8787/ ",
                "http://127.0.0.1:8787"
            ),
            Ok("K7QM-2XPD")
        );
        assert_eq!(
            pairing_code(
                "K7QM-2XPD --server 'http://files.example.com:8080'",
                "http://files.example.com:8080/"
            ),
            Ok("K7QM-2XPD")
        );
    }

    #[test]
    fn a_code_for_another_server_is_refused() {
        assert_eq!(
            pairing_code(
                "K7QM-2XPD@files.example.com",
                "https://koidrive.alice.workers.dev"
            ),
            Err(SessionError::CodeForOtherServer {
                host: "files.example.com".to_owned(),
                server: "koidrive.alice.workers.dev".to_owned(),
            })
        );
        assert_eq!(
            pairing_code(
                "K7QM-2XPD@files.example.com",
                "https://files.example.com:8443"
            ),
            Err(SessionError::CodeForOtherServer {
                host: "files.example.com".to_owned(),
                server: "files.example.com:8443".to_owned(),
            })
        );
        assert_eq!(
            pairing_code(
                "K7QM-2XPD --server http://127.0.0.1:8788/",
                "http://127.0.0.1:8787/"
            ),
            Err(SessionError::CodeForOtherServer {
                host: "http://127.0.0.1:8788/".to_owned(),
                server: "http://127.0.0.1:8787".to_owned(),
            })
        );
        assert_eq!(
            pairing_code("K7QM-2XPD@localhost:8787", "http://localhost:8787"),
            Err(SessionError::CodeForOtherServer {
                host: "localhost:8787".to_owned(),
                server: "http://localhost:8787".to_owned(),
            })
        );
    }
}
