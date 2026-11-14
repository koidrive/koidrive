// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use crate::core::{Core, Events, SessionListener};
use crate::daemon::DaemonCore;
use crate::fake::FakeCore;
use crate::profile::Profile;
use crate::types::{
    Account, Answer, Device, FileVersions, LinkedFolder, NewShareLink, PairingCode, RestoreAsked,
    RestoreRequest, ServerInfo, SessionError, SessionState, ShareExpiry, ShareLink, Space,
    SpaceUsage, TelemetrySetting, Trash,
};

/// A sync session: the one object the app talks to. Each method forwards to the session's
/// [`Core`]; see there for what it does. Methods that talk to the server block, so call them
/// off the main thread.
#[derive(uniffi::Object)]
pub struct SyncSession {
    core: Arc<dyn Core>,
    fake: Option<Arc<FakeCore>>,
    _listener: Arc<Box<dyn SessionListener>>,
}

impl Drop for SyncSession {
    fn drop(&mut self) {
        self.core.close();
    }
}

#[uniffi::export]
impl SyncSession {
    /// Opens the CLI's profile on this machine through the `koi` at `helper`: the profile of the installed launch agent,
    /// or else the one the environment names. Changes run as `koi` commands and the state follows the daemon, so the
    /// app and the command show one device. The session is stopped until [`SyncSession::start`].
    #[uniffi::constructor]
    pub fn open(
        listener: Box<dyn SessionListener>,
        helper: String,
    ) -> Result<Arc<Self>, SessionError> {
        let listener = Arc::new(listener);
        let profile =
            Profile::discover(std::path::Path::new(&helper), |name| std::env::var_os(name))?;
        let core = DaemonCore::open(helper.into(), profile, Events::new(&listener))?;
        Ok(Arc::new(Self {
            core,
            fake: None,
            _listener: listener,
        }))
    }

    /// Creates a stopped session backed by [`FakeCore`], which keeps a scripted state in memory,
    /// for previews and tests.
    #[uniffi::constructor]
    pub fn fake(listener: Box<dyn SessionListener>) -> Arc<Self> {
        let listener = Arc::new(listener);
        let fake = Arc::new(FakeCore::new(Events::new(&listener)));
        Arc::new(Self {
            core: fake.clone(),
            fake: Some(fake),
            _listener: listener,
        })
    }

    /// The fake behind this session, for scripting its state, or none for a real core.
    pub fn fake_core(&self) -> Option<Arc<FakeCore>> {
        self.fake.clone()
    }

    pub fn state(&self) -> SessionState {
        self.core.state()
    }

    pub fn start(&self) {
        self.core.start();
    }

    pub fn stop(&self) {
        self.core.stop();
    }

    pub fn pause(&self) -> Result<(), SessionError> {
        self.core.pause()
    }

    pub fn resume(&self) -> Result<(), SessionError> {
        self.core.resume()
    }

    pub fn connect(&self, url: String) -> Result<ServerInfo, SessionError> {
        self.core.connect(&url)
    }

    pub fn claim(
        &self,
        code: String,
        user_name: String,
        device_name: String,
    ) -> Result<Account, SessionError> {
        self.core.claim(&code, &user_name, &device_name)
    }

    pub fn pair(&self, code: String, device_name: String) -> Result<Account, SessionError> {
        self.core.pair(&code, &device_name)
    }

    pub fn sign_out(&self, confirm_last_device: bool) -> Result<(), SessionError> {
        self.core.sign_out(confirm_last_device)
    }

    pub fn devices(&self) -> Result<Vec<Device>, SessionError> {
        self.core.devices()
    }

    pub fn revoke_device(&self, device_id: String) -> Result<(), SessionError> {
        self.core.revoke_device(&device_id)
    }

    pub fn start_pairing(&self) -> Result<PairingCode, SessionError> {
        self.core.start_pairing()
    }

    pub fn spaces(&self) -> Result<Vec<Space>, SessionError> {
        self.core.spaces()
    }

    pub fn create_space(&self, name: String) -> Result<Space, SessionError> {
        self.core.create_space(&name)
    }

    pub fn space_usage(&self, space_id: String) -> Result<SpaceUsage, SessionError> {
        self.core.space_usage(&space_id)
    }

    pub fn link_folder(
        &self,
        space_id: String,
        path: String,
    ) -> Result<LinkedFolder, SessionError> {
        self.core.link_folder(&space_id, &path)
    }

    pub fn unlink_folder(&self, space_id: String) -> Result<(), SessionError> {
        self.core.unlink_folder(&space_id)
    }

    pub fn trash(&self, path: String) -> Result<Trash, SessionError> {
        self.core.trash(&path)
    }

    pub fn versions(&self, path: String) -> Result<FileVersions, SessionError> {
        self.core.versions(&path)
    }

    pub fn restore_deleted(
        &self,
        path: String,
        node_id: String,
    ) -> Result<RestoreAsked, SessionError> {
        self.core.restore_deleted(&path, &node_id)
    }

    pub fn restore_version(
        &self,
        path: String,
        version_id: String,
    ) -> Result<RestoreAsked, SessionError> {
        self.core.restore_version(&path, &version_id)
    }

    pub fn restore_requests(&self, path: String) -> Result<Vec<RestoreRequest>, SessionError> {
        self.core.restore_requests(&path)
    }

    pub fn share_links(&self) -> Result<Vec<ShareLink>, SessionError> {
        self.core.share_links()
    }

    pub fn create_share_link(
        &self,
        path: String,
        expiry: ShareExpiry,
        password: Option<String>,
    ) -> Result<NewShareLink, SessionError> {
        self.core.create_share_link(&path, expiry, password)
    }

    pub fn revoke_share_link(&self, link_id: String) -> Result<ShareLink, SessionError> {
        self.core.revoke_share_link(&link_id)
    }

    pub fn answer(&self, question_id: String, answer: Answer) -> Result<(), SessionError> {
        self.core.answer(&question_id, answer)
    }

    pub fn dismiss_notice(&self, notice_id: String) -> Result<(), SessionError> {
        self.core.dismiss_notice(&notice_id)
    }

    pub fn telemetry(&self) -> TelemetrySetting {
        self.core.telemetry()
    }

    pub fn set_telemetry(&self, enabled: bool) -> Result<(), SessionError> {
        self.core.set_telemetry(enabled)
    }
}
