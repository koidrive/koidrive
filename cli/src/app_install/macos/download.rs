use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::config::Config;
use crate::app_install::macos::control::{Control, io_error, record, step};
use crate::app_install::macos::filesystem::identity;
use crate::app_install::macos::state::{OwnedPath, VerifiedDmg};
use semver::Version;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn checksum(bytes: &[u8], asset: &str) -> Result<[u8; 32], AppInstallError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| failure(AppInstallFailure::Verification, "SHA256SUMS is not UTF-8"))?;
    let mut found = None;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 2 || fields[1].strip_prefix('*').unwrap_or(fields[1]) != asset {
            continue;
        }
        if found.is_some()
            || fields.len() != 2
            || fields[0].len() != 64
            || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(failure(
                AppInstallFailure::Verification,
                "Duplicate or malformed asset checksum",
            ));
        }
        let mut digest = [0; 32];
        for (i, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&fields[0][i * 2..i * 2 + 2], 16).map_err(|_| {
                failure(AppInstallFailure::Verification, "Malformed asset checksum")
            })?;
        }
        found = Some(digest);
    }
    found.ok_or_else(|| {
        failure(
            AppInstallFailure::Verification,
            "Target asset missing from SHA256SUMS",
        )
    })
}
pub(super) async fn download(
    config: &Config,
    target: &Version,
    image: &OwnedPath,
    control: &Control,
) -> Result<VerifiedDmg, AppInstallError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("Too many release redirects")
            } else if attempt.url().scheme() != "https" {
                attempt.error("Insecure release redirect")
            } else if !attempt.url().username().is_empty() || attempt.url().password().is_some() {
                attempt.error("Release redirect contains credentials")
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(config.limits.stall)
        .referer(false)
        .no_proxy()
        .build()
        .map_err(|error| failure(AppInstallFailure::Download, error))?;
    let asset = format!("Koidrive-{target}-arm64.dmg");
    let base = format!("{}/v{target}", config.release);
    let sums = fetch(
        &client,
        &format!("{base}/SHA256SUMS"),
        1024 * 1024,
        None,
        config.limits.stall,
        control,
    )
    .await?;
    let expected = checksum(&sums, &asset)?;
    let path = image.path.clone();
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&path)
        .map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if identity(&metadata) != image.identity || metadata.nlink() != 1 || !metadata.is_file() {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Download file changed",
        ));
    }
    let digest = fetch(
        &client,
        &format!("{base}/{asset}"),
        config.max_download,
        Some(file),
        config.limits.stall,
        control,
    )
    .await?;
    if digest.as_slice() != expected {
        return Err(failure(
            AppInstallFailure::Verification,
            "Downloaded app checksum does not match SHA256SUMS",
        ));
    }
    Ok(VerifiedDmg(path))
}
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    limit: u64,
    mut file: Option<File>,
    stall: Duration,
    control: &Control,
) -> Result<Vec<u8>, AppInstallError> {
    let name = url.rsplit('/').next().unwrap_or(url);
    let what = format!("Downloading {name}");
    let received = AtomicU64::new(0);
    let total = AtomicU64::new(0);
    let progress = || {
        let mb = |bytes: u64| bytes as f64 / 1_000_000.0;
        match total.load(Ordering::Relaxed) {
            0 => format!("{:.1} MB", mb(received.load(Ordering::Relaxed))),
            all => format!(
                "{:.1} of {:.1} MB",
                mb(received.load(Ordering::Relaxed)),
                mb(all)
            ),
        }
    };
    let transfer = async {
        let mut response = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| download_error(name, "could not start", stall, &error))?;
        total.store(response.content_length().unwrap_or(0), Ordering::Relaxed);
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(failure(
                AppInstallFailure::Download,
                "Release asset exceeds download limit",
            ));
        }
        let mut count = 0u64;
        let mut bytes = Vec::new();
        let mut hash = Sha256::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            download_error(name, &format!("stopped at {}", progress()), stall, &error)
        })? {
            control.check()?;
            count = count.saturating_add(chunk.len() as u64);
            received.store(count, Ordering::Relaxed);
            if count > limit {
                return Err(failure(
                    AppInstallFailure::Download,
                    "Release asset exceeds download limit",
                ));
            }
            if let Some(file) = file.as_mut() {
                file.write_all(&chunk).map_err(io_error)?;
                hash.update(&chunk);
            } else {
                bytes.extend_from_slice(&chunk);
            }
        }
        if let Some(file) = file {
            file.sync_all().map_err(io_error)?;
            Ok(hash.finalize().to_vec())
        } else {
            Ok(bytes)
        }
    };
    let bounded = async {
        control
            .bounded(&what, control.remaining(), transfer)
            .await
            .map_err(|mut error| {
                if error.kind == AppInstallFailure::Deadline {
                    error.message = format!(
                        "{what} was still running at {} when the app update ran out of time",
                        progress()
                    );
                }
                error
            })
    };
    let result = step(&what, bounded).await;
    if result.is_ok() {
        record(&format!("  received {}", progress()));
    }
    result
}

fn download_error(
    name: &str,
    what: &str,
    stall: Duration,
    error: &reqwest::Error,
) -> AppInstallError {
    let why = match (error.is_connect(), error.is_timeout()) {
        (true, _) => format!("could not connect: {error}"),
        (false, true) => format!("no data arrived for {} seconds", stall.as_secs()),
        _ => error.to_string(),
    };
    failure(
        AppInstallFailure::Download,
        format!("The download of {name} {what}: {why}"),
    )
}
