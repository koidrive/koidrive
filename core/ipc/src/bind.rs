// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;

use rustix::io::Errno;
use tokio::net::{UnixListener, UnixStream};

/// Binds the daemon's socket, unless another process still answers on it. The caller holds the profile lock, so a
/// socket file that doesn't answer, or that this process created, is stale. A child spawned while this process listened
/// can still hold the old listener, but it reports this process as its creator.
///
/// The directory must belong to this user and be closed to everyone else, so no other user can reach the socket even
/// before its mode is set. The socket is bound under a temporary name and renamed once it is 0600, so it never
/// appears at `path` with another mode. Setting the umask instead would change it for every thread of the process.
/// The temporary name is `.tmp` and the process id, at most 11 bytes like `daemon.sock`, so it fits the socket address
/// limit wherever the socket path does. A socket path too long for that limit is refused, since no client could
/// connect to it.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    std::os::unix::net::SocketAddr::from_pathname(path)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let metadata = fs::metadata(dir)?;
    let refuse = |reason: String| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {reason}", dir.display()),
        )
    };
    let uid = rustix::process::getuid().as_raw();
    if metadata.uid() != uid {
        return Err(refuse("belongs to another user".to_owned()));
    }
    let mode = metadata.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(refuse(format!(
            "is open to other users (mode {mode:o}); make it 0700"
        )));
    }
    #[cfg(target_os = "macos")]
    if acl::allows_others(dir, uid)? {
        return Err(refuse(
            "lets other users in through an ACL; remove it with chmod -N".to_owned(),
        ));
    }
    if answers_for_another_process(path)? {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another process answers on {}", path.display()),
        ));
    }
    let temp = dir.join(format!(".tmp{}", std::process::id()));
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // A socket left by an earlier version can carry an inherited deny-delete entry, which blocks the rename.
    #[cfg(target_os = "macos")]
    let _ = acl::clear(path);
    let listener = bind_socket(&temp)
        .and_then(|listener| fs::rename(&temp, path).map(|()| listener))
        .inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })?;
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

fn answers_for_another_process(path: &Path) -> io::Result<bool> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
    let probe = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
    rustix::io::ioctl_fionbio(&probe, true)?;
    match rustix::net::connect(&probe, &SocketAddrUnix::new(path)?) {
        Ok(()) => {}
        Err(Errno::AGAIN | Errno::INPROGRESS) => return Ok(true),
        Err(_) => return Ok(false),
    }
    let Ok(peer) = UnixStream::from_std(std::os::unix::net::UnixStream::from(probe))
        .and_then(|stream| stream.peer_cred())
    else {
        return Ok(false);
    };
    Ok(peer.pid() != i32::try_from(std::process::id()).ok())
}

/// Creates the socket 0600 before binding it, so it is closed to other users from its first moment even if the
/// directory was swapped after it was checked. Linux takes the new file's mode from the socket, so no chmod by path is
/// needed. One could follow a symlink planted in a swapped directory.
#[cfg(target_os = "linux")]
fn bind_socket(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::fs::fchmod(&socket, rustix::fs::Mode::from_raw_mode(0o600))?;
    rustix::net::bind(&socket, &SocketAddrUnix::new(path)?)?;
    rustix::net::listen(&socket, 1024)?;
    Ok(socket.into())
}

/// macOS takes the new socket file's mode from the umask, so the file is set to 0600 right after binding, without
/// following a symlink, and any ACL it inherited from the directory is cleared, since an inherited deny-delete entry
/// would block replacing and removing it. Until then the directory check keeps other users out.
#[cfg(target_os = "macos")]
fn bind_socket(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    use rustix::fs::{AtFlags, CWD, Mode};
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    rustix::fs::chmodat(
        CWD,
        path,
        Mode::from_raw_mode(0o600),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    acl::clear(path)?;
    Ok(listener)
}

#[cfg(target_os = "macos")]
mod acl {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_int = 1;
    const ID_TYPE_UID: c_int = 0;
    unsafe extern "C" {
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_get_file(path: *const c_char, kind: c_int) -> *mut c_void;
        fn acl_set_link_np(path: *const c_char, kind: c_int, acl: *mut c_void) -> c_int;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
        fn acl_free(object: *mut c_void) -> c_int;
        fn mbr_uuid_to_id(uuid: *const u8, id: *mut u32, id_type: *mut c_int) -> c_int;
    }

    /// Removes every extended ACL entry from `path`, without following a symlink.
    pub fn clear(path: &Path) -> io::Result<()> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `path` is a valid C string, and the empty ACL from acl_init is freed once after use.
        unsafe {
            let empty = acl_init(0);
            if empty.is_null() {
                return Err(io::Error::last_os_error());
            }
            let result = acl_set_link_np(path.as_ptr(), ACL_TYPE_EXTENDED, empty);
            let error = io::Error::last_os_error();
            acl_free(empty);
            if result != 0 {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Whether the extended ACL of `dir` has an allow entry for anyone but `owner`. Such an entry grants access the
    /// mode bits do not show. Deny entries only take access away.
    pub fn allows_others(dir: &Path, owner: u32) -> io::Result<bool> {
        let path = CString::new(dir.as_os_str().as_bytes())?;
        // SAFETY: `path` is a valid C string for the duration of the call.
        let acl = unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::NotFound => Ok(false),
                _ => Err(error),
            };
        }
        let mut allows_others = false;
        let mut entry = std::ptr::null_mut();
        let mut which = ACL_FIRST_ENTRY;
        // SAFETY: `acl` is a live ACL from acl_get_file, `entry` points into it, and each qualifier is freed once.
        unsafe {
            while acl_get_entry(acl, which, &mut entry) == 0 {
                which = ACL_NEXT_ENTRY;
                let mut tag = 0;
                if acl_get_tag_type(entry, &mut tag) == 0 && tag != ACL_EXTENDED_ALLOW {
                    continue;
                }
                let qualifier = acl_get_qualifier(entry);
                let (mut id, mut id_type) = (0, -1);
                let owner_only = !qualifier.is_null()
                    && mbr_uuid_to_id(qualifier.cast(), &mut id, &mut id_type) == 0
                    && id_type == ID_TYPE_UID
                    && id == owner;
                if !qualifier.is_null() {
                    acl_free(qualifier);
                }
                if !owner_only {
                    allows_others = true;
                    break;
                }
            }
            acl_free(acl);
        }
        Ok(allows_others)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn the_socket_appears_with_mode_0600_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("run/daemon.sock");
        let stop = Arc::new(AtomicBool::new(false));
        let sightings = Arc::new(AtomicUsize::new(0));
        let watcher = std::thread::spawn({
            let run = dir.path().join("run");
            let (stop, sightings) = (stop.clone(), sightings.clone());
            move || {
                let (mut at_socket, mut anywhere) = (BTreeSet::new(), BTreeSet::new());
                while !stop.load(Ordering::Relaxed) {
                    for entry in fs::read_dir(&run).into_iter().flatten().flatten() {
                        if let Ok(metadata) = entry.metadata() {
                            let mode = metadata.permissions().mode() & 0o777;
                            if entry.file_name() == "daemon.sock" {
                                at_socket.insert(mode);
                                sightings.fetch_add(1, Ordering::Relaxed);
                            }
                            anywhere.insert(mode);
                        }
                    }
                }
                (at_socket, anywhere)
            }
        });
        let started = Instant::now();
        let mut binds = 0;
        while binds < 200 || sightings.load(Ordering::Relaxed) < 20 {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the watcher saw the socket {} times in {binds} binds",
                sightings.load(Ordering::Relaxed)
            );
            drop(bind(&socket).unwrap());
            binds += 1;
        }
        stop.store(true, Ordering::Relaxed);
        let (at_socket, anywhere) = watcher.join().unwrap();
        assert_eq!(at_socket, BTreeSet::from([0o600]));
        if cfg!(target_os = "linux") {
            assert_eq!(anywhere, BTreeSet::from([0o600]));
        }
        let names: Vec<_> = fs::read_dir(dir.path().join("run"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["daemon.sock"]);
        let mode = fs::metadata(dir.path().join("run"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[tokio::test]
    async fn a_directory_other_users_can_enter_gets_no_socket() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        let error = bind(&run.join("daemon.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("open to other users"), "{error}");
        assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_socket_another_process_answers_on_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        let socket = run.join("daemon.sock");
        let mut other = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bind::tests::listen_until_stdin_closes",
                "--ignored",
                "--nocapture",
            ])
            .env("KOI_TEST_LISTEN", &socket)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines =
            std::io::BufRead::lines(std::io::BufReader::new(other.stdout.take().unwrap()));
        assert!(lines.any(|line| line.unwrap() == "listening"));
        let error = bind(&socket).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        tokio::net::UnixStream::connect(&socket).await.unwrap();
        drop(other.stdin.take());
        other.wait().unwrap();
    }

    #[test]
    #[ignore = "the other process of a_socket_another_process_answers_on_is_left_alone"]
    fn listen_until_stdin_closes() {
        let Some(socket) = std::env::var_os("KOI_TEST_LISTEN") else {
            return;
        };
        let _listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        println!("listening");
        let _ = io::Read::read_to_end(&mut io::stdin(), &mut Vec::new());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_socket_whose_backlog_is_full_is_left_alone() {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        let socket = run.join("daemon.sock");
        let address = SocketAddrUnix::new(&socket).unwrap();
        let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        rustix::net::bind(&listener, &address).unwrap();
        rustix::net::listen(&listener, 0).unwrap();
        let mut waiting = Vec::new();
        loop {
            let client =
                rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
            rustix::io::ioctl_fionbio(&client, true).unwrap();
            match rustix::net::connect(&client, &address) {
                Ok(()) => waiting.push(client),
                Err(Errno::AGAIN) => break,
                Err(error) => panic!("{error}"),
            }
        }
        let error = bind(&socket).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn a_socket_this_process_created_is_taken_while_a_copy_of_it_listens() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        let socket = run.join("daemon.sock");
        let first = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let _copy = first.try_clone().unwrap();
        drop(first);
        let listener = bind(&socket).unwrap();
        let _client = UnixStream::connect(&socket).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_socket_path_too_long_to_connect_to_gets_no_socket() {
        let dir = tempfile::tempdir().unwrap();
        let limit = if cfg!(target_os = "macos") { 104 } else { 108 };
        let fixed = dir.path().as_os_str().len() + "/".len() + "/daemon.sock".len();
        let run = dir.path().join("r".repeat(limit - fixed));
        let socket = run.join("daemon.sock");
        assert_eq!(socket.as_os_str().len(), limit);
        let error = bind(&socket).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!run.exists());
    }

    #[tokio::test]
    async fn a_directory_of_another_user_gets_no_socket() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let error = bind(Path::new("/daemon.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("belongs to another user"),
            "{error}"
        );
    }

    #[cfg(target_os = "macos")]
    fn add_acl_entries(dir: &Path, entries: &[&str]) {
        for entry in entries {
            let status = std::process::Command::new("/bin/chmod")
                .arg("+a")
                .arg(entry)
                .arg(dir)
                .status()
                .unwrap();
            assert!(status.success(), "chmod +a {entry:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_directory_whose_acl_lets_everyone_in_gets_no_socket() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        add_acl_entries(
            &run,
            &[
                "everyone allow list,search",
                "everyone allow read,write,append,file_inherit,only_inherit",
            ],
        );
        let error = bind(&run.join("daemon.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("through an ACL"), "{error}");
        assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_directory_whose_acl_only_denies_or_allows_the_owner_gets_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        let user = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout;
        let user = String::from_utf8(user).unwrap();
        add_acl_entries(
            &run,
            &[
                "everyone deny delete",
                &format!("user:{} allow list,search", user.trim()),
            ],
        );
        let socket = run.join("daemon.sock");
        let listener = bind(&socket).unwrap();
        let mode = fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _client = UnixStream::connect(&socket).await.unwrap();
        listener.accept().await.unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_directory_whose_acl_denies_deleting_new_files_still_gets_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        add_acl_entries(&run, &["everyone deny delete,file_inherit"]);
        let socket = run.join("daemon.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        drop(bind(&socket).unwrap());
        let listener = bind(&socket).unwrap();
        let names: Vec<_> = fs::read_dir(&run)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["daemon.sock"]);
        let _client = UnixStream::connect(&socket).await.unwrap();
        listener.accept().await.unwrap();
    }
}
