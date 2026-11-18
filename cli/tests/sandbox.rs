// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use koidrive_test_support::Sandbox;
use syn::visit::Visit;

#[path = "support/process.rs"]
mod process;

const CHILD: &str = "KOI_SANDBOX_TEST_CHILD";

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const ENDED: Duration = Duration::from_secs(30);

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + ENDED;
    while !done() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn first(
    child: &mut Reaped,
    mut wanted: impl FnMut(String) -> Option<String> + Send + 'static,
) -> String {
    let stdout = child.0.stdout.take().unwrap();
    let (found, line) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = found.send(
            BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .find_map(&mut wanted),
        );
    });
    (line.recv_timeout(ENDED).ok().flatten())
        .unwrap_or_else(|| panic!("no startup line within {ENDED:?}"))
}

fn daemon(sandbox: &Sandbox) -> (Reaped, PathBuf) {
    let home = sandbox.path();
    let mut daemon = Reaped(
        process::koi(home)
            .args(["_daemon", "--json"])
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_RUNTIME_DIR", home.join("run"))
            .env("KOIDRIVE_TELEMETRY", "0")
            .env("KOIDRIVE_UPDATE_CHECK", "0")
            .env_remove("KOIDRIVE_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started: serde_json::Value = serde_json::from_str(&first(&mut daemon, Some)).unwrap();
    let socket = PathBuf::from(started["socket"].as_str().unwrap());
    (daemon, socket)
}

#[test]
fn a_koi_stops_when_its_sandbox_drops_after_its_test_passes_or_panics() {
    for panics in [false, true] {
        let sandbox = Sandbox::new();
        let root = sandbox.path().to_path_buf();
        let (mut daemon, _) = daemon(&sandbox);
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _sandbox = sandbox;
            assert!(!panics, "the test failed");
        }));
        assert_eq!(ended.is_err(), panics);
        let mut status = None;
        until("the daemon outlived its sandbox", || {
            status = daemon.0.try_wait().unwrap();
            status.is_some()
        });
        assert_eq!(
            status.and_then(|status| status.signal()),
            Some(9),
            "{status:?}"
        );
        assert!(!root.exists());
    }
}

struct Left(rustix::process::Pid);

impl Drop for Left {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.0, rustix::process::Signal::KILL);
    }
}

#[test]
fn a_koi_stops_when_the_test_process_that_started_it_is_killed() {
    if std::env::var_os(CHILD).is_some() {
        let sandbox = Sandbox::new();
        let (daemon, socket) = daemon(&sandbox);
        println!("{CHILD} {} {}", daemon.0.id(), socket.display());
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }
    let mut test = Reaped(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_koi_stops_when_the_test_process_that_started_it_is_killed",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let line = first(&mut test, |line| {
        line.strip_prefix(&format!("{CHILD} ")).map(str::to_owned)
    });
    let mut words = line.split(' ');
    let pid: i32 = words.next().unwrap().parse().unwrap();
    let _left = Left(rustix::process::Pid::from_raw(pid).unwrap());
    let socket = PathBuf::from(words.next().unwrap());
    let answered = UnixStream::connect(&socket).is_ok();

    test.0.kill().unwrap();
    test.0.wait().unwrap();

    assert!(answered, "the daemon was not running");
    until(
        "the daemon still answers after the test process that started it was killed",
        || UnixStream::connect(&socket).is_err(),
    );
}

#[test]
fn a_test_cannot_run_koi_outside_its_sandbox() {
    let account = std::env::home_dir().unwrap();
    let sandbox = Sandbox::new();
    let set: Vec<_> = process::koi(sandbox.path())
        .get_envs()
        .map(|(name, value)| (name.to_owned(), value.is_some()))
        .collect();
    for name in [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "HOMEBREW_PREFIX",
    ] {
        assert!(
            set.contains(&(name.into(), false)),
            "{name} reaches a sandboxed koi"
        );
    }
    let decoy = tempfile::tempdir().unwrap();
    let app = decoy.path().join("Applications/Koidrive.app");
    for dir in ["Contents/Helpers", "Contents/MacOS"] {
        std::fs::create_dir_all(app.join(dir)).unwrap();
    }
    std::fs::copy(process::KOI, app.join("Contents/Helpers/koi")).unwrap();
    std::fs::write(app.join("Contents/MacOS/Koidrive"), "x").unwrap();
    let linked = |name: &str, to: &Path| {
        let link = sandbox.path().join(name);
        std::os::unix::fs::symlink(to, &link).unwrap();
        link
    };
    let config = decoy.path().join("config/koidrive");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(sandbox.path().join("config.toml"), "").unwrap();
    std::os::unix::fs::symlink(
        sandbox.path().join("config.toml"),
        config.join("config.toml"),
    )
    .unwrap();
    let real = std::fs::canonicalize(sandbox.path()).unwrap();
    let up = PathBuf::from_iter(real.components().skip(1).map(|_| ".."));
    let run = |args: &[&str], name: &str, value: &Path| {
        process::koi(sandbox.path())
            .args(args)
            .env("HOME", sandbox.path())
            .env(name, value)
            .output()
            .unwrap()
    };
    let refused = |output: Output, case: String| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(78), "{case}: {stderr}");
        assert!(
            stderr.starts_with("koi: a test must not"),
            "{case}: {stderr}"
        );
    };
    for (name, value) in [
        ("HOME", account.clone()),
        ("HOME", linked("home", &account)),
        (
            "HOME",
            real.join(&up).join(account.strip_prefix("/").unwrap()),
        ),
        ("HOME", decoy.path().to_path_buf()),
        ("XDG_STATE_HOME", account.join(".local/state")),
        ("XDG_CONFIG_HOME", account.join(".config")),
        ("XDG_CONFIG_HOME", decoy.path().join("config")),
        ("XDG_RUNTIME_DIR", decoy.path().join("run")),
        ("KOIDRIVE_TEST_INSTALL_INVENTORY", PathBuf::from("/")),
        ("HOMEBREW_PREFIX", PathBuf::from("/opt/homebrew")),
        ("HOMEBREW_PREFIX", linked("brew", decoy.path())),
    ] {
        let case = format!("{name}={}", value.display());
        refused(run(&["help", "--json"], name, &value), case);
    }
    let escaped = sandbox
        .path()
        .join("..")
        .join(decoy.path().file_name().unwrap());
    let uninstall = ["uninstall", "--yes", "--last-device"];
    let inventory = "KOIDRIVE_TEST_INSTALL_INVENTORY";
    refused(
        run(&uninstall, inventory, &escaped),
        format!("{inventory}={}", escaped.display()),
    );
    assert!(app.exists());
    linked("Applications", &decoy.path().join("Applications"));
    let output = run(&uninstall, inventory, sandbox.path());
    assert!(app.exists(), "{}", String::from_utf8_lossy(&output.stdout));
    let state = sandbox.path().join("state");
    assert!(
        run(&["help", "--json"], "XDG_STATE_HOME", &state)
            .status
            .success()
    );
}

#[test]
fn a_koi_outside_a_sandbox_cannot_start() {
    let outside = tempfile::tempdir().unwrap();
    let started = std::panic::catch_unwind(|| process::koi(outside.path()));
    assert!(started.is_err());
}

const ALLOWED: [&str; 7] = [
    "\"kill\"",
    "\"/bin/kill\"",
    "\"/bin/cp\"",
    "\"/bin/launchctl\"",
    "\"systemctl\"",
    "\"pgrep\"",
    "std::env::current_exe()",
];

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => sources(&path, found),
            false if path.extension().is_some_and(|extension| extension == "rs") => {
                found.push(path)
            }
            false => {}
        }
    }
}

#[test]
fn every_program_a_test_runs_that_could_start_koi_is_sandboxed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    sources(&root.join("cli/tests"), &mut files);
    files.push(root.join("core/ffi/src/daemon/server.rs"));
    let mut unsandboxed = Vec::new();
    let support = root.join("cli/tests/support");
    for file in files.iter().filter(|file| !file.starts_with(&support)) {
        let text = std::fs::read_to_string(file).unwrap();
        for (number, line) in text.lines().enumerate() {
            let spawns = line
                .match_indices(concat!("Command", "::new("))
                .any(|(at, call)| {
                    let program = &line[at + call.len()..];
                    !ALLOWED.iter().any(|allowed| program.starts_with(allowed))
                });
            if spawns || line.contains(concat!("CARGO_BIN_", "EXE_koi")) {
                unsandboxed.push(format!(
                    "{}:{}: {}",
                    file.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        unsandboxed.is_empty(),
        "run these with process::koi or koidrive_test_support::sandboxed, so that a koi they start stops with the test:\n{}",
        unsandboxed.join("\n")
    );
}

#[test]
fn every_program_koi_starts_goes_through_its_program_runner() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    let tests = |file: &Path| {
        file.components().any(|part| part.as_os_str() == "tests")
            || file.file_name().is_some_and(|name| name == "tests.rs")
    };
    let mut found = Vec::new();
    for file in files.iter().map(|file| file.strip_prefix(&src).unwrap()) {
        if tests(file) {
            continue;
        }
        let tree = syn::parse_file(&std::fs::read_to_string(src.join(file)).unwrap()).unwrap();
        Runner {
            file,
            found: &mut found,
            tainted: Vec::new(),
        }
        .visit_file(&tree);
    }
    assert!(
        found.is_empty(),
        "start programs through crate::program and pass paths to them as program::Confined, so that a test koi stays inside its sandbox:\n{}",
        found.join("\n")
    );
}

struct Runner<'a> {
    file: &'a Path,
    found: &'a mut Vec<String>,
    tainted: Vec<String>,
}

impl Runner<'_> {
    fn flag(&mut self, span: proc_macro2::Span, what: String) {
        let line = span.start().line;
        self.found
            .push(format!("cli/src/{}:{line}: {what}", self.file.display()));
    }

    fn runner(&self) -> bool {
        self.file == Path::new("program.rs")
    }

    fn configures(&self) -> bool {
        self.runner() || self.file == Path::new("app_install/macos/control.rs")
    }

    fn stringified(&self, expr: &syn::Expr) -> Option<proc_macro2::Span> {
        let mut text = Stringified {
            found: None,
            tainted: &self.tainted,
        };
        text.visit_expr(expr);
        text.found
    }

    fn uses(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        let module = prefix.last().is_some_and(|last| last == "process");
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.uses(&path.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(syn::UseName { ident })
            | syn::UseTree::Rename(syn::UseRename { ident, .. })
                if module && ident == "Command" && !self.runner() =>
            {
                self.flag(ident.span(), format!("{}::Command", prefix.join("::")));
            }
            syn::UseTree::Name(syn::UseName { ident })
            | syn::UseTree::Rename(syn::UseRename { ident, .. })
                if ident == "configured_async" && !self.configures() =>
            {
                self.flag(
                    ident.span(),
                    "configured_async outside app_install's control".to_owned(),
                );
            }
            syn::UseTree::Rename(rename)
                if rename.ident == "process" || (module && rename.ident == "self") =>
            {
                self.flag(rename.ident.span(), "a renamed process module".to_owned());
            }
            syn::UseTree::Glob(glob) if module => {
                self.flag(glob.star_token.span, format!("{}::*", prefix.join("::")));
            }
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    self.uses(tree, prefix);
                }
            }
            _ => {}
        }
    }

    fn texts(&mut self, args: impl IntoIterator<Item = impl std::borrow::Borrow<syn::Expr>>) {
        for arg in args {
            if let Some(span) = self.stringified(arg.borrow()) {
                self.flag(span, "a path passed as text".to_owned());
            }
        }
    }
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    fn requires_test(meta: &syn::Meta) -> bool {
        match meta {
            syn::Meta::Path(path) => path.is_ident("test"),
            syn::Meta::List(list) if list.path.is_ident("all") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|all| all.iter().any(requires_test)),
            _ => false,
        }
    }
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|cfg| requires_test(&cfg))
    })
}

impl<'ast> Visit<'ast> for Runner<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs = match item {
            syn::Item::Fn(item) => &item.attrs,
            syn::Item::Mod(item) => &item.attrs,
            syn::Item::Use(item) => &item.attrs,
            syn::Item::Impl(item) => &item.attrs,
            syn::Item::Const(item) => &item.attrs,
            syn::Item::Static(item) => &item.attrs,
            _ => &Vec::new(),
        };
        if !test_only(attrs) {
            syn::visit::visit_item(self, item);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.uses(&item.tree, &mut Vec::new());
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let hands_out = match &item.sig.output {
            syn::ReturnType::Type(_, ty) => {
                let mut raw = Raw(None);
                raw.visit_type(ty);
                raw.0
            }
            syn::ReturnType::Default => None,
        };
        if let Some(span) = hands_out.filter(|_| !matches!(item.vis, syn::Visibility::Inherited)) {
            self.flag(
                span,
                format!("{} hands out a raw process::Command", item.sig.ident),
            );
        }
        let outer = std::mem::take(&mut self.tainted);
        syn::visit::visit_item_fn(self, item);
        self.tainted = outer;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let outer = std::mem::take(&mut self.tainted);
        syn::visit::visit_impl_item_fn(self, item);
        self.tainted = outer;
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        let pat = match &local.pat {
            syn::Pat::Type(typed) => &*typed.pat,
            pat => pat,
        };
        if let syn::Pat::Ident(binding) = pat {
            let name = binding.ident.to_string();
            self.tainted.retain(|tainted| *tainted != name);
            if (local.init.as_ref()).is_some_and(|init| self.stringified(&init.expr).is_some()) {
                self.tainted.push(name);
            }
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let names: Vec<String> = (path.segments.iter())
            .map(|segment| segment.ident.to_string())
            .collect();
        if !self.runner()
            && names
                .windows(2)
                .any(|pair| pair[0] == "process" && pair[1] == "Command")
        {
            self.flag(path.segments[0].ident.span(), names.join("::"));
        }
        let last = path.segments.last().map(|segment| &segment.ident);
        if let Some(ident) = last.filter(|ident| *ident == "configured_async")
            && !self.configures()
        {
            self.flag(
                ident.span(),
                "configured_async outside app_install's control".to_owned(),
            );
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if ["arg", "args", "env"]
            .iter()
            .any(|method| call.method == method)
        {
            self.texts(&call.args);
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(func) = &*call.func
            && func
                .path
                .segments
                .iter()
                .any(|segment| segment.ident == "Arg")
        {
            self.texts(&call.args);
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_array(&mut self, array: &'ast syn::ExprArray) {
        let args = array.elems.iter().any(|elem| match elem {
            syn::Expr::MethodCall(call) => call.method == "into",
            syn::Expr::Call(call) => matches!(&*call.func, syn::Expr::Path(func)
                if func.path.segments.iter().any(|segment| segment.ident == "Arg")),
            _ => false,
        });
        if args {
            self.texts(&array.elems);
        }
        syn::visit::visit_expr_array(self, array);
    }
}

struct Raw(Option<proc_macro2::Span>);

impl<'ast> Visit<'ast> for Raw {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        let names: Vec<String> = (path.segments.iter())
            .map(|segment| segment.ident.to_string())
            .collect();
        if names
            .windows(2)
            .any(|pair| pair[0] == "process" && pair[1] == "Command")
        {
            self.0.get_or_insert(path.segments[0].ident.span());
        }
        syn::visit::visit_path(self, path);
    }
}

const STRINGIFY: [&str; 5] = [
    "display",
    "to_str",
    "to_string_lossy",
    "as_os_str",
    "into_os_string",
];

struct Stringified<'a> {
    found: Option<proc_macro2::Span>,
    tainted: &'a [String],
}

impl<'ast> Visit<'ast> for Stringified<'_> {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if STRINGIFY.iter().any(|method| call.method == method) {
            self.found.get_or_insert(call.method.span());
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let confined = matches!(&*call.func, syn::Expr::Path(func)
            if func.path.segments.len() >= 2
                && func.path.segments.iter().rev().take(2).map(|segment| segment.ident.to_string()).eq(["new", "Confined"]));
        if !confined {
            syn::visit::visit_expr_call(self, call);
        }
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if let Some(name) = path.path.get_ident()
            && self.tainted.iter().any(|tainted| name == tainted)
        {
            self.found.get_or_insert(name.span());
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        fn find(tokens: proc_macro2::TokenStream) -> Option<proc_macro2::Span> {
            tokens.into_iter().find_map(|token| match token {
                proc_macro2::TokenTree::Ident(ident)
                    if STRINGIFY.iter().any(|method| ident == method) =>
                {
                    Some(ident.span())
                }
                proc_macro2::TokenTree::Group(group) => find(group.stream()),
                _ => None,
            })
        }
        if let Some(span) = find(mac.tokens.clone()) {
            self.found.get_or_insert(span);
        }
    }
}

#[test]
fn another_test_process_leaves_a_sandbox_whole_while_it_ends() {
    let test = "another_test_process_leaves_a_sandbox_whole_while_it_ends";
    if let Some(parent) = std::env::var_os("KOI_SANDBOX_TEST_PARENT") {
        drop(Sandbox::in_dir(Path::new(&parent)));
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::in_dir(parent.path());
    let root = sandbox.path().to_path_buf();
    let lifeline = sandbox.lifeline().to_path_buf();
    let exiting = std::fs::File::open(lifeline.join("alive")).unwrap();
    rustix::fs::flock(&exiting, rustix::fs::FlockOperation::LockShared).unwrap();
    let ending = std::thread::spawn(move || drop(sandbox));
    until("the sandbox never let its koi go", || {
        std::fs::File::open(lifeline.join("held")).is_ok_and(|held| {
            rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockShared).is_ok()
        })
    });

    let swept = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("KOI_SANDBOX_TEST_PARENT", parent.path())
        .output()
        .unwrap();
    let whole = root.exists();
    drop(exiting);
    let ended = ending.join();

    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
    assert!(
        whole,
        "another test process removed a sandbox that was still ending"
    );
    assert!(ended.is_ok());
    assert!(!root.exists());
}

#[test]
fn a_sweep_removes_only_an_abandoned_tree_a_sandbox_made() {
    let parent = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let private = |path: PathBuf| {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    };
    let elsewhere = private(other.path().join("koi-elsewhere"));
    let tree = |name: &str| {
        let lifeline = private(parent.path().join(format!("koi-lifeline-{name}")));
        std::fs::write(lifeline.join("owner"), "").unwrap();
        (lifeline, private(parent.path().join(format!("koi-{name}"))))
    };
    let (outside, _) = tree("outside");
    std::fs::write(outside.join("root"), elsewhere.as_os_str().as_bytes()).unwrap();
    let (shared, kept) = tree("shared");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let (living, running) = tree("living");
    let alive = std::fs::File::create(living.join("alive")).unwrap();
    rustix::fs::flock(&alive, rustix::fs::FlockOperation::LockShared).unwrap();
    let (abandoned, stale) = tree("abandoned");
    for (lifeline, root) in [(&shared, &kept), (&living, &running), (&abandoned, &stale)] {
        std::fs::write(lifeline.join("root"), root.as_os_str().as_bytes()).unwrap();
    }

    drop(Sandbox::in_dir(parent.path()));

    assert!(elsewhere.exists());
    assert!(kept.exists());
    assert!(running.exists() && living.exists());
    assert!(!stale.exists() && !abandoned.exists());
}
