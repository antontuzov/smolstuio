//! What the secret mask costs, on output the size of a command that talks a lot.
//!
//! Every tool's output passes through the redactor before the model is shown it,
//! so the mask is on the path of every step. A `cargo test` run, a `git log` of a
//! busy repository and a `.env` dump are the three shapes it exists for, and
//! they are the three measured here: a large body of text with nothing secret in
//! it, the same text with a credential on every hundredth line, and a config
//! dump where most lines are secret.
//!
//! The point of the first row is that ordinary text is not slow: the scan is one
//! pass over the bytes, and a file full of `key=value` pairs the redactor must
//! look at closely is the worst case, not the common one.
//!
//! Run this built for release; see the note in `agent-tools/benches`.

use std::time::Instant;

fn measure(name: &str, text: &str, iterations: usize) {
    let once = agent_sandbox::redact(text);
    let started = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(agent_sandbox::redact(text));
    }
    let per = started.elapsed().as_nanos() as f64 / iterations as f64;
    // The per-byte figure is the one worth reading: these three inputs are
    // different sizes, and a scan over a big file that does nothing is not
    // slower than a scan over a small one that masks most of it.
    println!(
        "{name:<30} {per:>10.0} ns each  {:>6.1} ns/byte  masked {:>4}  {} bytes in",
        per / text.len() as f64,
        once.1.masked,
        text.len()
    );
}

/// A build log: lines of prose and paths, no credential anywhere in it.
fn ordinary_log(lines: usize) -> String {
    (0..lines)
        .map(|line| {
            format!(
                "   Compiling synthetic_crate_{line} v0.1.0 ({})\n    \
                 Finished dev [unoptimized + debuginfo] target(s) in 1.2s\n",
                "/Users/dev/.cargo/registry/src/index.crates.io-1124/src"
            )
        })
        .collect()
}

/// The same log with a credential on every hundredth line, which is what a
/// leaked token mixed into build noise looks like.
fn log_with_secrets(lines: usize) -> String {
    (0..lines)
        .map(|line| {
            if line % 100 == 0 {
                format!(
                    "POST https://api.example.com/v1 with Authorization: Bearer \
                     sk-proxy-{} and retry in 3s\n",
                    "abc123def456abc123"
                )
            } else {
                format!("   Compiling synthetic_crate_{line} v0.1.0\n")
            }
        })
        .collect()
}

/// A `.env` dump: nearly every line is a name and a value, so nearly every line
/// reaches the part of the scan that decides.
fn config_dump(lines: usize) -> String {
    (0..lines)
        .map(|line| {
            if line % 3 == 0 {
                format!("SERVICE_{line}_API_TOKEN=xy{}z\n", "0123456789abcdef")
            } else {
                format!("SERVICE_{line}_HOST=internal.example.com\n")
            }
        })
        .collect()
}

fn main() {
    let quiet = ordinary_log(5_000);
    let leaked = log_with_secrets(5_000);
    let dump = config_dump(2_000);

    measure("redact: ordinary log", &quiet, 200);
    measure("redact: log with secrets", &leaked, 200);
    measure("redact: config dump", &dump, 200);

    // A single small output, the shape of most tool results: the fixed cost the
    // mask pays even when there is nothing to mask.
    measure("redact: one short file", "old text\n", 20_000);
}
