// SPDX-License-Identifier: Apache-2.0

use std::io::Write;
use std::time::{Duration, Instant};

use koidrive_engine::classify::Trees;
use koidrive_engine::ignore::{RuleFile, Source};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};

const ROOT: &str = "root";
const ROUNDS: usize = 21;

fn shape(parent: &str, name: &str, kind: Kind, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind,
        content: (kind == Kind::File).then(|| Content {
            size: 3,
            manifest_digest: vec![digest; 32],
            ..Default::default()
        }),
    }
}

fn add(trees: &mut Trees, id: String, shape: Shape) {
    let ino = trees.remote().len() as u128 + 1;
    trees.set_remote(
        id.clone(),
        RemoteNode {
            made_over: Vec::new(),
            shape: shape.clone(),
            trashed: false,
            content_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: 1,
            pending_for: None,
            pending_by: None,
        },
    );
    trees.set_local(
        id.clone(),
        LocalNode {
            displaced: false,
            left_out_when_placed: false,
            content_from_server: false,
            shape: shape.clone(),
            identity: FileIdentity {
                ino,
                birth_ns: Some(0),
                handle: None,
            },
            parked: false,
        },
    );
    trees.set_synced(id, Some(shape));
}

fn median(trees: &mut Trees, mut change: impl FnMut(&mut Trees, usize)) -> Duration {
    let mut times: Vec<Duration> = (0..ROUNDS)
        .map(|round| {
            change(trees, round);
            let started = Instant::now();
            trees.settle(&Default::default());
            started.elapsed()
        })
        .collect();
    times.sort();
    times[ROUNDS / 2]
}

fn report(trees: &mut Trees, nodes: usize, label: &str) {
    let idle = median(trees, |_, _| {});
    let one_file = median(trees, |trees, _| {
        let mut node = trees.local().get("w0").unwrap().clone();
        let edited = shape(ROOT, "w0", Kind::File, 2).content;
        let digest = if node.shape.content == edited { 3 } else { 2 };
        node.shape.content = shape(ROOT, "w0", Kind::File, digest).content;
        trees.set_local_changed("w0".into(), node);
    });
    let mut stderr = std::io::stderr();
    writeln!(
        stderr,
        "BENCH nodes={nodes} {label} idle_settle_ns={} one_file_settle_ns={}",
        idle.as_nanos(),
        one_file.as_nanos()
    )
    .unwrap();
}

#[test]
#[ignore = "a benchmark: run it with --ignored in a release build"]
fn settle_cost_while_the_rules_leave_out_a_large_folder() {
    let nodes: usize =
        std::env::var("KOIDRIVE_BENCH_NODES").map_or(110_000, |nodes| nodes.parse().unwrap());
    assert!(nodes >= 102, "KOIDRIVE_BENCH_NODES must be at least 102");
    let mut trees = Trees::new(ROOT);
    for file in 0..100 {
        add(
            &mut trees,
            format!("w{file}"),
            shape(ROOT, &format!("w{file}.docx"), Kind::File, 1),
        );
    }
    add(&mut trees, "big".into(), shape(ROOT, "big", Kind::Dir, 0));
    let mut big = vec!["big".to_owned()];
    for folder in 0..100 {
        let id = format!("b{folder}");
        add(&mut trees, id.clone(), shape("big", &id, Kind::Dir, 0));
        big.push(id);
    }
    for file in 0..nodes.saturating_sub(big.len()) {
        let (id, folder) = (format!("f{file}"), format!("b{}", file % 100));
        add(
            &mut trees,
            id.clone(),
            shape(&folder, &format!("{id}.o"), Kind::File, 1),
        );
        big.push(id);
    }
    trees.settle(&Default::default());
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"big/\n")),
    );
    trees.settle(&Default::default());
    assert!(trees.excluded("f0"));
    report(&mut trees, big.len(), "excluded");
    for id in big.iter().rev() {
        trees.remove_local(id);
    }
    trees.settle(&Default::default());
    report(&mut trees, big.len(), "excluded_and_removed");
}
