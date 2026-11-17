// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;
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

fn output(program: &str, args: &[&OsStr]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("{program} {args:?}: {error}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn run(program: &str, args: &[&OsStr]) -> String {
    output(program, args).unwrap_or_else(|failure| panic!("{failure}"))
}

/// Disk images mounted into folders the way a person mounts a USB drive: APFS images through `hdiutil` on macOS,
/// and ext4 images through `sudo -n mount -o loop` on Linux. Drop attempts to detach every image and
/// retains the directory without panicking if the OS cannot release one.
pub struct Disks {
    dir: tempfile::TempDir,
    mounted: Vec<Mounted>,
}

struct Mounted {
    at: PathBuf,
    #[cfg(target_os = "macos")]
    image: PathBuf,
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
            "-ov",
            "-nospotlight",
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
        let stage = koidrive_state_file::AtomicFile::prepare(&copy).unwrap();
        std::io::copy(&mut fs::File::open(image).unwrap(), &mut stage.as_file()).unwrap();
        stage.finish(koidrive_state_file::Publish::Replace).unwrap();
        copy
    }

    /// Mounts `image` at the folder `at`, which must exist, and leaves its root writable by this user.
    pub fn attach(&mut self, image: &Path, at: &Path) {
        self.mounted.push(Mounted {
            at: at.to_path_buf(),
            #[cfg(target_os = "macos")]
            image: fs::canonicalize(image).unwrap(),
        });
        if cfg!(target_os = "macos") {
            run(
                "hdiutil",
                &[
                    "attach".as_ref(),
                    "-nobrowse".as_ref(),
                    "-noautoopen".as_ref(),
                    "-mountpoint".as_ref(),
                    at.as_os_str(),
                    image.as_os_str(),
                ],
            );
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
    }

    /// Unmounts the image mounted at `at`, as ejecting the disk does.
    pub fn detach(&mut self, at: &Path) {
        let index = self
            .mounted
            .iter()
            .position(|mounted| mounted.at == at)
            .unwrap_or_else(|| panic!("no image owned at {}", at.display()));
        if let Err(failure) = unmount(&self.mounted[index]) {
            panic!("{failure}");
        }
        self.mounted.remove(index);
    }

    /// Records that the image mounted at `from` is at `to` now, after a folder above it moved.
    pub fn moved(&mut self, from: &Path, to: &Path) {
        for mounted in &mut self.mounted {
            if let Ok(below) = mounted.at.strip_prefix(from) {
                mounted.at = to.join(below);
            }
        }
    }

    fn cleanup(&mut self, mut detach: impl FnMut(&Mounted) -> Result<(), String>) {
        let failures: Vec<String> = self
            .mounted
            .drain(..)
            .rev()
            .filter_map(|mounted| detach(&mounted).err())
            .collect();
        if !failures.is_empty() {
            self.dir.disable_cleanup(true);
            let _ = writeln!(
                std::io::stderr(),
                "Disk cleanup failed; keeping {} for the OS to reclaim: {failures:?}",
                self.dir.path().display()
            );
        }
    }
}

fn sudo(args: &[&OsStr]) {
    let mut all: Vec<&OsStr> = vec!["-n".as_ref()];
    all.extend_from_slice(args);
    run("sudo", &all);
}

#[cfg(target_os = "macos")]
fn attached_device(image: &Path) -> Result<Option<String>, String> {
    let info = output("hdiutil", &["info".as_ref(), "-plist".as_ref()])?;
    let info = plist::Value::from_reader_xml(info.as_bytes()).map_err(|error| error.to_string())?;
    device_for_image(&info, image)
}

#[cfg(target_os = "macos")]
fn device_for_image(info: &plist::Value, image: &Path) -> Result<Option<String>, String> {
    let images = info
        .as_dictionary()
        .and_then(|info| info.get("images"))
        .and_then(plist::Value::as_array)
        .ok_or("hdiutil info has no images array")?;
    for attached in images {
        let attached = attached.as_dictionary().ok_or("invalid hdiutil image")?;
        if attached
            .get("image-path")
            .and_then(plist::Value::as_string)
            .map(Path::new)
            != Some(image)
        {
            continue;
        }
        let entities = attached
            .get("system-entities")
            .and_then(plist::Value::as_array)
            .ok_or("hdiutil image has no system-entities")?;
        return entities
            .iter()
            .rev()
            .filter_map(plist::Value::as_dictionary)
            .filter_map(|entity| {
                let device = entity.get("dev-entry")?.as_string()?;
                device.strip_prefix("/dev/disk").filter(|number| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })?;
                let partition_map = entity
                    .get("content-hint")
                    .and_then(plist::Value::as_string)
                    .is_some_and(|hint| hint.ends_with("_partition_scheme"));
                Some((partition_map, device))
            })
            .max_by_key(|(partition_map, _)| *partition_map)
            .map(|(_, device)| Some(device.to_owned()))
            .ok_or_else(|| format!("no whole device for {}", image.display()));
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn unmount(mounted: &Mounted) -> Result<(), String> {
    let started = std::time::Instant::now();
    let mut diagnosed = false;
    while let Some(device) = attached_device(&mounted.image)? {
        if started.elapsed() >= std::time::Duration::from_secs(10) {
            return Err(format!(
                "{} remains attached as {device}",
                mounted.image.display()
            ));
        }
        let is_mounted = match (
            fs::metadata(&mounted.at),
            mounted.at.parent().map(fs::metadata),
        ) {
            (Ok(at), Some(Ok(parent))) => at.dev() != parent.dev(),
            (Err(error), _) if error.kind() == std::io::ErrorKind::NotFound => false,
            (Err(error), _) | (_, Some(Err(error))) => return Err(error.to_string()),
            _ => return Err(format!("no parent for {}", mounted.at.display())),
        };
        let detached = if is_mounted {
            output("/sbin/umount", &["-f".as_ref(), mounted.at.as_os_str()])
        } else {
            Ok(String::new())
        }
        .and_then(|_| {
            output(
                "hdiutil",
                &["detach".as_ref(), "-force".as_ref(), device.as_ref()],
            )
        });
        match detached {
            Ok(_) => {}
            Err(failure) => {
                if attached_device(&mounted.image)?.is_none() {
                    return Ok(());
                }
                if !failure.contains("Resource busy") {
                    return Err(failure);
                }
                if !diagnosed {
                    let holders = Command::new("lsof")
                        .args(["-nP", "+f", "--"])
                        .arg(&mounted.at)
                        .output();
                    let system_holders = Command::new("sudo")
                        .args(["-n", "lsof", "-nP", "+f", "--"])
                        .arg(&mounted.at)
                        .output();
                    for out in [holders, system_holders] {
                        let detail = out
                            .map(|out| {
                                format!(
                                    "{}{}",
                                    String::from_utf8_lossy(&out.stdout),
                                    String::from_utf8_lossy(&out.stderr)
                                )
                            })
                            .unwrap_or_else(|error| error.to_string());
                        let _ = writeln!(
                            std::io::stderr(),
                            "{failure}; holders at {}:\n{detail}",
                            mounted.at.display()
                        );
                    }
                    diagnosed = true;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn unmount(mounted: &Mounted) -> Result<(), String> {
    output(
        "sudo",
        &["-n".as_ref(), "umount".as_ref(), mounted.at.as_ref()],
    )
    .map(drop)
}

impl Drop for Disks {
    fn drop(&mut self) {
        self.cleanup(unmount);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(dir: tempfile::TempDir) -> Disks {
        let mounted = ["outer", "inner"]
            .into_iter()
            .map(|name| Mounted {
                at: dir.path().join(name),
                #[cfg(target_os = "macos")]
                image: dir.path().join(format!("{name}.dmg")),
            })
            .collect();
        Disks { dir, mounted }
    }

    #[test]
    fn cleanup_failure_keeps_files_and_does_not_replace_the_test_result() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::write(base.join("image.dmg"), "keep").unwrap();
        let mut disks = owned(dir);
        let mut tried = Vec::new();
        disks.cleanup(|mounted| {
            tried.push(mounted.at.file_name().unwrap().to_owned());
            Err("Resource busy".to_owned())
        });
        drop(disks);
        assert_eq!(tried, ["inner", "outer"]);
        assert_eq!(fs::read(base.join("image.dmg")).unwrap(), b"keep");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn successful_cleanup_removes_the_fixture_directory() {
        let mut disks = owned(tempfile::tempdir().unwrap());
        let base = disks.base();
        disks.cleanup(|_| Ok(()));
        drop(disks);
        assert!(!base.exists());
    }

    #[test]
    fn image_copies_publish_the_same_bytes_without_an_unfinished_stage() {
        let disks = Disks {
            dir: tempfile::tempdir().unwrap(),
            mounted: Vec::new(),
        };
        let image = disks.base().join("source.dmg");
        let bytes = vec![42; 16_384];
        fs::write(&image, &bytes).unwrap();
        let copy = disks.copy(&image, "copy");
        assert_eq!(fs::read(&copy).unwrap(), bytes);
        assert!(std::panic::catch_unwind(|| disks.copy(&disks.base().join("missing.dmg"), "unfinished")).is_err());
        assert!(!disks.base().join("unfinished.dmg").exists());
        let mut files: Vec<_> = fs::read_dir(disks.base())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        files.sort();
        assert_eq!(files, ["copy.dmg", "source.dmg"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_image_owns_the_physical_disk_not_its_apfs_volume() {
        let info = plist::Value::from_reader_xml(br#"<?xml version="1.0"?><plist version="1.0"><dict><key>images</key><array><dict><key>image-path</key><string>/fixture/test.dmg</string><key>system-entities</key><array><dict><key>dev-entry</key><string>/dev/disk6s1</string></dict><dict><key>dev-entry</key><string>/dev/disk6</string></dict><dict><key>dev-entry</key><string>/dev/disk5</string><key>content-hint</key><string>GUID_partition_scheme</string></dict></array></dict></array></dict></plist>"#.as_slice()).unwrap();
        assert_eq!(
            device_for_image(&info, Path::new("/fixture/test.dmg"))
                .unwrap()
                .as_deref(),
            Some("/dev/disk5")
        );
        assert_eq!(
            device_for_image(&info, Path::new("/fixture/other.dmg")).unwrap(),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_image_without_a_whole_device_is_not_detached_by_partition() {
        let info = plist::Value::from_reader_xml(br#"<?xml version="1.0"?><plist version="1.0"><dict><key>images</key><array><dict><key>image-path</key><string>/fixture/test.dmg</string><key>system-entities</key><array><dict><key>dev-entry</key><string>/dev/disk6s1</string></dict></array></dict></array></dict></plist>"#.as_slice()).unwrap();
        assert!(device_for_image(&info, Path::new("/fixture/test.dmg")).is_err());
        assert!(
            device_for_image(
                &plist::Value::String("bad".into()),
                Path::new("/fixture/test.dmg")
            )
            .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn images_detach_after_releasing_a_file_and_moving_the_parent() {
        for format in ["APFS", "MS-DOS", "ExFAT"] {
            let mut disks = Disks::new().unwrap();
            let image = disks.formatted("Held", format);
            let old = disks.base().join("parent");
            let moved = disks.base().join("moved");
            let mount = old.join("mount");
            fs::create_dir_all(&mount).unwrap();
            disks.attach(&image, &mount);
            fs::write(mount.join("held.txt"), b"saved").unwrap();
            let held = fs::File::open(mount.join("held.txt")).unwrap();
            drop(held);
            fs::rename(&old, &moved).unwrap();
            disks.moved(&old, &moved);
            disks.detach(&moved.join("mount"));
            assert_eq!(attached_device(&image).unwrap(), None, "{format}");
            disks.attach(&image, &moved.join("mount"));
            assert_eq!(fs::read(moved.join("mount/held.txt")).unwrap(), b"saved");
            let base = disks.base();
            drop(disks);
            assert_eq!(attached_device(&image).unwrap(), None, "{format}");
            assert!(!base.exists(), "{format}");
        }
    }
}
