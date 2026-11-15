use super::*;

#[tokio::test]
async fn owned_retired_completion_removes_only_the_retired_tree() {
    let (fixture, retired) = retired_fixture().await;
    let parent = retired.bundle.parent().unwrap().to_path_buf();
    let nested = retired.bundle.join("Contents/Versions/A");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("data"), "owned data").unwrap();
    symlink("A", nested.parent().unwrap().join("Current")).unwrap();
    retired.discard().unwrap();
    assert!(!parent.exists());
    assert_eq!(
        fs::read(fixture.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    fixture.unmounted();
}

#[tokio::test]
async fn owned_cleanup_unlinks_partial_and_cyclic_links_without_following_them() {
    let (fixture, retired) = retired_fixture().await;
    let contents = retired.bundle.join("Contents");
    symlink("missing", contents.join("dangling")).unwrap();
    symlink("second", contents.join("first")).unwrap();
    symlink("first", contents.join("second")).unwrap();
    symlink("../missing", contents.join("parent-relative")).unwrap();
    let parent = retired.bundle.parent().unwrap().to_path_buf();
    retired.discard().unwrap();
    assert!(!parent.exists());
    assert_eq!(
        fs::read(fixture.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    fixture.unmounted();
}

#[tokio::test]
async fn retired_completion_refuses_changed_or_symlinked_ownership() {
    for variant in [
        "root",
        "root-symlink",
        "parent",
        "parent-symlink",
        "bundle",
        "bundle-symlink",
        "field",
        "escape",
        "relative-escape",
        "extra-entry",
        "receipt-contents",
    ] {
        let (fixture, mut retired) = retired_fixture().await;
        let parent = retired.bundle.parent().unwrap().to_path_buf();
        let displaced = fixture.base().join("displaced");
        let outside = fixture.base().join("outside");
        bundle(&outside, Some("9.0.0"));
        match variant {
            "root" | "root-symlink" => {
                fs::rename(&fixture.config.system, &displaced).unwrap();
                if variant == "root" {
                    fs::create_dir(&fixture.config.system).unwrap();
                    fs::write(fixture.config.system.join("keep"), "unrelated").unwrap();
                } else {
                    symlink(&displaced, &fixture.config.system).unwrap();
                }
            }
            "parent" | "parent-symlink" => {
                fs::rename(&parent, &displaced).unwrap();
                if variant == "parent" {
                    fs::create_dir(&parent).unwrap();
                    bundle(&parent.join(BUNDLE), Some("9.0.0"));
                } else {
                    symlink(&displaced, &parent).unwrap();
                }
            }
            "bundle" | "bundle-symlink" => {
                fs::rename(&retired.bundle, &displaced).unwrap();
                if variant == "bundle" {
                    bundle(&retired.bundle, Some("9.0.0"));
                } else {
                    symlink(&outside, &retired.bundle).unwrap();
                }
            }
            "field" => retired.bundle = outside.clone(),
            "escape" => symlink(&outside, retired.bundle.join("escape")).unwrap(),
            "relative-escape" => {
                symlink("../../../outside", retired.bundle.join("escape")).unwrap();
            }
            "extra-entry" => {
                fs::write(parent.join("unowned"), "keep").unwrap();
            }
            "receipt-contents" => fs::write(retired.ownership.receipt.path(), b"{}").unwrap(),
            _ => unreachable!(),
        }
        let error = retired.discard().unwrap_err();
        assert!(
            matches!(
                error.kind,
                AppInstallFailure::UnsafePath | AppInstallFailure::ConcurrentChange
            ),
            "{variant}: {error:?}"
        );
        assert!(!error.residue.is_empty());
        assert_eq!(
            fs::read(outside.join("Contents/Helpers/koi")).unwrap(),
            b"9.0.0",
            "{variant}"
        );
        if matches!(variant, "parent" | "parent-symlink") {
            assert_eq!(
                fs::read(displaced.join("Koidrive.app/Contents/Helpers/koi")).unwrap(),
                b"0.1.0"
            );
        } else if matches!(variant, "bundle" | "bundle-symlink") {
            assert_eq!(
                fs::read(displaced.join("Contents/Helpers/koi")).unwrap(),
                b"0.1.0"
            );
        }
        if !matches!(variant, "root" | "root-symlink") {
            assert_eq!(
                fs::read(fixture.installed().join("Contents/Helpers/koi")).unwrap(),
                TARGET.as_bytes()
            );
        }
    }
}
