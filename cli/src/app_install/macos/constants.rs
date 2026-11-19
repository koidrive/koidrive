use std::time::Duration;

pub(super) const BUNDLE: &str = "Koidrive.app";
pub(super) const IDENTIFIER: &str = "dev.koidrive.Koidrive";
pub(super) const REQUIREMENT: &str = "anchor apple generic and identifier \"dev.koidrive.Koidrive\" and certificate leaf[subject.OU] = \"656SPP5XR8\" and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";
pub(super) const SIGNER: &str = "anchor apple generic and certificate leaf[subject.OU] = \"656SPP5XR8\" and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";
pub(super) const CLEANUP: Duration = Duration::from_secs(20);
pub(super) const DOWNLOAD: Duration = Duration::from_secs(4 * 60 * 60);
pub(super) const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
pub(super) const INSPECT_RUNNING: &str = r#"ObjC.import('AppKit');
function run(args) {
    const inspectedBundlePath = ObjC.unwrap($(args[0]).stringByStandardizingPath);
    const apps = $.NSRunningApplication.runningApplicationsWithBundleIdentifier('dev.koidrive.Koidrive');
    for (let i = 0; i < apps.count; i++) {
        const app = apps.objectAtIndex(i);
        if (!app.terminated && app.bundleURL && ObjC.unwrap(app.bundleURL.path.stringByStandardizingPath) === inspectedBundlePath) return 'running';
    }
    return 'stopped';
}"#;
pub(super) const AWAIT_RUNNING: &str = r#"ObjC.import('AppKit');
function run(args) {
    const path = ObjC.unwrap($(args[0]).stringByStandardizingPath);
    const until = Date.now() + 10000;
    while (Date.now() < until) {
        const apps = $.NSRunningApplication.runningApplicationsWithBundleIdentifier('dev.koidrive.Koidrive');
        for (let i = 0; i < apps.count; i++) {
            const app = apps.objectAtIndex(i);
            if (!app.terminated && app.bundleURL && ObjC.unwrap(app.bundleURL.path.stringByStandardizingPath) === path) return 'running';
        }
        $.NSThread.sleepForTimeInterval(0.1);
    }
    return 'stopped';
}"#;
pub(crate) const QUIESCE: &str = r#"ObjC.import('AppKit');
function run(args) {
    const paths = args.map(p => ObjC.unwrap($(p).stringByStandardizingPath));
    const asked = [];
    let running = false;
    while (true) {
        const apps = $.NSRunningApplication.runningApplicationsWithBundleIdentifier('dev.koidrive.Koidrive');
        let remaining = false;
        for (let i = 0; i < apps.count; i++) {
            const app = apps.objectAtIndex(i);
            if (!app.bundleURL || paths.indexOf(ObjC.unwrap(app.bundleURL.path.stringByStandardizingPath)) < 0) continue;
            if (app.terminated) continue;
            remaining = true;
            running = true;
            if (asked.indexOf(app.processIdentifier) >= 0) continue;
            if (!app.terminate) throw Error('Koidrive refused graceful termination');
            asked.push(app.processIdentifier);
        }
        if (!remaining) return running ? 'running' : 'stopped';
        $.NSThread.sleepForTimeInterval(0.05);
    }
}"#;
