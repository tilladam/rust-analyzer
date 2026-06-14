//! Fully integrated benchmarks for rust-analyzer, which load real cargo
//! projects.
//!
//! The benchmark here is used to debug specific performance regressions. If you
//! notice that, eg, completion is slow in some specific case, you can  modify
//! code here exercise this specific completion, and thus have a fast
//! edit/compile/test cycle.
//!
//! Note that "rust-analyzer: Run" action does not allow running a single test
//! in release mode in VS Code. There's however "rust-analyzer: Copy Run Command Line"
//! which you can use to paste the command in terminal and add `--release` manually.

use hir::{ChangeWithProcMacros, Semantics};
use ide::{
    AnalysisHost, CallableSnippets, CompletionConfig, CompletionFieldsToResolve, DiagnosticsConfig,
    FilePosition, RaFixtureConfig, TextSize,
};
use ide_db::{
    RootDatabase, SnippetCap,
    imports::insert_use::{ImportGranularity, InsertUseConfig},
};
use project_model::CargoConfig;
use test_utils::project_root;
use vfs::{AbsPathBuf, VfsPath};

use load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_at};

#[track_caller]
fn file_id(vfs: &vfs::Vfs, path: &VfsPath) -> vfs::FileId {
    match vfs.file_id(path) {
        Some((file_id, vfs::FileExcluded::No)) => file_id,
        None | Some((_, vfs::FileExcluded::Yes)) => panic!("can't find virtual file for {path}"),
    }
}

#[test]
fn integrated_highlighting_benchmark() {
    if std::env::var("RUN_SLOW_BENCHES").is_err() {
        return;
    }

    // Load rust-analyzer itself.
    let workspace_to_load = project_root();
    let file = "./crates/rust-analyzer/src/config.rs";

    let cargo_config = CargoConfig {
        sysroot: Some(project_model::RustLibSource::Discover),
        all_targets: true,
        set_test: true,
        ..CargoConfig::default()
    };
    let load_cargo_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };

    let (db, vfs, _proc_macro) = {
        let _it = stdx::timeit("workspace loading");
        load_workspace_at(
            workspace_to_load.as_std_path(),
            &cargo_config,
            &load_cargo_config,
            &|_| {},
        )
        .unwrap()
    };
    let mut host = AnalysisHost::with_database(db);

    let file_id = {
        let file = workspace_to_load.join(file);
        let path = VfsPath::from(AbsPathBuf::assert(file));
        file_id(&vfs, &path)
    };

    {
        let _it = stdx::timeit("initial");
        let analysis = host.analysis();
        analysis.highlight_as_html(file_id, false).unwrap();
    }

    {
        let _it = stdx::timeit("change");
        let mut text = host.analysis().file_text(file_id).unwrap().to_string();
        text = text.replace(
            "self.data.cargo_buildScripts_rebuildOnSave",
            "self. data. cargo_buildScripts_rebuildOnSave",
        );
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file_id, Some(text));
        host.apply_change(change);
    }

    let _g = crate::tracing::hprof::init("*>10");

    {
        let _it = stdx::timeit("after change");
        let _span = profile::cpu_span();
        let analysis = host.analysis();
        analysis.highlight_as_html(file_id, false).unwrap();
    }
}

#[test]
fn integrated_completion_benchmark() {
    if std::env::var("RUN_SLOW_BENCHES").is_err() {
        return;
    }

    // Load rust-analyzer itself.
    let workspace_to_load = project_root();
    let file = "./crates/hir/src/lib.rs";

    let cargo_config = CargoConfig {
        sysroot: Some(project_model::RustLibSource::Discover),
        all_targets: true,
        set_test: true,
        ..CargoConfig::default()
    };
    let load_cargo_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: true,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };

    let (db, vfs, _proc_macro) = {
        let _it = stdx::timeit("workspace loading");
        load_workspace_at(
            workspace_to_load.as_std_path(),
            &cargo_config,
            &load_cargo_config,
            &|_| {},
        )
        .unwrap()
    };
    let mut host = AnalysisHost::with_database(db);

    let file_id = {
        let file = workspace_to_load.join(file);
        let path = VfsPath::from(AbsPathBuf::assert(file));
        file_id(&vfs, &path)
    };

    // kick off parsing and index population

    let completion_offset = {
        let _it = stdx::timeit("change");
        let mut text = host.analysis().file_text(file_id).unwrap().to_string();
        let completion_offset =
            patch(&mut text, "db.struct_signature(self.id)", "sel;\ndb.struct_signature(self.id)")
                + "sel".len();
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file_id, Some(text));
        host.apply_change(change);
        completion_offset
    };

    {
        let _span = profile::cpu_span();
        let analysis = host.analysis();
        let config = completion_config();
        let position =
            FilePosition { file_id, offset: TextSize::try_from(completion_offset).unwrap() };
        analysis.completions(&config, position, None).unwrap();
    }

    let _g = crate::tracing::hprof::init("*>10");

    let completion_offset = {
        let _it = stdx::timeit("change");
        let mut text = host.analysis().file_text(file_id).unwrap().to_string();
        let completion_offset = patch(
            &mut text,
            "sel;\ndb.struct_signature(self.id)",
            ";sel;\ndb.struct_signature(self.id)",
        ) + ";sel".len();
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file_id, Some(text));
        host.apply_change(change);
        completion_offset
    };

    {
        let _p = tracing::info_span!("unqualified path completion").entered();
        let _span = profile::cpu_span();
        let analysis = host.analysis();
        let config = completion_config();
        let position =
            FilePosition { file_id, offset: TextSize::try_from(completion_offset).unwrap() };
        analysis.completions(&config, position, None).unwrap();
    }

    let completion_offset = {
        let _it = stdx::timeit("change");
        let mut text = host.analysis().file_text(file_id).unwrap().to_string();
        let completion_offset = patch(
            &mut text,
            "sel;\ndb.struct_signature(self.id)",
            "self.;\ndb.struct_signature(self.id)",
        ) + "self.".len();
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file_id, Some(text));
        host.apply_change(change);
        completion_offset
    };

    {
        let _p = tracing::info_span!("dot completion").entered();
        let _span = profile::cpu_span();
        let analysis = host.analysis();
        let config = completion_config();
        let position =
            FilePosition { file_id, offset: TextSize::try_from(completion_offset).unwrap() };
        analysis.completions(&config, position, None).unwrap();
    }
}

#[test]
fn integrated_diagnostics_benchmark() {
    if std::env::var("RUN_SLOW_BENCHES").is_err() {
        return;
    }

    // Load rust-analyzer itself.
    let workspace_to_load = project_root();
    let file = "./crates/hir/src/lib.rs";

    let cargo_config = CargoConfig {
        sysroot: Some(project_model::RustLibSource::Discover),
        all_targets: true,
        set_test: true,
        ..CargoConfig::default()
    };
    let load_cargo_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: true,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };

    let (db, vfs, _proc_macro) = {
        let _it = stdx::timeit("workspace loading");
        load_workspace_at(
            workspace_to_load.as_std_path(),
            &cargo_config,
            &load_cargo_config,
            &|_| {},
        )
        .unwrap()
    };
    let mut host = AnalysisHost::with_database(db);

    let file_id = {
        let file = workspace_to_load.join(file);
        let path = VfsPath::from(AbsPathBuf::assert(file));
        file_id(&vfs, &path)
    };

    let diagnostics_config = DiagnosticsConfig {
        enabled: false,
        proc_macros_enabled: true,
        proc_attr_macros_enabled: true,
        disable_experimental: true,
        disabled: Default::default(),
        expr_fill_default: Default::default(),
        style_lints: false,
        snippet_cap: SnippetCap::new(true),
        insert_use: InsertUseConfig {
            granularity: ImportGranularity::Crate,
            enforce_granularity: false,
            prefix_kind: hir::PrefixKind::ByCrate,
            group: true,
            skip_glob_imports: true,
        },
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        term_search_fuel: 400,
        term_search_borrowck: true,
        show_rename_conflicts: true,
    };
    host.analysis()
        .full_diagnostics(&diagnostics_config, ide::AssistResolveStrategy::None, file_id)
        .unwrap();

    let _g = crate::tracing::hprof::init("*");

    {
        let _it = stdx::timeit("change");
        let mut text = host.analysis().file_text(file_id).unwrap().to_string();
        patch(&mut text, "db.struct_signature(self.id)", "();\ndb.struct_signature(self.id)");
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file_id, Some(text));
        host.apply_change(change);
    };

    {
        let _p = tracing::info_span!("diagnostics").entered();
        let _span = profile::cpu_span();
        host.analysis()
            .full_diagnostics(&diagnostics_config, ide::AssistResolveStrategy::None, file_id)
            .unwrap();
    }
}

/// Harness for profiling `slint!` (or any function-like proc-macro) expansion in isolation.
///
/// Loads a real cargo project, locates the proc-macro call in a file, and times its
/// expansion both cold (server spin-up + first expand) and warm (re-expand after editing
/// the macro body, which is what we actually want to profile).
///
/// Run with, e.g.:
/// ```bash
/// SLINT_TESTCASE=$HOME/Code/slint-ra-testcases/crates/size_xlarge \
///   RUN_SLOW_BENCHES=1 cargo test --release -p rust-analyzer \
///   integrated_slint_macro_expansion_benchmark -- --nocapture --exact
/// ```
/// Defaults to `~/Code/slint-ra-testcases/crates/size_xlarge` and `src/lib.rs` if the
/// `SLINT_TESTCASE` / `SLINT_TESTCASE_FILE` env vars are unset.
#[test]
fn integrated_slint_macro_expansion_benchmark() {
    if std::env::var("RUN_SLOW_BENCHES").is_err() {
        return;
    }

    let project = std::env::var("SLINT_TESTCASE").unwrap_or_else(|_| {
        format!("{}/Code/slint-ra-testcases/crates/size_xlarge", std::env::var("HOME").unwrap())
    });
    let rel_file = std::env::var("SLINT_TESTCASE_FILE").unwrap_or_else(|_| "src/lib.rs".to_owned());
    let workspace_to_load = AbsPathBuf::assert_utf8(std::path::PathBuf::from(project));

    let cargo_config = CargoConfig {
        sysroot: Some(project_model::RustLibSource::Discover),
        all_targets: true,
        set_test: true,
        ..CargoConfig::default()
    };
    let load_cargo_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };

    let (db, vfs, _proc_macro) = {
        let _it = stdx::timeit("workspace loading");
        load_workspace_at(workspace_to_load.as_ref(), &cargo_config, &load_cargo_config, &|_| {})
            .unwrap()
    };
    let mut host = AnalysisHost::with_database(db);

    let file_id = {
        let path = VfsPath::from(workspace_to_load.join(&rel_file));
        file_id(&vfs, &path)
    };

    // Cold expansion: includes proc-macro server spin-up and the very first expand of the
    // macro. This is the cost paid once when a file is first opened.
    {
        let _it = stdx::timeit("cold expand");
        let count = expand_proc_macros_in(host.raw_database(), file_id);
        eprintln!("expanded {count} proc-macro call(s)");
        assert!(count > 0, "no proc-macro calls found in {rel_file}; is the testcase built?");
    }

    // Invalidate the macro: edit a byte inside the macro body so salsa must re-expand it.
    // This is the steady-state cost we care about (typing inside / near the macro).
    {
        let _it = stdx::timeit("apply edit");
        let mut text = host.analysis().file_text(file_id).unwrap().to_string();
        // Insert a harmless whitespace token inside the first `{` of the macro invocation.
        let brace = text.find('{').expect("no `{` in file");
        text.insert(brace + 1, ' ');
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file_id, Some(text));
        host.apply_change(change);
    }

    let _g = crate::tracing::hprof::init("*");

    // Warm expansion: the profiled run. The proc-macro server is already up, so this
    // isolates per-expansion cost (serialize -> IPC + slint codegen -> deserialize -> reparse).
    {
        let _p = tracing::info_span!("slint macro expansion").entered();
        let _it = stdx::timeit("warm expand");
        let _span = profile::cpu_span();
        let count = expand_proc_macros_in(host.raw_database(), file_id);
        eprintln!("re-expanded {count} proc-macro call(s)");
    }
}

/// Forces expansion of every function-like proc-macro call in `file_id` and returns how
/// many were expanded. Walking the expansion drives `expand_proc_macro` +
/// `token_tree_to_syntax_node`, which carry the profiling spans.
fn expand_proc_macros_in(db: &RootDatabase, file_id: vfs::FileId) -> usize {
    use syntax::{AstNode, ast};

    let sema = Semantics::new(db);
    let source_file = sema.parse_guess_edition(file_id);
    let mut count = 0;
    for macro_call in source_file.syntax().descendants().filter_map(ast::MacroCall::cast) {
        if let Some(expanded) = sema.expand_macro_call(&macro_call) {
            // Touch the whole expansion so lazy work isn't skipped.
            let _ = expanded.value.descendants().count();
            count += 1;
        }
    }
    count
}

fn patch(what: &mut String, from: &str, to: &str) -> usize {
    let idx = what.find(from).unwrap();
    *what = what.replacen(from, to, 1);
    idx
}

fn completion_config() -> CompletionConfig<'static> {
    CompletionConfig {
        enable_postfix_completions: true,
        enable_imports_on_the_fly: true,
        enable_self_on_the_fly: true,
        enable_private_editable: true,
        enable_term_search: true,
        term_search_fuel: 200,
        full_function_signatures: false,
        callable: Some(CallableSnippets::FillArguments),
        snippet_cap: SnippetCap::new(true),
        insert_use: InsertUseConfig {
            granularity: ImportGranularity::Crate,
            prefix_kind: hir::PrefixKind::ByCrate,
            enforce_granularity: true,
            group: true,
            skip_glob_imports: true,
        },
        prefer_no_std: false,
        prefer_prelude: true,
        prefer_absolute: false,
        snippets: Vec::new(),
        limit: None,
        add_colons_to_module: true,
        add_semicolon_to_unit: true,
        fields_to_resolve: CompletionFieldsToResolve::empty(),
        exclude_flyimport: vec![],
        exclude_traits: &[],
        enable_auto_await: true,
        enable_auto_iter: true,
        ra_fixture: RaFixtureConfig::default(),
    }
}
