// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

use koidrive_test_support::Sandbox;

#[path = "support/process.rs"]
mod process;

fn koi(sandbox: &Sandbox) -> Command {
    let mut command = process::koi(sandbox.path());
    command.env("HOME", sandbox.path());
    command
}

fn koidrive(args: &[&str]) -> std::process::Output {
    koi(&Sandbox::new()).args(args).output().expect("run koi")
}

#[test]
fn prints_its_version() {
    let output = koidrive(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("koi {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn plain_help_and_no_color_override_forced_styles() {
    for (args, vars) in [
        (vec!["--plain", "--help"], vec![]),
        (vec!["--help"], vec![("NO_COLOR", "1")]),
        (vec!["--help"], vec![("KOI_PLAIN", "1")]),
        (vec!["--help"], vec![("TERM", "dumb")]),
    ] {
        let sandbox = Sandbox::new();
        let mut command = koi(&sandbox);
        command
            .args(args)
            .env_remove("NO_COLOR")
            .env_remove("KOI_PLAIN")
            .env("TERM", "xterm-256color")
            .env("CLICOLOR_FORCE", "1")
            .env("FORCE_COLOR", "1");
        for (name, value) in vars {
            command.env(name, value);
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.contains(&0x1b));
        assert!(!output.stderr.contains(&0x1b));
    }
}

#[test]
fn an_empty_no_color_does_not_override_explicit_forced_color() {
    let output = koi(&Sandbox::new())
        .args(["--help"])
        .env("NO_COLOR", "")
        .env_remove("KOI_PLAIN")
        .env("TERM", "xterm-256color")
        .env("CLICOLOR_FORCE", "1")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.contains(&0x1b));
}

#[test]
fn alone_or_with_a_group_alone_it_shows_help_and_succeeds() {
    for args in [&[][..], &["folder"], &["history"], &["server"]] {
        let output = koidrive(args);
        let help = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}: {help}");
        assert!(help.contains("Usage: koi"), "{args:?}: {help}");
    }
    let folder = String::from_utf8(koidrive(&["folder"]).stdout).unwrap();
    assert!(folder.contains("Usage: koi folder"), "{folder}");
    let help = String::from_utf8(koidrive(&[]).stdout).unwrap();
    assert!(help.contains("Koidrive sync client"), "{help}");
    assert!(!help.contains("_daemon"), "{help}");
}
