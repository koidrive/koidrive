// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const MOUNTS: &str = "KOIDRIVE_TEST_MOUNTS";

const VOLUME_METADATA: [&str; 5] = [
    ".fseventsd",
    ".Spotlight-V100",
    ".Trashes",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
];

/// Whether `path` is in a folder the system writes on a volume it mounts whenever it likes, such as `.fseventsd`.
pub fn is_volume_metadata(path: &Path) -> bool {
    path.components()
        .any(|part| VOLUME_METADATA.iter().any(|name| part.as_os_str() == *name))
}

fn run(program: &str, args: &[&OsStr]) -> String {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Disk images mounted into folders the way a person mounts a USB drive: APFS images through `hdiutil` on macOS,
/// and ext4 images through `sudo -n mount -o loop` on Linux. Every image still mounted is detached on drop.
pub struct Disks {
    dir: tempfile::TempDir,
    mounted: Vec<PathBuf>,
}

impl Disks {
    /// `None` on Linux unless `KOIDRIVE_TEST_MOUNTS` is set, since mounting there takes `sudo` without a password.
    pub fn new() -> Option<Self> {
        if cfg!(target_os = "linux") && std::env::var_os(MOUNTS).is_none() {
            let _ = writeln!(
                std::io::stderr(),
                "SKIPPED: {MOUNTS} is not set, so the test may not mount file systems with sudo"
            );
            return None;
        }
        Some(Disks {
            dir: tempfile::tempdir().unwrap(),
            mounted: Vec::new(),
        })
    }

    /// A folder for the test's own files, outside every image.
    pub fn base(&self) -> PathBuf {
        fs::canonicalize(self.dir.path()).unwrap()
    }

    /// Creates an empty image of 16 MB named `name`.
    pub fn image(&self, name: &str) -> PathBuf {
        if cfg!(target_os = "macos") {
            return self.formatted(name, "APFS");
        }
        let image = self.base().join(format!("{name}.img"));
        run("truncate", &["-s".as_ref(), "16M".as_ref(), image.as_ref()]);
        run("mkfs.ext4", &["-q".as_ref(), "-F".as_ref(), image.as_ref()]);
        let at = self.base().join(format!("{name}.mkfs"));
        fs::create_dir(&at).unwrap();
        sudo(&[
            "mount".as_ref(),
            "-o".as_ref(),
            "loop".as_ref(),
            image.as_ref(),
            at.as_ref(),
        ]);
        sudo(&["rmdir".as_ref(), at.join("lost+found").as_ref()]);
        sudo(&["umount".as_ref(), at.as_ref()]);
        fs::remove_dir(&at).unwrap();
        image
    }

    /// Creates an empty FAT image of 16 MB named `name`, the file system most USB sticks ship with. Linux reports no
    /// UUID for it; macOS derives one from its serial number.
    pub fn fat(&self, name: &str) -> PathBuf {
        if cfg!(target_os = "macos") {
            return self.formatted(name, "MS-DOS");
        }
        let image = self.base().join(format!("{name}.fat"));
        run("truncate", &["-s".as_ref(), "16M".as_ref(), image.as_ref()]);
        run("mkfs.vfat", &[image.as_ref()]);
        image
    }

    /// Creates an empty exFAT image of 16 MB named `name`.
    #[cfg(target_os = "macos")]
    pub fn exfat(&self, name: &str) -> PathBuf {
        self.formatted(name, "ExFAT")
    }

    fn formatted(&self, name: &str, file_system: &str) -> PathBuf {
        let image = self.base().join(format!("{name}.dmg"));
        let mut args: Vec<OsString> = [
            "create",
            "-quiet",
            "-size",
            "16m",
            "-fs",
            file_system,
            "-volname",
            name,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        if file_system != "APFS" {
            let mut serial = DefaultHasher::new();
            image.hash(&mut serial);
            args.push("-fsargs".into());
            args.push(format!("-I {:#x}", serial.finish() as u32).into());
        }
        args.push(image.clone().into());
        run(
            "hdiutil",
            &args.iter().map(OsString::as_os_str).collect::<Vec<_>>(),
        );
        image
    }

    /// A byte-identical copy of `image` named `name`, whose files have the same inode numbers and birth times.
    pub fn copy(&self, image: &Path, name: &str) -> PathBuf {
        let copy = image
            .with_file_name(name)
            .with_extension(image.extension().unwrap());
        fs::copy(image, &copy).unwrap();
        copy
    }

    /// Mounts `image` at the folder `at`, which must exist, and leaves its root writable by this user.
    pub fn attach(&mut self, image: &Path, at: &Path) {
        if cfg!(target_os = "macos") {
            let args = ["attach", "-quiet", "-nobrowse", "-mountpoint"];
            let mut args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
            args.extend([at.as_os_str(), image.as_os_str()]);
            run("hdiutil", &args);
        } else if image
            .extension()
            .is_some_and(|extension| extension == "fat")
        {
            let options = format!(
                "loop,uid={},gid={}",
                run("id", &["-u".as_ref()]),
                run("id", &["-g".as_ref()])
            );
            sudo(&[
                "mount".as_ref(),
                "-o".as_ref(),
                options.as_ref(),
                image.as_ref(),
                at.as_ref(),
            ]);
        } else {
            sudo(&[
                "mount".as_ref(),
                "-o".as_ref(),
                "loop".as_ref(),
                image.as_ref(),
                at.as_ref(),
            ]);
            let owner = format!(
                "{}:{}",
                run("id", &["-u".as_ref()]),
                run("id", &["-g".as_ref()])
            );
            sudo(&["chown".as_ref(), owner.as_ref(), at.as_ref()]);
        }
        self.mounted.push(at.to_path_buf());
    }

    /// Unmounts the image mounted at `at`, as ejecting the disk does.
    pub fn detach(&mut self, at: &Path) {
        self.mounted.retain(|mounted| mounted != at);
        unmount(at);
    }

    /// Records that the image mounted at `from` is at `to` now, after a folder above it moved.
    pub fn moved(&mut self, from: &Path, to: &Path) {
        for mounted in &mut self.mounted {
            if let Ok(below) = mounted.strip_prefix(from) {
                *mounted = to.join(below);
            }
        }
    }
}

fn sudo(args: &[&OsStr]) {
    let mut all: Vec<&OsStr> = vec!["-n".as_ref()];
    all.extend_from_slice(args);
    run("sudo", &all);
}

fn unmount(at: &Path) {
    if cfg!(target_os = "macos") {
        let mut failures = Vec::new();
        while failures.len() < 10 {
            let out = Command::new("hdiutil")
                .args(["detach", "-force"])
                .arg(at)
                .output()
                .unwrap();
            if out.status.success() {
                return;
            }
            failures.push(String::from_utf8_lossy(&out.stderr).into_owned());
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        panic!("hdiutil detach -force {}: {failures:?}", at.display());
    } else {
        sudo(&["umount".as_ref(), at.as_ref()]);
    }
}

impl Drop for Disks {
    fn drop(&mut self) {
        for at in self.mounted.drain(..).rev() {
            unmount(&at);
        }
    }
}
