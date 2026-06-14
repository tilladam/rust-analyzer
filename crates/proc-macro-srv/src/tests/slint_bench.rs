//! Server-side profiling harness for the `slint!` proc-macro.
//!
//! The integrated benchmark in the `rust-analyzer` crate
//! (`integrated_slint_macro_expansion_benchmark`) measures expansion *through* the IPC
//! boundary and shows that essentially all of the cost is spent inside the proc-macro
//! server running slint's own codegen. This harness drives that codegen **in-process**,
//! so the actual work can be profiled directly -- e.g. under `samply`/`perf` for a
//! flamegraph that includes slint's own symbols, which is impossible across the IPC
//! boundary.
//!
//! It loads the `slint-macros` dylib that `cargo` built for one of the testcases, lexes
//! the `slint!` macro body from the source file, and expands it in a loop.
//!
//! IMPORTANT: `proc-macro-srv` checks the dylib's rustc version *exactly*, so the testcase
//! must be built with the same nightly toolchain used to run this test, e.g.:
//! ```bash
//! ( cd $SLINT_TESTCASE && RUSTUP_TOOLCHAIN=nightly cargo build )
//! ```
//!
//! Coarse timing (debug):
//! ```bash
//! SLINT_TESTCASE=$HOME/Code/slint-ra-testcases/crates/size_xlarge RUN_SLOW_BENCHES=1 \
//!   RUSTUP_TOOLCHAIN=nightly cargo test -p proc-macro-srv --features in-rust-tree \
//!   tests::slint_bench::slint_server_side_expansion_benchmark -- --nocapture --exact
//! ```
//!
//! Flamegraph of slint's internals -- uses an in-process, signal-based sampler (`pprof`),
//! so it needs no `perf_event_open`/root and works where `perf`/`samply` are unavailable.
//! Enable the `slint-bench-pprof` feature and point `SLINT_BENCH_PPROF` at an output file.
//! Use `--release` for representative stacks:
//! ```bash
//! SLINT_TESTCASE=$HOME/Code/slint-ra-testcases/crates/size_xlarge \
//!   SLINT_BENCH_ITERS=200 SLINT_BENCH_PPROF=/tmp/slint-expansion.svg RUN_SLOW_BENCHES=1 \
//!   RUSTUP_TOOLCHAIN=nightly cargo test --release -p proc-macro-srv \
//!   --features "in-rust-tree slint-bench-pprof" \
//!   tests::slint_bench::slint_server_side_expansion_benchmark -- --nocapture --exact
//! ```
//!
//! Env vars: `SLINT_TESTCASE` (project dir, default `~/Code/slint-ra-testcases/crates/size_xlarge`),
//! `SLINT_TESTCASE_FILE` (default `src/lib.rs`), `SLINT_BENCH_ITERS` (default 50),
//! `SLINT_BENCH_PPROF` (flamegraph output path; requires the `slint-bench-pprof` feature).

use std::time::Instant;

use paths::Utf8PathBuf;

use crate::{ProcMacroKind, SpanId, dylib, token_stream::TokenStream};

fn env_or(var: &str, default: impl Into<String>) -> String {
    std::env::var(var).unwrap_or_else(|_| default.into())
}

/// Locate the freshly-built `slint-macros` dylib under `<project>/target/{debug,release}/deps`.
fn find_slint_dylib(project: &str) -> Utf8PathBuf {
    let ext = if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let prefix = if cfg!(target_os = "windows") { "slint_macros" } else { "libslint_macros" };

    let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for profile in ["debug", "release"] {
        let deps = std::path::Path::new(project).join("target").join(profile).join("deps");
        let Ok(entries) = std::fs::read_dir(&deps) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(prefix) && name.ends_with(ext) {
                if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                    candidates.push((modified, path));
                }
            }
        }
    }
    candidates.sort_by_key(|(t, _)| *t);
    let (_, path) = candidates.pop().unwrap_or_else(|| {
        panic!(
            "no slint-macros dylib found under {project}/target/*/deps -- \
             run `cargo build` (or `cargo check`) in the testcase first"
        )
    });
    Utf8PathBuf::from_path_buf(path).expect("dylib path is not utf-8")
}

/// Extract the token text inside the outermost braces of the `slint!` invocation, skipping
/// string/char literals and line comments so braces inside them don't confuse the matcher.
fn extract_macro_body(src: &str) -> String {
    let bang = src.find("slint!").expect("no `slint!` invocation in source file");
    let open = src[bang..].find('{').expect("no `{` after `slint!`") + bang;

    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    let mut in_str = false;
    let mut in_char = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
        } else if in_char {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '\'' {
                in_char = false;
            }
        } else if c == '/' && bytes.get(i + 1) == Some(&b'/') {
            // line comment: skip to end of line
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        } else {
            match c {
                '"' => in_str = true,
                '\'' => in_char = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        // Return the contents between the outer braces.
                        return src[open + 1..i].to_owned();
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    panic!("unbalanced braces in `slint!` invocation");
}

/// Load the slint-macros dylib for `project` and resolve the function-like (bang) macro name.
/// Returns the `TempDir` (the dylib is copied into it and must outlive the expander), the
/// loaded expander, and the macro name.
fn load_slint_expander(project: &str) -> (temp_dir::TempDir, dylib::Expander, String) {
    let dylib_path = find_slint_dylib(project);
    let temp = temp_dir::TempDir::new().unwrap();
    let load_start = Instant::now();
    let expander = dylib::Expander::new(&temp, &dylib_path)
        .unwrap_or_else(|e| panic!("failed to load {dylib_path}: {e}"));
    eprintln!("dylib:      {dylib_path}");
    eprintln!("dylib load: {:?}", load_start.elapsed());

    let macros: Vec<(String, ProcMacroKind)> =
        expander.list_macros().map(|(n, k)| (n.to_owned(), k)).collect();
    let name = macros
        .iter()
        .find(|(n, k)| matches!(k, ProcMacroKind::Bang) && n == "slint")
        .or_else(|| macros.iter().find(|(_, k)| matches!(k, ProcMacroKind::Bang)))
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| panic!("no bang macro in dylib; found {macros:?}"));
    (temp, expander, name)
}

/// Expand `body` once as warm-up, then `iters` times; return mean ms/expansion and the
/// number of top-level tokens produced.
fn time_expansion(expander: &dylib::Expander, name: &str, body: &str, iters: u32) -> (f64, usize) {
    let call_site = SpanId(1);
    let body_ts = TokenStream::from_str(body, call_site)
        .unwrap_or_else(|e| panic!("failed to lex macro body: {e}"));
    let expand_once = || {
        expander
            .expand(
                name,
                body_ts.clone(),
                None,
                SpanId(0),
                call_site,
                SpanId(2),
                &mut Default::default(),
                None,
            )
            .unwrap_or_else(|e| panic!("expansion failed: {:?}", e.into_string()))
    };
    let tokens = expand_once().len();
    let start = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(expand_once());
    }
    (start.elapsed().as_secs_f64() * 1000.0 / iters as f64, tokens)
}

#[test]
fn slint_server_side_expansion_benchmark() {
    if std::env::var("RUN_SLOW_BENCHES").is_err() {
        return;
    }

    let project = env_or(
        "SLINT_TESTCASE",
        format!("{}/Code/slint-ra-testcases/crates/size_xlarge", std::env::var("HOME").unwrap()),
    );
    let rel_file = env_or("SLINT_TESTCASE_FILE", "src/lib.rs");
    let iters: u32 =
        env_or("SLINT_BENCH_ITERS", "50").parse().expect("SLINT_BENCH_ITERS not a u32");

    let src = std::fs::read_to_string(std::path::Path::new(&project).join(&rel_file))
        .unwrap_or_else(|e| panic!("cannot read {project}/{rel_file}: {e}"));
    let body = extract_macro_body(&src);
    eprintln!("macro body: {} bytes from {rel_file}", body.len());

    let (_temp, expander, name) = load_slint_expander(&project);
    eprintln!("expanding:  {name}!");

    let def_site = SpanId(0);
    let call_site = SpanId(1);
    let mixed_site = SpanId(2);
    let body_ts = TokenStream::from_str(&body, call_site)
        .unwrap_or_else(|e| panic!("failed to lex macro body: {e}"));

    let expand_once = || {
        expander
            .expand(
                &name,
                body_ts.clone(),
                None,
                def_site,
                call_site,
                mixed_site,
                &mut Default::default(),
                None,
            )
            .unwrap_or_else(|e| panic!("expansion failed: {:?}", e.into_string()))
    };

    // Warm-up: first expansion also pays slint's one-time init (interners, etc.).
    let warm_start = Instant::now();
    let first = expand_once();
    eprintln!(
        "cold expand: {:?} (produced {} top-level tokens)",
        warm_start.elapsed(),
        first.len()
    );

    // Optionally sample the steady-state loop with an in-process, signal-based profiler
    // (no `perf_event_open` / root needed) and write a flamegraph of slint's internals.
    #[cfg(feature = "slint-bench-pprof")]
    let _guard = std::env::var("SLINT_BENCH_PPROF").ok().map(|out| {
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(997)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .expect("failed to start pprof");
        eprintln!("pprof: sampling at 997 Hz -> {out}");
        (guard, out)
    });

    // Steady-state loop -- this is the body sampled under a profiler.
    let loop_start = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(expand_once());
    }
    let total = loop_start.elapsed();
    eprintln!(
        "warm expand: {iters} iters in {total:?} -> {:.2} ms/expansion",
        total.as_secs_f64() * 1000.0 / iters as f64
    );

    #[cfg(feature = "slint-bench-pprof")]
    if let Some((guard, out)) = _guard {
        let report = guard.report().build().expect("failed to build pprof report");
        let file = std::fs::File::create(&out).expect("cannot create flamegraph file");
        report.flamegraph(file).expect("failed to write flamegraph");
        eprintln!("pprof: wrote flamegraph to {out}");
    }
}

/// Quantifies how much of each `slint!` expansion is spent re-loading and re-compiling the
/// imported `std-widgets.slint` library -- work that is identical across expansions and could
/// be cached, but currently is not (slint builds a fresh compiler per invocation, so the
/// import is parsed, type-checked and lowered again on every keystroke inside the macro).
///
/// It expands a trivial component with and without a `std-widgets.slint` import; the delta
/// over the no-import baseline is the per-expansion import-reload cost. The full testcase body
/// is timed too for reference. Run like `slint_server_side_expansion_benchmark` (nightly +
/// `--features in-rust-tree`), e.g.:
/// ```bash
/// SLINT_TESTCASE=$HOME/Code/slint-ra-testcases/crates/size_xlarge \
///   SLINT_BENCH_ITERS=40 RUN_SLOW_BENCHES=1 RUSTUP_TOOLCHAIN=nightly \
///   cargo test --release -p proc-macro-srv --features in-rust-tree \
///   tests::slint_bench::slint_import_cost_breakdown -- --nocapture --exact
/// ```
#[test]
fn slint_import_cost_breakdown() {
    if std::env::var("RUN_SLOW_BENCHES").is_err() {
        return;
    }

    let project = env_or(
        "SLINT_TESTCASE",
        format!("{}/Code/slint-ra-testcases/crates/size_xlarge", std::env::var("HOME").unwrap()),
    );
    let iters: u32 =
        env_or("SLINT_BENCH_ITERS", "40").parse().expect("SLINT_BENCH_ITERS not a u32");

    let (_temp, expander, name) = load_slint_expander(&project);

    // The same wide import the size_xlarge testcase uses, so the load cost is comparable.
    let std_import = "import { Button, Slider, ComboBox, CheckBox, LineEdit, ProgressIndicator, \
         VerticalBox, HorizontalBox, GroupBox, TabWidget } from \"std-widgets.slint\";";
    let cases: [(&str, String); 3] = [
        ("no-import    ", "export component Bench { Rectangle { width: 10px; } }".to_owned()),
        (
            "import-unused",
            format!("{std_import}\nexport component Bench {{ Rectangle {{ width: 10px; }} }}"),
        ),
        (
            "import-used  ",
            format!(
                "{std_import}\nexport component Bench {{ VerticalBox {{ Button {{ text: \"x\"; }} }} }}"
            ),
        ),
    ];

    eprintln!("\n{name}! expansion cost ({iters} iters each):");
    let mut baseline = None::<f64>;
    for (label, body) in &cases {
        let (ms, tokens) = time_expansion(&expander, &name, body, iters);
        let delta = match baseline {
            Some(b) => format!("   +{:6.2} ms vs no-import (import reload)", ms - b),
            None => String::new(),
        };
        eprintln!("  {label}: {ms:7.2} ms/expansion  ({tokens:>4} tokens){delta}");
        baseline.get_or_insert(ms);
    }

    // Reference: the real testcase body.
    let rel_file = env_or("SLINT_TESTCASE_FILE", "src/lib.rs");
    if let Ok(src) = std::fs::read_to_string(std::path::Path::new(&project).join(&rel_file)) {
        let body = extract_macro_body(&src);
        let (ms, tokens) = time_expansion(&expander, &name, &body, iters);
        eprintln!("  full file    : {ms:7.2} ms/expansion  ({tokens:>4} tokens)   [{rel_file}]");
    }
}
