// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MOUNTS: &str = "KOIDRIVE_TEST_MOUNTS";
const EXFAT: &str = "KOIDRIVE_TEST_EXFAT";

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

#[cfg(target_os = "macos")]
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
    closed: bool,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Ejected {
        #[cfg(target_os = "macos")]
        device: String,
    },
    #[cfg(target_os = "macos")]
    KeptBusy { device: String },
}

#[derive(Clone)]
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
            closed: false,
        })
    }

    /// A folder for the test's own files, outside every image.
    pub fn base(&self) -> PathBuf {
        fs::canonicalize(self.dir.path()).unwrap()
    }

    /// Creates an empty image of 16 MB named `name`.
    pub fn image(&self, name: &str) -> PathBuf {
        if cfg!(target_os = "macos") {
            return self.formatted(name, "APFS", 16, None);
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
        self.fat_with_serial(name, 16, serial(&self.base().join(name)))
    }

    /// Creates an empty FAT image of `megabytes` MB named `name` with the serial number `serial`, like a stick
    /// formatted with another stick's serial number.
    pub fn fat_with_serial(&self, name: &str, megabytes: u32, serial: u32) -> PathBuf {
        if cfg!(target_os = "macos") {
            return self.formatted(name, "MS-DOS", megabytes, Some(serial));
        }
        self.linux_fat(&self.base().join(format!("{name}.fat")), megabytes, serial)
    }

    /// Creates an empty FAT image of 16 MB named `name` whose serial number is 0, which no system reports as an ID.
    #[cfg(target_os = "linux")]
    pub fn fat_without_serial(&self, name: &str) -> PathBuf {
        self.linux_fat(&self.base().join(format!("{name}.fat")), 16, 0)
    }

    fn linux_fat(&self, image: &Path, megabytes: u32, serial: u32) -> PathBuf {
        let (size, serial) = (format!("{megabytes}M"), format!("{serial:08x}"));
        run("truncate", &["-s".as_ref(), size.as_ref(), image.as_ref()]);
        run(
            "mkfs.vfat",
            &["-i".as_ref(), serial.as_ref(), image.as_ref()],
        );
        image.to_path_buf()
    }

    /// Whether exFAT volumes have IDs here. On Linux only udev reads an exFAT volume's serial number, so that takes
    /// udev's device records with their `/dev/disk/by-diskseq` links, an exFAT driver, and `mkfs.exfat` and
    /// `tune.exfat` from exfatprogs.
    pub fn exfat_has_ids(&self) -> bool {
        if cfg!(target_os = "macos") {
            return true;
        }
        let tools = ["mkfs.exfat", "tune.exfat"];
        let diskseq = std::fs::read_dir("/run/udev/data").is_ok_and(|records| {
            records.flatten().any(|record| {
                let text = std::fs::read_to_string(record.path()).unwrap_or_default();
                text.lines()
                    .any(|line| line.starts_with("S:disk/by-diskseq/"))
            })
        });
        let driver = std::fs::read_to_string("/proc/filesystems")
            .is_ok_and(|known| known.split_whitespace().any(|name| name == "exfat"))
            || (Command::new("modprobe")
                .args(["-n", "-q", "exfat"])
                .status())
            .is_ok_and(|status| status.success());
        let missing = match tools
            .iter()
            .find(|tool| Command::new(tool).arg("-V").output().is_err())
        {
            _ if !diskseq => {
                "udev records no /dev/disk/by-diskseq links here, so exFAT volumes have no ID"
                    .to_owned()
            }
            Some(tool) => format!("{tool} from exfatprogs is not installed"),
            None if !driver => "this kernel has no exFAT driver".to_owned(),
            None => return true,
        };
        assert!(
            std::env::var_os(EXFAT).is_none(),
            "{EXFAT} is set, but {missing}"
        );
        eprintln!("SKIPPED: {missing}");
        false
    }

    /// Creates an empty exFAT image of 16 MB named `name`. On Linux it takes `mkfs.exfat` from exfatprogs.
    pub fn exfat(&self, name: &str) -> PathBuf {
        self.exfat_with_serial(name, 16, serial(&self.base().join(name)))
    }

    /// Creates an empty exFAT image of `megabytes` MB named `name` with the serial number `serial`. On Linux it
    /// takes `mkfs.exfat` and `tune.exfat` from exfatprogs.
    pub fn exfat_with_serial(&self, name: &str, megabytes: u32, serial: u32) -> PathBuf {
        if cfg!(target_os = "macos") {
            return self.formatted(name, "ExFAT", megabytes, Some(serial));
        }
        let image = self.base().join(format!("{name}.exfat"));
        let (size, serial) = (format!("{megabytes}M"), format!("{serial:#x}"));
        run("truncate", &["-s".as_ref(), size.as_ref(), image.as_ref()]);
        run("mkfs.exfat", &[image.as_ref()]);
        run(
            "tune.exfat",
            &["-I".as_ref(), serial.as_ref(), image.as_ref()],
        );
        image
    }

    fn formatted(
        &self,
        name: &str,
        file_system: &str,
        megabytes: u32,
        serial: Option<u32>,
    ) -> PathBuf {
        let image = self.base().join(format!("{name}.dmg"));
        let size = format!("{megabytes}m");
        let mut args: Vec<OsString> = [
            "create",
            "-ov",
            "-nospotlight",
            "-size",
            &size,
            "-fs",
            file_system,
            "-volname",
            name,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        if let Some(serial) = serial {
            args.push("-fsargs".into());
            args.push(format!("-I {serial:#x}").into());
        }
        args.push(image.clone().into());
        let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        created(&image, || output("hdiutil", &args)).unwrap_or_else(|failure| panic!("{failure}"));
        image
    }

    /// A disk image next to the file system image `volume` whose one partition holds a copy of it, the way USB sticks
    /// are laid out. [`Disks::attach`] mounts that partition.
    #[cfg(target_os = "linux")]
    pub fn partitioned(&self, volume: &Path) -> PathBuf {
        let stick = volume
            .extension()
            .is_some_and(|kind| kind == "fat" || kind == "exfat");
        assert!(stick, "{} is not a FAT or exFAT image", volume.display());
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
        let [mut from, mut to] = [OsString::from("if="), OsString::from("of=")];
        from.push(volume);
        to.push(&image);
        let copy = [&from, &to].map(OsString::as_os_str);
        let options = ["bs=1M", "seek=1", "conv=notrunc", "status=none"].map(OsStr::new);
        run("dd", &[&copy[..], &options[..]].concat());
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
                let detach = Detach(device.clone());
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
                drop(detach);
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
            .rposition(|mounted| mounted.at == at)
            .unwrap_or_else(|| panic!("no image owned at {}", at.display()));
        match unmount(&self.mounted[index]) {
            Ok(Outcome::Ejected { .. }) => {}
            #[cfg(target_os = "macos")]
            Ok(Outcome::KeptBusy { device }) => {
                panic!("{} stays attached as {device}", at.display())
            }
            Err(failure) => panic!("{failure}"),
        }
        self.mounted.remove(index);
    }

    /// Unmounts the folder `at` and keeps its image attached, so no image attached later takes its device number.
    #[cfg(target_os = "macos")]
    pub fn unmount(&mut self, at: &Path) {
        let mounted = self
            .mounted
            .iter_mut()
            .rfind(|mounted| mounted.at == at)
            .unwrap_or_else(|| panic!("no image owned at {}", at.display()));
        run("/sbin/umount", &["-f".as_ref(), mounted.at.as_os_str()]);
        mounted.at = self.dir.path().join("unmounted");
    }

    /// Records that the image mounted at `from` is at `to` now, after a folder above it moved.
    pub fn moved(&mut self, from: &Path, to: &Path) {
        for mounted in &mut self.mounted {
            if let Ok(below) = mounted.at.strip_prefix(from) {
                mounted.at = to.join(below);
            }
        }
    }

    fn cleanup(
        &mut self,
        mut detach: impl FnMut(&Mounted) -> Result<Outcome, String>,
    ) -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        let mut failures = Vec::new();
        for mounted in self.mounted.drain(..).rev() {
            match detach(&mounted) {
                Ok(outcome) => {
                    if !matches!(outcome, Outcome::Ejected { .. }) {
                        failures.push(format!("{outcome:?}"));
                    }
                    outcomes.push(outcome);
                }
                Err(failure) => failures.push(failure),
            }
        }
        if !failures.is_empty() {
            self.dir.disable_cleanup(true);
            let _ = writeln!(
                std::io::stderr(),
                "Disk cleanup failed; keeping {} for the OS to reclaim: {failures:?}",
                self.dir.path().display()
            );
        }
        outcomes
    }

    fn close(&mut self) -> Vec<Outcome> {
        if std::mem::replace(&mut self.closed, true) {
            return Vec::new();
        }
        #[cfg(target_os = "macos")]
        match attached_under(self.dir.path()) {
            Ok(attached) => {
                for image in attached {
                    if !self
                        .mounted
                        .iter()
                        .any(|known| known.device == image.device)
                    {
                        let _ = writeln!(
                            std::io::stderr(),
                            "Disk cleanup found {} attached as {} without a record",
                            image.image.display(),
                            image.device
                        );
                        self.mounted.push(image);
                    }
                }
            }
            Err(failure) => {
                self.dir.disable_cleanup(true);
                let _ = writeln!(
                    std::io::stderr(),
                    "Disk cleanup could not list attached images; keeping {} for the OS to reclaim: {failure}",
                    self.dir.path().display()
                );
            }
        }
        self.cleanup(unmount)
    }
}

fn created(
    image: &Path,
    mut create: impl FnMut() -> Result<String, String>,
) -> Result<String, String> {
    until_ready(std::time::Duration::from_secs(120), || {
        let created = create();
        if created
            .as_ref()
            .is_err_and(|failure| failure.contains("Resource busy"))
        {
            eject_strays(image)?;
        }
        created
    })
}

#[cfg(target_os = "macos")]
fn eject_strays(image: &Path) -> Result<(), String> {
    for attached in registry::attached()? {
        let Some(path) = attached
            .image
            .filter(|path| resolved(path) == resolved(image))
        else {
            continue;
        };
        let device = attached.device.ok_or_else(|| {
            format!(
                "{} is attached without a device it can read",
                path.display()
            )
        })?;
        let stray = Mounted {
            at: path.clone(),
            image: path,
            device,
        };
        let outcome = unmount(&stray)?;
        let _ = writeln!(
            std::io::stderr(),
            "A refused create left {} attached as {}: {outcome:?}",
            image.display(),
            stray.device
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn eject_strays(_: &Path) -> Result<(), String> {
    Ok(())
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

#[cfg(target_os = "linux")]
struct Detach(String);

#[cfg(target_os = "linux")]
impl Drop for Detach {
    fn drop(&mut self) {
        let detached = Command::new("sudo")
            .args(["-n", "losetup", "-d", &self.0])
            .status();
        let detached = detached.is_ok_and(|status| status.success());
        assert!(
            detached || std::thread::panicking(),
            "{} stayed attached",
            self.0
        );
    }
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
mod registry {
    use std::ffi::{CStr, OsStr, c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(class: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingServices(
            main_port: u32,
            matching: *mut c_void,
            iterator: *mut u32,
        ) -> i32;
        fn IOIteratorNext(iterator: u32) -> u32;
        fn IORegistryEntrySearchCFProperty(
            entry: u32,
            plane: *const c_char,
            key: *const c_void,
            allocator: *const c_void,
            options: u32,
        ) -> *const c_void;
        fn IOObjectRelease(object: u32) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            text: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFStringGetCString(
            text: *const c_void,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFGetTypeID(object: *const c_void) -> usize;
        fn CFDataGetTypeID() -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFStringGetLength(text: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFDataGetLength(data: *const c_void) -> isize;
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFRelease(object: *const c_void);
    }

    const UTF8: u32 = 0x0800_0100;
    const CHILDREN: u32 = 0b01;

    unsafe fn search(entry: u32, name: &CStr, options: u32) -> Result<Option<Vec<u8>>, String> {
        unsafe {
            let key = CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), UTF8);
            if key.is_null() {
                return Err(format!("{name:?} cannot be made into a registry key"));
            }
            let found = IORegistryEntrySearchCFProperty(
                entry,
                c"IOService".as_ptr(),
                key,
                std::ptr::null(),
                options,
            );
            CFRelease(key);
            if found.is_null() {
                return Ok(None);
            }
            let value = if CFGetTypeID(found) == CFDataGetTypeID() {
                let bytes = match CFDataGetLength(found) {
                    0 => &[][..],
                    length => std::slice::from_raw_parts(CFDataGetBytePtr(found), length as usize),
                };
                Ok(Some(bytes.strip_suffix(&[0]).unwrap_or(bytes).to_vec()))
            } else if CFGetTypeID(found) == CFStringGetTypeID() {
                let mut buffer = vec![
                    0 as c_char;
                    CFStringGetMaximumSizeForEncoding(CFStringGetLength(found), UTF8)
                        as usize
                        + 1
                ];
                (CFStringGetCString(found, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) != 0)
                    .then(|| Some(CStr::from_ptr(buffer.as_ptr()).to_bytes().to_vec()))
                    .ok_or_else(|| format!("{name:?} does not fit in {} bytes", buffer.len()))
            } else {
                Err(format!("{name:?} is neither data nor a string"))
            };
            CFRelease(found);
            value
        }
    }

    unsafe fn image(drive: u32) -> Result<Option<PathBuf>, String> {
        let path = |bytes: Vec<u8>| PathBuf::from(OsStr::from_bytes(&bytes));
        if let Some(image) = unsafe { search(drive, c"image-path", 0)? } {
            return Ok(Some(path(image)));
        }
        let url = unsafe { search(drive, c"DiskImageURL", 0)? };
        let Some(url) = url else {
            return Err("a disk image drive has no image path it can read".to_owned());
        };
        let Some(encoded) = url.strip_prefix(b"file://") else {
            return Ok(None);
        };
        let mut encoded = encoded.iter().copied();
        let mut decoded = Vec::new();
        while let Some(byte) = encoded.next() {
            decoded.push(match byte {
                b'%' => encoded
                    .next()
                    .zip(encoded.next())
                    .and_then(|(high, low)| {
                        u8::from_str_radix(std::str::from_utf8(&[high, low]).ok()?, 16).ok()
                    })
                    .ok_or_else(|| {
                        format!("{} has a broken escape", String::from_utf8_lossy(&url))
                    })?,
                byte => byte,
            });
        }
        Ok(Some(path(decoded)))
    }

    pub struct Attached {
        pub image: Option<PathBuf>,
        pub device: Option<String>,
    }

    pub fn attached() -> Result<Vec<Attached>, String> {
        let mut images = Vec::new();
        for class in [c"IOHDIXHDDrive", c"AppleDiskImageDevice"] {
            let mut drives = 0;
            let status = unsafe {
                IOServiceGetMatchingServices(0, IOServiceMatching(class.as_ptr()), &mut drives)
            };
            if status != 0 {
                return Err(format!("listing {class:?} failed with {status:#x}"));
            }
            let found = std::iter::from_fn(|| {
                let drive = unsafe { IOIteratorNext(drives) };
                (drive != 0).then(|| {
                    let found = unsafe { (image(drive), search(drive, c"BSD Name", CHILDREN)) };
                    unsafe { IOObjectRelease(drive) };
                    Ok((found.0?, found.1?))
                })
            })
            .collect::<Result<Vec<_>, String>>();
            unsafe { IOObjectRelease(drives) };
            images.extend(found?.into_iter().map(|(image, name)| Attached {
                image,
                device: name.map(|name| format!("/dev/{}", String::from_utf8_lossy(&name))),
            }));
        }
        Ok(images)
    }
}

#[cfg(target_os = "macos")]
fn resolved(image: &Path) -> Option<PathBuf> {
    fs::canonicalize(image).ok().or_else(|| {
        Some(
            fs::canonicalize(image.parent()?)
                .ok()?
                .join(image.file_name()?),
        )
    })
}

#[cfg(target_os = "macos")]
fn attached_under(dir: &Path) -> Result<Vec<Mounted>, String> {
    let base = match fs::canonicalize(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        base => base.map_err(|error| error.to_string())?,
    };
    registry::attached()?
        .into_iter()
        .filter_map(|attached| Some((attached.image?, attached.device)))
        .filter(|(image, _)| resolved(image).is_some_and(|image| image.starts_with(&base)))
        .map(|(image, device)| {
            Ok(Mounted {
                at: base.clone(),
                device: device.ok_or_else(|| {
                    format!(
                        "{} is attached without a device it can read",
                        image.display()
                    )
                })?,
                image,
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn still_attached(mounted: &Mounted) -> Result<bool, String> {
    listed(Some(&mounted.image), &mounted.device)
}

#[cfg(target_os = "macos")]
fn listed(image: Option<&Path>, device: &str) -> Result<bool, String> {
    let mut listed = false;
    for attached in registry::attached()? {
        if attached.image.as_deref() != image {
            continue;
        }
        let on = attached
            .device
            .ok_or_else(|| format!("{image:?} is attached without a device it can read"))?;
        listed |= on == device;
    }
    Ok(listed)
}

#[cfg(target_os = "macos")]
fn unmount(mounted: &Mounted) -> Result<Outcome, String> {
    let started = std::time::Instant::now();
    let mut busy = None;
    loop {
        if !still_attached(mounted)? {
            break;
        }
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
            let _ = writeln!(
                std::io::stderr(),
                "{} stays attached as {}: {failure}",
                mounted.image.display(),
                mounted.device
            );
            return Ok(Outcome::KeptBusy {
                device: mounted.device.clone(),
            });
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
    Ok(Outcome::Ejected {
        device: mounted.device.clone(),
    })
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
fn unmount(mounted: &Mounted) -> Result<Outcome, String> {
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
            return Ok(Outcome::Ejected {});
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
        self.close();
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
        Disks {
            dir,
            mounted,
            closed: false,
        }
    }

    #[cfg(target_os = "macos")]
    fn close_ejects_or_keeps_busy(mut disks: Disks, image: &Path, device: &str) {
        let (base, outcomes) = (disks.base(), disks.close());
        let listed = listed(Some(image), device);
        drop(disks);
        let ejected = Outcome::Ejected {
            device: device.to_owned(),
        };
        let kept = Outcome::KeptBusy {
            device: device.to_owned(),
        };
        let all_ejected = outcomes
            .iter()
            .all(|outcome| matches!(outcome, Outcome::Ejected { .. }));
        assert!(
            (outcomes.contains(&ejected) && listed == Ok(false) && base.exists() != all_ejected)
                || (outcomes.contains(&kept) && listed == Ok(true) && base.exists()),
            "{device} {outcomes:?} {listed:?}"
        );
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
        disks.cleanup(|_| {
            Ok(Outcome::Ejected {
                #[cfg(target_os = "macos")]
                device: String::new(),
            })
        });
        drop(disks);
        assert!(!base.exists());
    }

    #[test]
    fn image_copies_publish_the_same_bytes_without_an_unfinished_stage() {
        let disks = Disks {
            dir: tempfile::tempdir().unwrap(),
            mounted: Vec::new(),
            closed: false,
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
    fn unmount_leaves_a_folder_it_does_not_own_mounted() {
        let mut theirs = Disks::new().unwrap();
        let at = theirs.base().join("theirs");
        fs::create_dir(&at).unwrap();
        let image = theirs.image("theirs");
        theirs.attach(&image, &at);
        let ours = std::panic::catch_unwind(|| Disks::new().unwrap().unmount(&at));
        assert!(ours.is_err());
        assert_eq!(is_mount_point(&at), Ok(true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_unmounted_image_leaves_the_folder_to_the_image_mounted_there_later() {
        let mut ours = Disks::new().unwrap();
        let at = ours.base().join("shared");
        fs::create_dir(&at).unwrap();
        let image = ours.image("ours");
        ours.attach(&image, &at);
        ours.unmount(&at);
        let mut theirs = Disks::new().unwrap();
        let image = theirs.image("theirs");
        theirs.attach(&image, &at);
        drop(ours);
        assert_eq!(is_mount_point(&at), Ok(true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_image_attached_without_a_record_is_ejected_on_drop() {
        let disks = Disks::new().unwrap();
        let image = disks.image("Unrecorded");
        let at = disks.base().join("at");
        fs::create_dir(&at).unwrap();
        let attached = hdiutil(&[
            "attach".as_ref(),
            "-plist".as_ref(),
            "-nobrowse".as_ref(),
            "-noautoopen".as_ref(),
            "-mountpoint".as_ref(),
            at.as_os_str(),
            image.as_os_str(),
        ]);
        let unrecorded = Mounted {
            at,
            image,
            device: whole_device(&attached).unwrap(),
        };
        close_ejects_or_keeps_busy(disks, &unrecorded.image, &unrecorded.device);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_drop_leaves_another_disks_image_attached_through_a_link_into_its_folder() {
        let mut theirs = Disks::new().unwrap();
        let ours = Disks::new().unwrap();
        theirs.image("Theirs");
        std::os::unix::fs::symlink(theirs.base(), ours.base().join("link")).unwrap();
        let at = theirs.base().join("at");
        fs::create_dir(&at).unwrap();
        theirs.attach(&ours.base().join("link/Theirs.dmg"), &at);
        let recorded = theirs.mounted[0].clone();
        drop(ours);
        assert_eq!(still_attached(&recorded), Ok(true), "{}", recorded.device);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_drop_ejects_an_image_of_its_folder_attached_through_another_spelling() {
        let disks = Disks::new().unwrap();
        let image = disks.image("Spelled");
        let links = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(disks.base(), links.path().join("link")).unwrap();
        let spelled = links.path().join("link/Spelled.dmg");
        assert_ne!(spelled, image);
        let at = disks.base().join("at");
        fs::create_dir(&at).unwrap();
        let attached = hdiutil(&[
            "attach".as_ref(),
            "-plist".as_ref(),
            "-nobrowse".as_ref(),
            "-noautoopen".as_ref(),
            "-mountpoint".as_ref(),
            at.as_os_str(),
            spelled.as_os_str(),
        ]);
        let unrecorded = Mounted {
            at,
            image: spelled,
            device: whole_device(&attached).unwrap(),
        };
        close_ejects_or_keeps_busy(disks, &unrecorded.image, &unrecorded.device);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_drop_ejects_an_image_of_its_folder_that_disk_images_2_attached() {
        let disks = Disks::new().unwrap();
        let image = disks.image("Disk Images 2%");
        let device = Eject::attach(Some(image.clone()));
        close_ejects_or_keeps_busy(disks, &image, &device.device);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_drop_ejects_an_image_of_its_folder_while_a_ram_disk_is_attached() {
        let ram = Eject::attach(None);
        let disks = Disks::new().unwrap();
        let image = disks.image("BesideRam");
        let at = disks.base().join("at");
        fs::create_dir(&at).unwrap();
        let attached = hdiutil(&[
            "attach".as_ref(),
            "-plist".as_ref(),
            "-nobrowse".as_ref(),
            "-noautoopen".as_ref(),
            "-mountpoint".as_ref(),
            at.as_os_str(),
            image.as_os_str(),
        ]);
        let unrecorded = Mounted {
            at,
            image,
            device: whole_device(&attached).unwrap(),
        };
        close_ejects_or_keeps_busy(disks, &unrecorded.image, &unrecorded.device);
        assert_eq!(listed(None, &ram.device), Ok(true), "{}", ram.device);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_drop_ejects_an_image_of_its_folder_at_the_longest_url_disk_images_2_takes() {
        use std::os::unix::ffi::OsStrExt;
        let disks = Disks::new().unwrap();
        let image = disks.image("Long");
        let url_length = |path: &Path| {
            "file://".len()
                + path
                    .as_os_str()
                    .as_bytes()
                    .iter()
                    .map(|&byte| if byte == b' ' { 3 } else { 1 })
                    .sum::<usize>()
        };
        let mut long = fs::canonicalize(disks.base()).unwrap();
        for _ in 0..3 {
            long.push(" ".repeat(100));
        }
        fs::create_dir_all(&long).unwrap();
        let name = "x".repeat(1023 - url_length(&long) - "/.dmg".len());
        long.push(format!("{name}.dmg"));
        assert_eq!(url_length(&long), 1023);
        fs::rename(&image, &long).unwrap();
        let device = Eject::attach(Some(long.clone()));
        close_ejects_or_keeps_busy(disks, &long, &device.device);
    }

    #[cfg(target_os = "macos")]
    struct Eject {
        device: String,
        image: Option<PathBuf>,
    }

    #[cfg(target_os = "macos")]
    impl Eject {
        fn attach(image: Option<PathBuf>) -> Eject {
            let source = image
                .as_deref()
                .map_or("ram://32768".as_ref(), Path::as_os_str);
            Eject {
                device: disk_images_2_attach(source),
                image,
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for Eject {
        fn drop(&mut self) {
            if listed(self.image.as_deref(), &self.device) == Ok(true) {
                let _ = output("diskutil", &["eject".as_ref(), self.device.as_ref()]);
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_refused_create_ejects_the_drive_it_left_on_its_path() {
        let disks = Disks::new().unwrap();
        let image = disks.image("Stray");
        let stray = Eject::attach(Some(image.clone()));
        let mut answers = [
            Err("hdiutil: create failed - Resource busy".to_owned()),
            Ok(String::new()),
        ]
        .into_iter();
        assert_eq!(
            created(&image, || answers.next().unwrap()),
            Ok(String::new())
        );
        assert_eq!(
            listed(stray.image.as_deref(), &stray.device),
            Ok(false),
            "{}",
            stray.device
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_eject_guard_ejects_only_its_own_image() {
        let disks = Disks::new().unwrap();
        let image = disks.image("Held");
        let held = Eject::attach(Some(image.clone()));
        let device = held.device.clone();
        drop(Eject {
            device: device.clone(),
            image: Some(disks.image("Other")),
        });
        let after_another_guard = listed(Some(&image), &device);
        drop(held);
        assert_eq!(after_another_guard, Ok(true));
        assert_eq!(listed(Some(&image), &device), Ok(false));
    }

    #[cfg(target_os = "macos")]
    fn disk_images_2_attach(source: &OsStr) -> String {
        let attached = run(
            "diskutil",
            &[
                "image".as_ref(),
                "attach".as_ref(),
                "--noMount".as_ref(),
                "--plist".as_ref(),
                source,
            ],
        );
        plist::Value::from_reader_xml(attached.as_bytes())
            .unwrap()
            .as_dictionary()
            .and_then(|attached| attached.get("system-entities"))
            .and_then(plist::Value::as_array)
            .and_then(|entities| entities.first())
            .and_then(plist::Value::as_dictionary)
            .and_then(|entity| entity.get("dev-entry"))
            .and_then(plist::Value::as_string)
            .map(|name| format!("/dev/{name}"))
            .unwrap()
    }

    #[test]
    fn a_drop_after_its_folder_moved_keeps_it_without_a_panic() {
        let disks = Disks {
            dir: tempfile::tempdir().unwrap(),
            mounted: Vec::new(),
            closed: false,
        };
        let moved = disks.dir.path().with_extension("moved");
        fs::rename(disks.dir.path(), &moved).unwrap();
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(disks)));
        fs::remove_dir_all(&moved).unwrap();
        assert!(dropped.is_ok());
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
    fn unmount_leaves_a_device_that_now_holds_another_image() {
        let mut disks = Disks::new().unwrap();
        let image = disks.image("Now");
        let at = disks.base().join("now");
        fs::create_dir(&at).unwrap();
        disks.attach(&image, &at);
        let now = disks.mounted[0].clone();
        let earlier = Mounted {
            at: disks.base().join("earlier"),
            image: disks.base().join("Earlier.dmg"),
            device: now.device.clone(),
        };
        assert_eq!(
            unmount(&earlier),
            Ok(Outcome::Ejected {
                device: earlier.device.clone()
            })
        );
        assert_eq!(still_attached(&now), Ok(true), "{}", now.device);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_record_of_the_same_image_on_another_device_is_not_attached() {
        let mut disks = Disks::new().unwrap();
        let image = disks.image("Again");
        let at = disks.base().join("again");
        fs::create_dir(&at).unwrap();
        disks.attach(&image, &at);
        let again = disks.mounted[0].clone();
        let earlier = Mounted {
            at: disks.base().join("earlier"),
            image,
            device: "/dev/disk0".to_owned(),
        };
        assert_eq!(still_attached(&earlier), Ok(false));
        assert_eq!(
            unmount(&earlier),
            Ok(Outcome::Ejected {
                device: earlier.device.clone()
            })
        );
        assert_eq!(still_attached(&again), Ok(true), "{}", again.device);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_listing_the_registry_will_not_describe_is_an_error() {
        if std::env::var_os("KOIDRIVE_TEST_IOKIT_DENIED").is_some() {
            assert!(registry::attached().is_err());
            return;
        }
        let mut disks = Disks::new().unwrap();
        let image = disks.image("Described");
        let at = disks.base().join("at");
        fs::create_dir(&at).unwrap();
        disks.attach(&image, &at);
        let denied = std::process::Command::new("sandbox-exec")
            .args([
                "-p",
                "(version 1)(allow default)(deny iokit-get-properties)",
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "disks::tests::a_listing_the_registry_will_not_describe_is_an_error",
                "--nocapture",
            ])
            .env("KOIDRIVE_TEST_IOKIT_DENIED", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&denied.stdout);
        assert!(
            denied.status.success() && stdout.contains("test result: ok. 1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&denied.stderr)
        );
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
        let ejected = disks.mounted[0].clone();
        disks.detach(&at);
        assert_eq!(is_mount_point(&at), Ok(false));
        #[cfg(target_os = "macos")]
        assert_eq!(still_attached(&ejected), Ok(false), "{}", ejected.device);
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
        let ejected = disks.mounted[0].clone();
        let held = fs::File::open(&ejected.device).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(held);
        });
        disks.detach(&at);
        release.join().unwrap();
        assert_eq!(still_attached(&ejected), Ok(false), "{}", ejected.device);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn images_detach_after_releasing_a_file_and_moving_the_parent() {
        for format in ["APFS", "MS-DOS", "ExFAT"] {
            let mut disks = Disks::new().unwrap();
            let image = disks.formatted("Held", format, 16, None);
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
            let first = disks.mounted[0].clone();
            disks.detach(&moved.join("mount"));
            assert_eq!(still_attached(&first), Ok(false), "{format}");
            disks.attach(&image, &moved.join("mount"));
            assert_eq!(fs::read(moved.join("mount/held.txt")).unwrap(), b"saved");
            let second = disks.mounted[0].clone();
            close_ejects_or_keeps_busy(disks, &second.image, &second.device);
        }
    }
}
