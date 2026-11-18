// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
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

fn hdiutil(args: &[&OsStr]) -> String {
    until_ready(std::time::Duration::from_secs(120), || {
        output("hdiutil", args)
    })
    .unwrap_or_else(|failure| panic!("{failure}"))
}

fn until_ready(
    deadline: std::time::Duration,
    mut command: impl FnMut() -> Result<String, String>,
) -> Result<String, String> {
    let started = std::time::Instant::now();
    let mut refusals = Vec::new();
    loop {
        match command() {
            Ok(out) => {
                if let Some(last) = refusals.last() {
                    let _ = writeln!(
                        std::io::stderr(),
                        "succeeded after {} refusals in {:?}, the last: {last}",
                        refusals.len(),
                        started.elapsed()
                    );
                }
                return Ok(out);
            }
            Err(failure)
                if started.elapsed() < deadline
                    && ["Resource busy", "Permission denied"]
                        .iter()
                        .any(|transient| failure.contains(transient)) =>
            {
                refusals.push(failure);
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(failure) => return Err(failure),
        }
    }
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
    image: PathBuf,
    #[cfg(target_os = "macos")]
    device: String,
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
        self.ext4(name, &["mkfs.ext4"])
    }

    /// Creates an empty ext4 image of 16 MB named `name` with the UUID `uuid`, formatted while the clock read
    /// `formatted_at` seconds since the epoch, so its root folder has that birth time.
    #[cfg(target_os = "linux")]
    pub fn ext4_with_uuid(&self, name: &str, uuid: &str, formatted_at: u64) -> PathBuf {
        let clock = format!("E2FSPROGS_FAKE_TIME={formatted_at}");
        self.ext4(name, &[&clock, "mkfs.ext4", "-U", uuid])
    }

    fn ext4(&self, name: &str, mkfs: &[&str]) -> PathBuf {
        let image = self.base().join(format!("{name}.img"));
        run("truncate", &["-s".as_ref(), "16M".as_ref(), image.as_ref()]);
        let options = ["-q".as_ref(), "-F".as_ref(), image.as_os_str()];
        let args: Vec<&OsStr> = mkfs.iter().map(OsStr::new).chain(options).collect();
        run("env", &args);
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

    /// Creates an empty FAT image of 16 MB named `name`, the file system most USB sticks ship with, with a serial
    /// number of its own.
    pub fn fat(&self, name: &str) -> PathBuf {
        if cfg!(target_os = "macos") {
            return self.formatted(name, "MS-DOS");
        }
        let image = self.base().join(format!("{name}.fat"));
        self.linux_fat(&image, serial(&image))
    }

    /// Creates an empty FAT image of 16 MB named `name` whose serial number is 0, which no system reports as an ID.
    #[cfg(target_os = "linux")]
    pub fn fat_without_serial(&self, name: &str) -> PathBuf {
        self.linux_fat(&self.base().join(format!("{name}.fat")), 0)
    }

    fn linux_fat(&self, image: &Path, serial: u32) -> PathBuf {
        let serial = format!("{serial:08x}");
        run("truncate", &["-s".as_ref(), "16M".as_ref(), image.as_ref()]);
        run(
            "mkfs.vfat",
            &["-i".as_ref(), serial.as_ref(), image.as_ref()],
        );
        image.to_path_buf()
    }

    /// Whether exFAT volumes have IDs here. On Linux only udev reads an exFAT volume's serial number, so that takes
    /// `/dev/disk/by-diskseq` links and `mkfs.exfat` from exfatprogs.
    pub fn exfat_has_ids(&self) -> bool {
        if cfg!(target_os = "macos") {
            return true;
        }
        if !Path::new("/dev/disk/by-diskseq").is_dir() {
            eprintln!(
                "SKIPPED: udev keeps no /dev/disk/by-diskseq links here, so exFAT volumes have no ID"
            );
            return false;
        }
        if Command::new("mkfs.exfat").arg("-V").output().is_err() {
            eprintln!("SKIPPED: mkfs.exfat from exfatprogs is not installed");
            return false;
        }
        true
    }

    /// Creates an empty exFAT image of 16 MB named `name`. On Linux it takes `mkfs.exfat` from exfatprogs.
    pub fn exfat(&self, name: &str) -> PathBuf {
        if cfg!(target_os = "macos") {
            return self.formatted(name, "ExFAT");
        }
        let image = self.base().join(format!("{name}.exfat"));
        run("truncate", &["-s".as_ref(), "16M".as_ref(), image.as_ref()]);
        run("mkfs.exfat", &[image.as_ref()]);
        image
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
            args.push("-fsargs".into());
            args.push(format!("-I {:#x}", serial(&image)).into());
        }
        args.push(image.clone().into());
        hdiutil(&args.iter().map(OsString::as_os_str).collect::<Vec<_>>());
        image
    }

    /// A disk image next to the file system image `volume` whose one partition holds a copy of it, the way USB sticks
    /// are laid out. [`Disks::attach`] mounts that partition.
    #[cfg(target_os = "linux")]
    pub fn partitioned(&self, volume: &Path) -> PathBuf {
        let image = volume.with_extension("disk");
        let size = fs::metadata(volume).unwrap().len() + (1 << 20);
        run(
            "truncate",
            &["-s".as_ref(), size.to_string().as_ref(), image.as_ref()],
        );
        run(
            "sh",
            &[
                "-c".as_ref(),
                "echo 'start=2048, type=7' | sfdisk -q \"$0\"".as_ref(),
                image.as_ref(),
            ],
        );
        let (from, to) = (
            format!("if={}", volume.display()),
            format!("of={}", image.display()),
        );
        let copy = [&from, &to, "bs=1M", "seek=1", "conv=notrunc", "status=none"];
        run("dd", &copy.map(OsStr::new));
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
        #[cfg(target_os = "macos")]
        {
            let attached = hdiutil(&[
                "attach".as_ref(),
                "-plist".as_ref(),
                "-nobrowse".as_ref(),
                "-noautoopen".as_ref(),
                "-mountpoint".as_ref(),
                at.as_os_str(),
                image.as_os_str(),
            ]);
            let device = whole_device(&attached)
                .unwrap_or_else(|failure| panic!("{} attached: {failure}", image.display()));
            self.mounted.push(Mounted {
                at: at.to_path_buf(),
                image: std::path::absolute(image).unwrap(),
                device,
            });
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.mounted.push(Mounted {
                at: at.to_path_buf(),
                image: fs::canonicalize(image).unwrap(),
            });
            if image
                .extension()
                .is_some_and(|extension| extension == "disk")
            {
                let device = run(
                    "sudo",
                    &[
                        "-n".as_ref(),
                        "losetup".as_ref(),
                        "--partscan".as_ref(),
                        "--find".as_ref(),
                        "--show".as_ref(),
                        image.as_ref(),
                    ],
                );
                let partition = format!("{device}p1");
                let started = std::time::Instant::now();
                while !Path::new(&partition).exists() {
                    assert!(
                        started.elapsed() < std::time::Duration::from_secs(10),
                        "{partition} never appeared"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let options = format!(
                    "uid={},gid={}",
                    run("id", &["-u".as_ref()]),
                    run("id", &["-g".as_ref()])
                );
                sudo(&[
                    "mount".as_ref(),
                    "-o".as_ref(),
                    options.as_ref(),
                    partition.as_ref(),
                    at.as_ref(),
                ]);
                sudo(&["losetup".as_ref(), "-d".as_ref(), device.as_ref()]);
            } else if image
                .extension()
                .is_some_and(|extension| extension == "fat" || extension == "exfat")
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

fn serial(image: &Path) -> u32 {
    let mut serial = DefaultHasher::new();
    image.hash(&mut serial);
    serial.finish() as u32
}

fn sudo(args: &[&OsStr]) {
    let mut all: Vec<&OsStr> = vec!["-n".as_ref()];
    all.extend_from_slice(args);
    run("sudo", &all);
}

#[cfg(target_os = "macos")]
fn whole_device(attached: &str) -> Result<String, String> {
    let attached =
        plist::Value::from_reader_xml(attached.as_bytes()).map_err(|error| error.to_string())?;
    attached
        .as_dictionary()
        .and_then(|attached| attached.get("system-entities"))
        .and_then(plist::Value::as_array)
        .ok_or("hdiutil attach has no system-entities")?
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
        .map(|(_, device)| device.to_owned())
        .ok_or_else(|| "no whole device".to_owned())
}

#[cfg(target_os = "macos")]
fn image_on(info: &plist::Value, device: &str) -> Result<Option<PathBuf>, String> {
    let images = info
        .as_dictionary()
        .and_then(|info| info.get("images"))
        .and_then(plist::Value::as_array)
        .ok_or("hdiutil info has no images array")?;
    for attached in images {
        let attached = attached.as_dictionary().ok_or("invalid hdiutil image")?;
        let on_device = attached
            .get("system-entities")
            .and_then(plist::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(plist::Value::as_dictionary)
            .any(|entity| {
                entity.get("dev-entry").and_then(plist::Value::as_string) == Some(device)
            });
        if on_device {
            return Ok(attached
                .get("image-path")
                .and_then(plist::Value::as_string)
                .map(PathBuf::from));
        }
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn still_attached(mounted: &Mounted) -> Result<bool, String> {
    if !Path::new(&mounted.device).exists() {
        return Ok(false);
    }
    let info = output("hdiutil", &["info".as_ref(), "-plist".as_ref()])?;
    let info = plist::Value::from_reader_xml(info.as_bytes()).map_err(|error| error.to_string())?;
    Ok(image_on(&info, &mounted.device)?.is_none_or(|image| image == mounted.image))
}

#[cfg(target_os = "macos")]
fn unmount(mounted: &Mounted) -> Result<(), String> {
    let started = std::time::Instant::now();
    let mut busy = None;
    loop {
        let detached = if is_mount_point(&mounted.at)? {
            output("/sbin/umount", &["-f".as_ref(), mounted.at.as_os_str()])
        } else {
            Ok(String::new())
        }
        .and_then(|_| {
            output(
                "hdiutil",
                &[
                    "detach".as_ref(),
                    "-force".as_ref(),
                    mounted.device.as_ref(),
                ],
            )
        });
        let Err(failure) = detached else {
            break;
        };
        if !still_attached(mounted)? {
            break;
        }
        if !failure.contains("Resource busy") {
            return Err(failure);
        }
        if started.elapsed() >= std::time::Duration::from_secs(30) {
            return Err(format!(
                "{} remains attached as {}: {failure}",
                mounted.image.display(),
                mounted.device
            ));
        }
        busy.get_or_insert_with(std::time::Instant::now);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if let Some(busy) = busy {
        let _ = writeln!(
            std::io::stderr(),
            "{} detached after {:?} of Resource busy",
            mounted.image.display(),
            busy.elapsed()
        );
    }
    Ok(())
}

fn is_mount_point(at: &Path) -> Result<bool, String> {
    match (fs::metadata(at), at.parent().map(fs::metadata)) {
        (Ok(at), Some(Ok(parent))) => Ok(at.dev() != parent.dev()),
        (Err(error), _) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        (Err(error), _) | (_, Some(Err(error))) => Err(error.to_string()),
        _ => Err(format!("no parent for {}", at.display())),
    }
}

#[cfg(not(target_os = "macos"))]
fn unmount(mounted: &Mounted) -> Result<(), String> {
    if is_mount_point(&mounted.at)? {
        output(
            "sudo",
            &[
                "-n".as_ref(),
                "umount".as_ref(),
                "--lazy".as_ref(),
                mounted.at.as_ref(),
            ],
        )?;
    }
    let started = std::time::Instant::now();
    loop {
        let loops = output(
            "sudo",
            &[
                "-n".as_ref(),
                "losetup".as_ref(),
                "--associated".as_ref(),
                mounted.image.as_ref(),
            ],
        )?;
        if loops.is_empty() {
            return Ok(());
        }
        if started.elapsed() >= std::time::Duration::from_secs(30) {
            return Err(format!(
                "{} left {} but a file on it is still open: {loops}",
                mounted.image.display(),
                mounted.at.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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
                image: dir.path().join(format!("{name}.dmg")),
                #[cfg(target_os = "macos")]
                device: String::new(),
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
    fn a_refused_disk_command_is_tried_again_until_it_succeeds_or_fails_otherwise() {
        let mut answers = vec![
            Err("hdiutil: create failed - Resource busy".to_owned()),
            Err("hdiutil: attach failed - Permission denied".to_owned()),
            Ok("done".to_owned()),
        ]
        .into_iter();
        let ready = until_ready(std::time::Duration::from_secs(5), || {
            answers.next().unwrap()
        });
        assert_eq!(ready.as_deref(), Ok("done"));
        let mut tries = 0;
        let broken = until_ready(std::time::Duration::from_secs(5), || {
            tries += 1;
            Err("hdiutil: create failed - No space left on device".to_owned())
        });
        assert_eq!(tries, 1);
        assert!(broken.unwrap_err().contains("No space left"));
        let busy = until_ready(std::time::Duration::from_millis(600), || {
            Err("hdiutil: create failed - Resource busy".to_owned())
        });
        assert!(busy.unwrap_err().contains("Resource busy"));
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
        let attached = r#"<?xml version="1.0"?><plist version="1.0"><dict><key>system-entities</key><array><dict><key>dev-entry</key><string>/dev/disk6s1</string></dict><dict><key>dev-entry</key><string>/dev/disk6</string></dict><dict><key>dev-entry</key><string>/dev/disk5</string><key>content-hint</key><string>GUID_partition_scheme</string></dict></array></dict></plist>"#;
        assert_eq!(whole_device(attached).as_deref(), Ok("/dev/disk5"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_image_without_a_whole_device_is_not_detached_by_partition() {
        let attached = r#"<?xml version="1.0"?><plist version="1.0"><dict><key>system-entities</key><array><dict><key>dev-entry</key><string>/dev/disk6s1</string></dict></array></dict></plist>"#;
        assert!(whole_device(attached).is_err());
        assert!(whole_device("bad").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_device_belongs_to_the_image_hdiutil_lists_on_it() {
        let info = plist::Value::from_reader_xml(br#"<?xml version="1.0"?><plist version="1.0"><dict><key>images</key><array><dict><key>image-path</key><string>/fixture/other.dmg</string><key>system-entities</key><array><dict><key>dev-entry</key><string>/dev/disk5</string></dict></array></dict></array></dict></plist>"#.as_slice()).unwrap();
        assert_eq!(
            image_on(&info, "/dev/disk5"),
            Ok(Some(PathBuf::from("/fixture/other.dmg")))
        );
        assert_eq!(image_on(&info, "/dev/disk7"), Ok(None));
        assert!(image_on(&plist::Value::String("bad".into()), "/dev/disk5").is_err());
    }

    #[test]
    fn a_disk_with_a_file_open_on_it_is_ejected_and_mounts_again_with_its_files() {
        let Some(mut disks) = Disks::new() else {
            return;
        };
        let image = disks.image("Held");
        let at = disks.base().join("at");
        fs::create_dir(&at).unwrap();
        disks.attach(&image, &at);
        fs::write(at.join("open.txt"), b"held").unwrap();
        let held = fs::File::open(at.join("open.txt")).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            drop(held);
        });
        #[cfg(target_os = "macos")]
        let device = disks.mounted[0].device.clone();
        disks.detach(&at);
        assert_eq!(is_mount_point(&at), Ok(false));
        #[cfg(target_os = "macos")]
        assert!(!Path::new(&device).exists(), "{device}");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            run(
                "sudo",
                &[
                    "-n".as_ref(),
                    "losetup".as_ref(),
                    "--associated".as_ref(),
                    image.as_ref()
                ]
            ),
            ""
        );
        release.join().unwrap();
        disks.attach(&image, &at);
        assert_eq!(fs::read(at.join("open.txt")).unwrap(), b"held");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_busy_image_attached_through_a_symlinked_path_is_still_ejected() {
        let mut disks = Disks::new().unwrap();
        let image = disks.image("Spelled");
        let spelled = disks.base().join("Alias.dmg");
        std::os::unix::fs::symlink(&image, &spelled).unwrap();
        let at = disks.base().join("at");
        fs::create_dir(&at).unwrap();
        disks.attach(&spelled, &at);
        let device = disks.mounted[0].device.clone();
        let held = fs::File::open(&device).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(held);
        });
        disks.detach(&at);
        release.join().unwrap();
        assert!(!Path::new(&device).exists(), "{device}");
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
            let first = disks.mounted[0].device.clone();
            disks.detach(&moved.join("mount"));
            assert!(!Path::new(&first).exists(), "{format}");
            disks.attach(&image, &moved.join("mount"));
            assert_eq!(fs::read(moved.join("mount/held.txt")).unwrap(), b"saved");
            let second = disks.mounted[0].device.clone();
            let base = disks.base();
            drop(disks);
            assert!(!Path::new(&second).exists(), "{format}");
            assert!(!base.exists(), "{format}");
        }
    }
}
