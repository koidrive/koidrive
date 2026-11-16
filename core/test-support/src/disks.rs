// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const MOUNTS: &str = "KOIDRIVE_TEST_MOUNTS";

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
            let image = self.base().join(format!("{name}.dmg"));
            let args = [
                "create", "-quiet", "-size", "16m", "-fs", "APFS", "-volname", name,
            ];
            let mut args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
            args.push(image.as_os_str());
            run("hdiutil", &args);
            return image;
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
        run(
            "hdiutil",
            &[
                "detach".as_ref(),
                "-quiet".as_ref(),
                "-force".as_ref(),
                at.as_ref(),
            ],
        );
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
