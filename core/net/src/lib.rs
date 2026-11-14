// SPDX-License-Identifier: Apache-2.0

//! Network client for a Koidrive server, the client side of spec v0 and RFC 0001 to 0003. It does I/O only and has
//! no dependency on the sync engine: requests and events are `koidrive-proto` types, and a driver feeds them to the
//! engine and stores what must survive a restart (credentials, the stream position, the commit slot).
//!
//! - [`Api`] calls the HTTP endpoints of spec v0 section 15.1, and [`Auth`] keeps a device's access token fresh.
//! - [`Session`] is the WebSocket session with one space: RPCs, the change stream as [`Event`]s, `Reauth`, and
//!   reconnects with backoff that resend only what RFC 0002 section 2 allows.
//! - [`Transfer`] uploads and downloads chunks through the targets the server hands out, checking every chunk
//!   against its SHA-256.
//!
//! TLS is rustls with the platform verifier, for HTTP (through reqwest) and for the WebSocket alike.

mod api;
mod auth;
mod backoff;
mod chunks;
mod create;
mod error;
mod event;
mod session;
mod stream;
mod transport;

pub use api::Api;
pub use auth::{
    Auth, CredentialStore, Credentials, DEFAULT_REFRESH_MARGIN, ROTATION_PERIOD, Reload, Staged,
    Stored,
};
pub use backoff::Backoff;
pub use chunks::{
    ChunkError, DEFAULT_UPLOAD_CONCURRENCY, MAX_CHUNKS_PER_CALL, Transfer, UploadGate, matches,
};
pub use create::{request_id, resend_lost};
pub use error::{ApiError, CallError, NotResent, Resend, ServerError, StopReason};
pub use event::{Committed, DisconnectReason, Event};
pub use session::{
    Events, MAX_CLIENT_MESSAGE, MAX_SERVER_MESSAGE, PROTO_MAX, PROTO_MIN, Session, SessionConfig,
};
pub use transport::{Cause, Step, Transport, chain};
