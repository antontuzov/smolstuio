//! How long the repository intelligence takes, on a tree of a size a real crate
//! is.
//!
//! The first turn of a run asks for a map of the repository and a ranking
//! against the task, so these are the milliseconds a person waits before a
//! model has produced a single token. A 4B model takes seconds to answer; this
//! is the part of the wait that is ours.
//!
//! The tree is synthetic and the same on every run: 300 source files across 20
//! directories, each declaring a few items, which is roughly a mid-sized Rust
//! crate. Timing it against this repository's own tree would measure the tree,
//! and the tree changes.
//!
//! Run this built for release; see the note in `agent-tools/benches`.

use std::path::Path;
use std::time::Instant;

fn measure(name: &str, iterations: usize, mut work: impl FnMut()) {
    work();
    let started = Instant::now();
    for _ in 0..iterations {
        work();
    }
    let per = started.elapsed().as_nanos() as f64 / iterations as f64;
    println!("{name:<34} {iterations:>6} runs  {per:>12.0} ns each");
}

/// A tree with something in it worth indexing: declarations to find, a
/// `Cargo.toml` to detect, an ignore file to honour.
fn make_tree(root: &Path) {
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"synthetic\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("a manifest");
    std::fs::write(root.join(".gitignore"), "target/\n").expect("an ignore file");
    for directory in 0..20 {
        let dir = root.join(format!("src/module_{directory}"));
        std::fs::create_dir_all(&dir).expect("a module directory");
        for file in 0..15 {
            let mut body = format!("//! Module {directory}, file {file}.\n\n");
            for item in 0..6 {
                body.push_str(&format!(
                    "pub fn fn_{directory}_{file}_{item}(value: u32) -> u32 {{ value + {item} }}\n"
                ));
            }
            body.push_str("pub struct Thing;\n");
            std::fs::write(dir.join(format!("file_{file}.rs")), body).expect("a source file");
        }
    }
    // The part an ignore file is supposed to keep out of the index.
    let ignored = root.join("target/debug");
    std::fs::create_dir_all(&ignored).expect("a build directory");
    for file in 0..50 {
        std::fs::write(
            ignored.join(format!("artifacts_{file}.rs")),
            "pub fn a() {}\n",
        )
        .expect("a build artifact");
    }
}

fn main() {
    let root = std::env::temp_dir().join("smoll-bench-repo-map");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("a scratch directory");
    make_tree(&root);

    // Walking the tree is disk work, so it runs fewer times; the index is what
    // everything else in this crate is built on.
    measure("index::walk", 20, || {
        std::hint::black_box(agent_repo::Index::walk(root.as_path()).len());
    });

    let index = agent_repo::Index::walk(root.as_path());
    println!("  indexed {} files of {}", index.len(), index.total_bytes());

    measure("project::detect", 200, || {
        std::hint::black_box(agent_repo::Project::detect(&index).describe());
    });

    // A map for the first turn, with and without a task to rank against: the
    // ranking is what reads file contents, so it is the expensive half.
    measure("repo_map::build_unranked", 50, || {
        std::hint::black_box(
            agent_repo::RepoMap::build(&index, &agent_repo::Project::detect(&index), None, 2_000)
                .tokens,
        );
    });

    let project = agent_repo::Project::detect(&index);
    measure("repo_map::build_for_a_task", 50, || {
        std::hint::black_box(
            agent_repo::RepoMap::build(
                &index,
                &project,
                Some("rename the thing in module 7 and fix its callers"),
                2_000,
            )
            .tokens,
        );
    });

    measure("map::context_for_a_task", 50, || {
        std::hint::black_box(
            agent_repo::map::context(
                &index,
                "rename the thing in module 7 and fix its callers",
                1_200,
                6,
            )
            .len(),
        );
    });

    std::fs::remove_dir_all(&root).ok();
}
