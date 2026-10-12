//! What the agent's hot paths cost in wall-clock, on this machine, right now.
//!
//! The rest of this repository quotes *token* costs — what a tool list or a repo
//! map takes out of a small model's window. Those are deterministic and a test
//! can assert them. Time is not: it depends on the machine, the file system and
//! whether the build was `--release`. So these benches state their conditions on
//! every line and compare nothing to anything. `cargo bench -p agent-tools`
//! re-measures them; there are no stored numbers here to go stale.
//!
//! Run them built for release. A debug build times `Vec` pushes and string
//! formatting the compiler would have removed, which measures nothing anyone
//! wants to know.

use std::time::Instant;

/// One measurement, printed with the conditions attached to it.
fn measure(name: &str, iterations: usize, mut work: impl FnMut()) {
    // One pass to touch the pages and fill the caches, not counted.
    work();
    let started = Instant::now();
    for _ in 0..iterations {
        work();
    }
    let per = started.elapsed().as_nanos() as f64 / iterations as f64;
    println!("{name:<34} {iterations:>6} runs  {per:>12.0} ns each");
}

fn main() {
    let root = std::env::temp_dir().join("smoll-bench-list-pricing");
    std::fs::create_dir_all(&root).expect("a directory to point the policy at");
    let policy = agent_sandbox::Policy::new(root.clone());

    // The registry is built once per run by the loop, and once per `smoll tools`
    // call, so its build is cheap next to a model turn — but it is the thing a
    // 25th tool would make slower for everyone.
    measure("registry::with_defaults", 2_000, || {
        let registry = agent_tools::Registry::with_defaults();
        std::hint::black_box(registry.len());
    });

    // The list price is summed on every turn, by `prompt_tokens()`.
    let priced = agent_tools::Registry::with_defaults();
    measure("registry::token_cost", 20_000, || {
        std::hint::black_box(priced.token_cost());
    });

    // Every turn filters the list against the policy before asking the provider.
    measure("registry::enabled_by", 2_000, || {
        let filtered = agent_tools::Registry::with_defaults().enabled_by(&policy);
        std::hint::black_box(filtered.len());
    });

    // `smoll tools` and `smoll doctor` render this; a person waits for it.
    measure("registry::inventory", 2_000, || {
        std::hint::black_box(priced.inventory(&policy).len());
    });

    // The provider layer is handed these on every request, and each definition
    // carries its parameter schema, so this is the copy the wire is built from.
    measure("definitions::clone", 2_000, || {
        std::hint::black_box(priced.definitions().len());
    });

    std::fs::remove_dir_all(&root).ok();
}
