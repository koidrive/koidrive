// SPDX-License-Identifier: Apache-2.0

use koidrive_cloudflare::Bundle;

use super::SERVER_BUNDLE;
#[cfg(feature = "test-setup")]
use super::testing;
use crate::error::CliError;

/// Fails when this koi has no embedded server bundle. The commands that find, deploy or upgrade a server call it before
/// the Cloudflare login, so that nobody authorizes a login that cannot be used.
pub(super) fn bundled() -> Result<(), String> {
    #[cfg(feature = "test-setup")]
    if testing::config()?.is_some() {
        return Ok(());
    }
    if SERVER_BUNDLE.is_empty() {
        return Err("this koi was built without a server bundle, so it cannot find, deploy or upgrade a server; build it with KOIDRIVE_SERVER_BUNDLE as docs/guide/install.md describes".into());
    }
    Ok(())
}

/// The embedded server bundle, read through [`Bundle::from_dir`] from a private copy, so the release's bundle passes the
/// same checks as one on disk.
pub(crate) fn embedded_bundle() -> Result<Bundle, CliError> {
    #[cfg(feature = "test-setup")]
    if let Some(testing) = testing::config()? {
        return Bundle::from_dir(&testing.bundle).map_err(|error| error.to_string().into());
    }
    bundled()?;
    let broken = |error: &dyn std::fmt::Display| {
        CliError::from(format!("this koi's server bundle is broken: {error}"))
    };
    let dir = tempfile::tempdir()
        .map_err(|error| CliError::from(format!("cannot unpack the server bundle: {error}")))?;
    for (path, bytes) in SERVER_BUNDLE {
        let target = dir.path().join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| broken(&error))?;
        }
        std::fs::write(&target, bytes).map_err(|error| broken(&error))?;
    }
    Bundle::from_dir(dir.path()).map_err(|error| broken(&error))
}
