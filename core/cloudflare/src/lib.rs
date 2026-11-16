// SPDX-License-Identifier: Apache-2.0

//! Cloudflare's OAuth login and the Workers and R2 API calls that deploy, find, upgrade and remove a Koidrive server
//! (docs/design/cli-setup.md). It holds no Koidrive protocol logic.

mod api;
mod bundle;
mod oauth;
mod secret;

pub use api::{
    Account, ApiError, Binding, Bucket, Cloudflare, CloudflareMessage, Jurisdiction, ObjectPage,
    Script, ScriptSettings,
};
pub use bundle::{
    Asset, Bundle, BundleError, Manifest, NewerDeployment, RECOVERY_KEY_SECRET, SERVER_TAG,
    Storage, UploadPlan,
};
pub use oauth::{
    AuthorizationCode, CLIENT_ID, Endpoints, Grant, Login, Loopback, OauthError, SCOPES,
    http_client,
};
pub use secret::Secret;
