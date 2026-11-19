use crate::app_install::error::{AppInstallError, AppInstallFailure};
use crate::app_install::facts::{AppLaunch, CommitChange, CommitFact};
use crate::app_install::macos::attempt::Attempt;
use crate::app_install::macos::config::{Config, Limits};
use crate::app_install::macos::constants::{
    BUNDLE, CLEANUP, IDENTIFIER, INSPECT_RUNNING, REQUIREMENT, SIGNER,
};
use crate::app_install::macos::control::{Control, Registry, active, passed, process};
use crate::app_install::macos::filesystem::{directory, lock, observed, private_work, regular_at};
use crate::app_install::macos::lifecycle::quiesce;
use crate::app_install::macos::receipt::begin_receipt;
use crate::app_install::macos::recovery::refresh_tokens;
use crate::app_install::macos::state::{Lifecycle, OwnedPath, ReceiptData};
use crate::app_install::macos::transaction::{
    ensure_registered, ensure_with, exchange, stage_copy,
};
use crate::app_install::macos::verification::{
    VerifiedHelper, current, inventory, safe_tree, signature_args, unchanged_bundle, verify_helper,
};
use crate::app_install::outcome::{AppChange, AppInstall, AppLaunchResult, AppSkip, RetiredApp};
use crate::app_install::{cancel_active, skip_reason};
use crate::program::Arg;
use rustix::fs::FlockOperation;
use semver::Version;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::Instant;
// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn review_killed_transaction_child() {
    let Some(base) = std::env::var_os("KOI_REVIEW_KILL_BASE").map(PathBuf::from) else {
        return;
    };
    let config = Config {
        system: base.join("system"),
        user: base.join("user"),
        work: base.join("work"),
        release: std::env::var("KOI_REVIEW_KILL_RELEASE").unwrap(),
        codesign: base.join("tools/codesign"),
        spctl: base.join("tools/spctl"),
        hdiutil: base.join("tools/hdiutil"),
        ditto: "/usr/bin/ditto".into(),
        osascript: base.join("tools/osascript"),
        open: base.join("tools/open"),
        updater: base.join("updater"),
        max_download: 1024,
        limits: Limits::default(),
    };
    ensure_with(
        config,
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(120),
    )
    .await
    .unwrap();
}

struct ForgedDeveloperId {
    temp: tempfile::TempDir,
    search_list: Option<Vec<String>>,
}

impl ForgedDeveloperId {
    const IDENTITY: &str = "Developer ID Application: Gunwoo Kim (656SPP5XR8)";
    const PASSWORD: &str = "forged";

    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("koi-forged-developer-id-")
            .tempdir()
            .unwrap();
        let dir = temp.path().to_path_buf();
        fs::write(
            dir.join("leaf.cnf"),
            format!(
                "[req]\ndistinguished_name=dn\nprompt=no\n[dn]\nCN={}\nOU=656SPP5XR8\nO=Gunwoo Kim\nC=US\n[leaf]\nbasicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=critical,codeSigning\n1.2.840.113635.100.6.1.13=critical,DER:0500\n",
                Self::IDENTITY
            ),
        )
        .unwrap();
        let mut forged = Self {
            temp,
            search_list: None,
        };
        let keychain = forged.keychain();
        succeed(
            std::process::Command::new("/usr/bin/openssl")
                .current_dir(&dir)
                .args([
                    "req",
                    "-x509",
                    "-newkey",
                    "rsa:2048",
                    "-nodes",
                    "-days",
                    "1",
                    "-config",
                    "leaf.cnf",
                    "-extensions",
                    "leaf",
                    "-keyout",
                    "key.pem",
                    "-out",
                    "cert.pem",
                ]),
        );
        succeed(
            std::process::Command::new("/usr/bin/openssl")
                .current_dir(&dir)
                .args([
                    "pkcs12",
                    "-export",
                    "-inkey",
                    "key.pem",
                    "-in",
                    "cert.pem",
                    "-out",
                    "identity.p12",
                    "-passout",
                ])
                .arg(format!("pass:{}", Self::PASSWORD)),
        );
        for args in [
            vec!["create-keychain", "-p", Self::PASSWORD],
            vec!["unlock-keychain", "-p", Self::PASSWORD],
        ] {
            succeed(
                std::process::Command::new("/usr/bin/security")
                    .args(args)
                    .arg(&keychain),
            );
        }
        succeed(
            std::process::Command::new("/usr/bin/security")
                .arg("import")
                .arg(dir.join("identity.p12"))
                .arg("-k")
                .arg(&keychain)
                .args([
                    "-f",
                    "pkcs12",
                    "-P",
                    Self::PASSWORD,
                    "-T",
                    "/usr/bin/codesign",
                ]),
        );
        succeed(
            std::process::Command::new("/usr/bin/security")
                .args([
                    "set-key-partition-list",
                    "-S",
                    "apple-tool:,apple:,codesign:",
                    "-s",
                    "-k",
                    Self::PASSWORD,
                ])
                .arg(&keychain),
        );
        if std::env::var_os("RUNNER_ENVIRONMENT").as_deref() == Some(OsStr::new("github-hosted")) {
            let listed = std::process::Command::new("/usr/bin/security")
                .args(["list-keychains", "-d", "user"])
                .output()
                .unwrap();
            assert!(listed.status.success());
            let listed: Vec<String> = String::from_utf8(listed.stdout)
                .unwrap()
                .lines()
                .map(|line| line.trim().trim_matches('"').to_owned())
                .filter(|line| !line.is_empty())
                .collect();
            succeed(
                std::process::Command::new("/usr/bin/security")
                    .args(["list-keychains", "-d", "user", "-s"])
                    .arg(&keychain)
                    .args(&listed),
            );
            forged.search_list = Some(listed);
        }
        forged
    }

    fn keychain(&self) -> PathBuf {
        self.temp.path().join("forged.keychain-db")
    }

    fn sign(&self, path: &Path) {
        succeed(
            std::process::Command::new("/usr/bin/codesign")
                .args(["--force", "--timestamp=none", "--keychain"])
                .arg(self.keychain())
                .args(["--sign", Self::IDENTITY])
                .arg(path),
        );
    }

    fn app(&self) -> PathBuf {
        let app = self.temp.path().join(BUNDLE);
        for (directory, file) in [("MacOS", "Koidrive"), ("Helpers", "koi")] {
            fs::create_dir_all(app.join("Contents").join(directory)).unwrap();
            fs::copy(
                "/usr/bin/true",
                app.join("Contents").join(directory).join(file),
            )
            .unwrap();
        }
        let mut info = plist::Dictionary::new();
        info.insert(
            "CFBundleIdentifier".into(),
            plist::Value::String(IDENTIFIER.into()),
        );
        info.insert(
            "CFBundleExecutable".into(),
            plist::Value::String("Koidrive".into()),
        );
        plist::Value::Dictionary(info)
            .to_file_xml(app.join("Contents/Info.plist"))
            .unwrap();
        self.sign(&app.join("Contents/Helpers/koi"));
        self.sign(&app);
        app
    }
}

impl Drop for ForgedDeveloperId {
    fn drop(&mut self) {
        if let Some(listed) = &self.search_list {
            let _ = std::process::Command::new("/usr/bin/security")
                .args(["list-keychains", "-d", "user", "-s"])
                .args(listed)
                .output();
        }
        let _ = std::process::Command::new("/usr/bin/security")
            .arg("delete-keychain")
            .arg(self.keychain())
            .output();
    }
}

fn succeed(command: &mut std::process::Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

const TARGET: &str = "0.2.0-beta.3";

const DMG: &[u8] = b"A real byte fixture, not a fake digest.\x00\x01\xff";

pub(crate) async fn verified_helper_fixture(version: &str) -> (impl Drop, VerifiedHelper) {
    verified_helper_fixture_with_metadata(version, Some(version)).await
}

pub(crate) async fn verified_helper_fixture_with_metadata(
    version: &str,
    metadata: Option<&str>,
) -> (impl Drop, VerifiedHelper) {
    let f = Fixture::versioned(version, None, DMG.to_vec(), 200).await;
    bundle(&f.installed(), metadata);
    let binary = f.installed().join("Contents/Helpers/koi");
    script(
        &binary,
        &format!(
            "printf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {version}' ;;\n_reconcile) echo '{{\"data\":{{\"service\":{{\"manager\":\"launchd\",\"name\":\"fixture\",\"state\":\"running\"}}}}}}' ;;\nesac"
        ),
    );
    let verified = verify_helper(
        f.config.clone(),
        &binary,
        Instant::now() + Duration::from_secs(30),
    )
    .await
    .unwrap();
    (f, verified)
}

fn digest() -> String {
    Sha256::digest(DMG)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Fixture {
    temp: tempfile::TempDir,
    config: Config,
    requests: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn journal_to(path: &Path) {
    crate::app_install::journal(fs::File::create(path).unwrap());
}

fn script(path: &Path, text: &str) {
    let inspect = if path.file_name() == Some(OsStr::new("osascript")) {
        format!(
            r#"case "$*" in *inspectedBundlePath*) if [ -e '{}/running' ]; then echo running; else echo stopped; fi; exit 0;; esac"#,
            path.parent().unwrap().parent().unwrap().display()
        )
    } else {
        String::new()
    };
    fs::write(path, format!("#!/bin/sh\nset -eu\n{inspect}\n{text}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn bundle(path: &Path, version: Option<&str>) {
    fs::create_dir_all(path.join("Contents/Helpers")).unwrap();
    let mut dict = plist::Dictionary::new();
    dict.insert(
        "CFBundleIdentifier".into(),
        plist::Value::String(IDENTIFIER.into()),
    );
    dict.insert(
        "CFBundleShortVersionString".into(),
        plist::Value::String("0.2.0".into()),
    );
    if let Some(version) = version {
        dict.insert(
            "KoidriveVersion".into(),
            plist::Value::String(version.into()),
        );
    }
    plist::Value::Dictionary(dict)
        .to_file_xml(path.join("Contents/Info.plist"))
        .unwrap();
    fs::write(
        path.join("Contents/Helpers/koi"),
        version.unwrap_or("development"),
    )
    .unwrap();
    fs::write(path.join("Signer"), "656SPP5XR8").unwrap();
    fs::write(path.join("Notarization"), "Notarized Developer ID").unwrap();
}

impl Fixture {
    async fn new(sums: Option<String>, dmg: Vec<u8>, status: u16) -> Self {
        Self::versioned(TARGET, sums, dmg, status).await
    }

    async fn versioned(target: &str, sums: Option<String>, dmg: Vec<u8>, status: u16) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("koi-installer-test-")
            .tempdir()
            .unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let tools = base.join("tools");
        fs::create_dir(&tools).unwrap();
        let incoming = base.join("incoming").join(BUNDLE);
        bundle(&incoming, Some(target));
        let sums = sums.unwrap_or_else(|| format!("{}  Koidrive-{target}-arm64.dmg\n", digest()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let stall = base.join("download-stall");
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let observed = observed.clone();
                let sums = sums.clone();
                let dmg = dmg.clone();
                let stall = stall.clone();
                tokio::spawn(async move {
                    let mut request = vec![0; 16 * 1024];
                    let len = socket.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..len]);
                    observed.lock().unwrap().push(request.to_string());
                    let path = request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let body = if path.ends_with("/SHA256SUMS") {
                        sums.as_bytes()
                    } else {
                        &dmg
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    if stall.exists() && !path.ends_with("/SHA256SUMS") {
                        socket.write_all(&body[..body.len() / 2]).await.unwrap();
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                    socket.write_all(body).await.unwrap();
                });
            }
        });
        let config = Config {
            system: base.join("system"),
            user: base.join("user"),
            work: base.join("work"),
            release: format!("http://{address}/releases/download"),
            codesign: tools.join("codesign"),
            spctl: tools.join("spctl"),
            hdiutil: tools.join("hdiutil"),
            ditto: "/usr/bin/ditto".into(),
            osascript: tools.join("osascript"),
            open: tools.join("open"),
            updater: base.join("updater"),
            max_download: 1024,
            limits: Limits::default(),
        };
        fs::create_dir(&config.system).unwrap();
        fs::write(&config.updater, "signed updater").unwrap();
        script(
            &config.codesign,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}/codesign.log'
requirement=
while [ $# -gt 1 ]; do
    [ "$1" != -R ] || requirement=$2
    shift
done
if [ "$1" = '{}' ]; then
    [ "$requirement" = '={SIGNER}' ]
else
    [ "$requirement" = '={REQUIREMENT}' ] && [ "$(cat "$1/Signer")" = '656SPP5XR8' ]
fi
"#,
                base.display(),
                config.updater.display()
            ),
        );
        script(
            &config.spctl,
            r#"
for last do :; done
printf '%s: accepted\nsource=%s\n' "$last" "$(cat "$last/Notarization")" >&2
"#,
        );
        script(
            &config.hdiutil,
            &format!(
                r#"
base='{}'
printf '%s\n' "$*" >> "$base/hdi.log"
case "$1" in
attach)
    while [ "$1" != '-mountpoint' ]; do shift; done
    point="$2"
    shift 2
    for image do :; done
    /usr/bin/ditto "$base/incoming/Koidrive.app" "$point/Koidrive.app"
    printf '%s\n%s\n' "$point" "$image" > "$base/mounted"
    [ ! -e "$base/attach-fail" ] || exit 7
    if [ -e "$base/attach-sleep" ]; then echo $$ > "$base/child.pid"; exec /bin/sleep 30; fi
    printf '<plist version="1.0"><dict/></plist>\n'
    ;;
info)
    if [ -e "$base/info-fail" ]; then exit 9; fi
    [ ! -e "$base/info-warn" ] || echo 'hdiutil: info: cannot find IO media' >&2
    if [ -e "$base/mounted" ]; then
        point=$(/usr/bin/head -n 1 "$base/mounted")
        image=$(/usr/bin/tail -n 1 "$base/mounted")
        printf '<plist version="1.0"><dict><key>images</key><array><dict><key>image-path</key><string>%s</string><key>system-entities</key><array><dict><key>mount-point</key><string>%s</string></dict></array></dict></array></dict></plist>\n' "$image" "$point"
    else
        printf '<plist version="1.0"><dict><key>images</key><array/></dict></plist>\n'
    fi
    ;;
detach)
    [ ! -e "$base/detach-fail" ] || exit 8
    if [ -e "$base/detach-block" ]; then
        echo $$ > "$base/detach.pid"
        while [ ! -e "$base/detach-release" ]; do /bin/sleep 0.02; done
    fi
    point=$(/usr/bin/head -n 1 "$base/mounted")
    /bin/rm -rf "$point/Koidrive.app"
    /bin/rm "$base/mounted"
    ;;
*) exit 11;;
esac
"#,
                base.display()
            ),
        );
        script(
            &config.osascript,
            &format!(
                r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
if [ -e "$base/quit-fail" ]; then exit 5; fi
if [ -e "$base/quit-hang" ]; then exec /bin/sleep 30; fi
if [ -e "$base/running" ]; then /bin/rm "$base/running"; echo running; else echo stopped; fi
"#,
                base.display()
            ),
        );
        script(
            &config.open,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}/open.log'
[ ! -e '{}/open-fail' ]
"#,
                base.display(),
                base.display()
            ),
        );
        Self {
            temp,
            config,
            requests,
            server,
        }
    }

    fn base(&self) -> PathBuf {
        fs::canonicalize(self.temp.path()).unwrap()
    }
    fn installed(&self) -> PathBuf {
        self.config.system.join(BUNDLE)
    }
    fn incoming(&self) -> PathBuf {
        self.base().join("incoming").join(BUNDLE)
    }
    fn flag(&self, name: &str) {
        fs::write(self.base().join(name), "yes").unwrap();
    }
    fn old(&self) {
        bundle(&self.installed(), Some("0.1.0"));
    }
    fn preserved(&self) {
        assert_eq!(
            fs::read(self.installed().join("Contents/Helpers/koi")).unwrap(),
            b"0.1.0"
        );
    }
    async fn run(&self) -> Result<AppInstall, AppInstallError> {
        ensure_with(
            self.config.clone(),
            Version::parse(TARGET).unwrap(),
            AppLaunch::Preserve,
            Instant::now() + Duration::from_secs(60),
        )
        .await
    }
    fn unmounted(&self) {
        assert!(!self.base().join("mounted").exists());
        if self.config.work.exists() {
            let entries: Vec<_> = fs::read_dir(&self.config.work)
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let mut receipts = 0;
            for entry in &entries {
                if entry.file_name() == ".koidrive-app-install.lock" {
                    continue;
                }
                assert!(entry.file_name().to_str().unwrap().starts_with("receipt-"));
                let data: ReceiptData =
                    serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
                assert!(!data.work.path.exists());
                let stage = data.stage.unwrap();
                if let Some(previous) = stage.transfer.as_ref().unwrap().previous() {
                    assert_eq!(
                        observed(&stage.parent.path.join(BUNDLE)).unwrap(),
                        Some(previous.identity)
                    );
                } else {
                    assert!(matches!(data.lifecycle, Lifecycle::Reopening(_)));
                    assert_eq!(observed(&stage.parent.path.join(BUNDLE)).unwrap(), None);
                    assert_eq!(
                        fs::read(self.installed().join("Contents/Helpers/koi")).unwrap(),
                        data.target.as_bytes()
                    );
                }
                receipts += 1;
            }
            assert_eq!(entries.len(), receipts + 1);
        }
    }
}

async fn pid_ready(path: PathBuf) -> i32 {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(pid) = fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

async fn retired_fixture() -> (Fixture, RetiredApp) {
    let fixture = Fixture::new(None, DMG.to_vec(), 200).await;
    fixture.old();
    let AppInstall::Present { retired, .. } = fixture.run().await.unwrap() else {
        panic!()
    };
    (fixture, retired.into_iter().next().unwrap())
}

pub(crate) async fn pending_retirements_fixture() -> (impl Drop, PathBuf, Vec<RetiredApp>) {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    bundle(&f.config.user.join(BUNDLE), Some("0.1.0"));
    let path = f.installed();
    let AppInstall::Present {
        retired, residue, ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 2);
    let mut paths: Vec<_> = retired.iter().map(|token| token.bundle.clone()).collect();
    drop(retired);
    let AppInstall::Present {
        change,
        retired,
        residue,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(change, AppChange::Unchanged);
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 2);
    let mut replayed: Vec<_> = retired.iter().map(|token| token.bundle.clone()).collect();
    paths.sort();
    replayed.sort();
    assert_eq!(paths, replayed);
    (f, path, retired)
}

mod verification;

mod alternatives;

mod recovery;

mod lifecycle;

mod installation;

mod control;

mod retirement;
