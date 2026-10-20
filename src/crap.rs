//! Rust-native CRAP/complexity gate.
//!
//! This is the Rust port of the original `scripts/check_crap.py` gate. It
//! never invokes Python: Rust sources are parsed with `syn`, changed lines come
//! from `git diff`, and line coverage comes from an llvm-cov JSON export (or
//! from `cargo llvm-cov`, which runs the test suite).
//!
//! # Semantics
//!
//! ```text
//! CRAP(f) = CC(f)^2 * (1 - coverage(f))^3 + CC(f)
//! ```
//!
//! Cyclomatic complexity starts at one and adds one for every `if` (including
//! `else if`, but not a bare `else`), every non-default `match` arm, `for`,
//! `while`, `loop`, `?` (`ExprTry`), `&&`, and `||`. A `try { .. }` block is
//! counted once as the catch-like construct. `unsafe { .. }` is not a branch
//! and is therefore not counted; Rust has no `catch` statement. Complexity is
//! measured inside a function body only and deliberately excludes nested
//! functions and closures (those nested functions are measured separately).
//! Branches hidden inside macro invocations are not visible to `syn` and are
//! not counted.
//!
//! Function coverage is `covered executable lines / executable lines inside the
//! function's line span`. A line is executable when an llvm-cov segment on it
//! has `has_count`; it is covered when any such segment has `count > 0`. If
//! there is no line data for a function, its coverage is reported as zero and
//! the gate prints a warning saying so. Unknown coverage is never silently
//! presented as measured zero.
//!
//! # Source spans
//!
//! Function identity, names and complexity come from `syn`'s AST
//! (`ItemFn`, `ImplItemFn`, and `TraitItemFn` with a default body), including
//! nested functions. `proc-macro2`'s `span-locations` feature is not enabled in
//! this crate (and the dependency manifest must not be changed), so stable
//! `Span::start()/end()` are unavailable. To recover line spans we tokenize the
//! source with a small lexer that is aware of comments, strings, raw strings,
//! character literals, attributes and macro token trees, then map the `fn`
//! declarations to the syn functions in source order. A function span starts at
//! its `fn` keyword and ends at its body's closing brace (or at the `;` for a
//! body-less trait declaration, which is only used for mapping and never
//! reported).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use syn::visit::Visit;
use syn::BinOp;

const DEFAULT_BASE: &str = "origin/main";
const DEFAULT_THRESHOLD: f64 = 10.0;
const DEFAULT_BASELINE: &str = "crap-baseline.json";
const MAX_VIOLATIONS: usize = 50;
const ERROR_DETAIL_LIMIT: usize = 1_200;

// ---------------------------------------------------------------------------
// Public entry point and options
// ---------------------------------------------------------------------------

/// Run the CRAP gate. Returns `0` for success, `1` for CRAP violations and `2`
/// for tool/configuration errors.
pub fn cli(args: &[String]) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("CRAP gate: {message}");
            2
        }
    }
}

#[derive(Debug, Clone)]
struct Options {
    root: PathBuf,
    threshold: f64,
    all: bool,
    base: String,
    skip_tests: bool,
    always_test: bool,
    coverage: Option<PathBuf>,
    baseline: Option<PathBuf>,
    update_baseline: bool,
    no_baseline: bool,
    json: bool,
}

fn usage() -> &'static str {
    "Usage: herdr-flow crap [options]\n\
     \n\
     Options:\n\
       --root DIR          crate root (default: current directory)\n\
       --threshold FLOAT   maximum CRAP for new/worsened functions (default: 10)\n\
       --all               check every function instead of only changed functions\n\
       --base REV          git revision used to select changes (default: origin/main)\n\
       --skip-tests        do not run tests; coverage is unmeasured (reported as 0%)\n\
       --always-test       run cargo test when no production function changed (CI)\n\
       --coverage FILE     llvm-cov JSON export to use instead of running coverage\n\
       --baseline FILE     incremental baseline (default: ROOT/crap-baseline.json)\n\
       --update-baseline   write current measurements to the baseline and exit\n\
       --no-baseline       ignore the baseline; enforce the absolute threshold\n\
       --json              print machine-readable violation details\n\
       -h, --help          show this help"
}

fn print_usage() {
    println!("{}", usage());
}

/// Parse CLI options. `Ok(None)` means help was requested.
fn parse_options(args: &[String]) -> Result<Option<Options>, String> {
    let root = std::env::current_dir()
        .map_err(|error| format!("cannot determine current directory: {error}"))?;
    let mut state = ParseState::new(root);
    if parse_arguments(args, &mut state)? {
        return Ok(None);
    }
    Ok(Some(state.finish()?))
}

/// Boolean flags accepted by the gate. The declaration order defines the
/// indexes into `ParseState::flags`; `Help` must stay last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    All,
    SkipTests,
    AlwaysTest,
    UpdateBaseline,
    NoBaseline,
    Json,
    Help,
}

const HELP_FLAG: usize = Flag::Help as usize;
const FLAG_COUNT: usize = 7;

const FLAG_NAMES: [(&str, Flag); 6] = [
    ("--all", Flag::All),
    ("--skip-tests", Flag::SkipTests),
    ("--always-test", Flag::AlwaysTest),
    ("--update-baseline", Flag::UpdateBaseline),
    ("--no-baseline", Flag::NoBaseline),
    ("--json", Flag::Json),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Continue,
    Help,
}

struct ParseState {
    root: PathBuf,
    threshold: f64,
    base: String,
    coverage: Option<PathBuf>,
    baseline: Option<PathBuf>,
    flags: [bool; FLAG_COUNT],
}

impl ParseState {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            threshold: DEFAULT_THRESHOLD,
            base: DEFAULT_BASE.to_string(),
            coverage: None,
            baseline: None,
            flags: [false; FLAG_COUNT],
        }
    }

    fn record_flag(&mut self, index: usize) -> Control {
        self.flags[index] = true;
        if index == HELP_FLAG {
            Control::Help
        } else {
            Control::Continue
        }
    }

    fn finish(self) -> Result<Options, String> {
        if !self.threshold.is_finite() {
            return Err("--threshold must be a finite number".to_string());
        }
        if self.flags[Flag::UpdateBaseline as usize] && self.flags[Flag::NoBaseline as usize] {
            return Err("--update-baseline and --no-baseline are mutually exclusive".to_string());
        }
        Ok(Options {
            root: self.root,
            threshold: self.threshold,
            all: self.flags[Flag::All as usize],
            base: self.base,
            skip_tests: self.flags[Flag::SkipTests as usize],
            always_test: self.flags[Flag::AlwaysTest as usize],
            coverage: self.coverage,
            baseline: self.baseline,
            update_baseline: self.flags[Flag::UpdateBaseline as usize],
            no_baseline: self.flags[Flag::NoBaseline as usize],
            json: self.flags[Flag::Json as usize],
        })
    }
}

fn parse_arguments(args: &[String], state: &mut ParseState) -> Result<bool, String> {
    let mut index = 0usize;
    while index < args.len() {
        let control = apply_argument(args, &mut index, state)?;
        if control == Control::Help {
            print_usage();
            return Ok(true);
        }
        index += 1;
    }
    Ok(false)
}

fn apply_argument(
    args: &[String],
    index: &mut usize,
    state: &mut ParseState,
) -> Result<Control, String> {
    let argument = &args[*index];
    let (name, inline_value) = split_option(argument);
    if let Some(result) = apply_known_argument(args, index, name, inline_value, state) {
        return result;
    }
    Err(format!("unrecognized argument: {name}\n\n{}", usage()))
}

/// Split `--name=value` into its name and inline value.
fn split_option(argument: &str) -> (&str, Option<&str>) {
    match argument.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (argument, None),
    }
}

fn apply_known_argument(
    args: &[String],
    index: &mut usize,
    name: &str,
    inline_value: Option<&str>,
    state: &mut ParseState,
) -> Option<Result<Control, String>> {
    apply_flag(name, inline_value, state)
        .or_else(|| apply_value_option(args, index, name, inline_value, state))
}

fn apply_flag(
    name: &str,
    inline_value: Option<&str>,
    state: &mut ParseState,
) -> Option<Result<Control, String>> {
    if inline_value.is_some() {
        return None;
    }
    flag_index(name).map(|index| Ok(state.record_flag(index)))
}

fn flag_index(name: &str) -> Option<usize> {
    if matches!(name, "-h" | "--help") {
        return Some(HELP_FLAG);
    }
    FLAG_NAMES
        .iter()
        .position(|(candidate, _)| *candidate == name)
}

type ValueSetter = fn(&mut ParseState, String) -> Result<(), String>;

const VALUE_OPTIONS: [(&str, ValueSetter); 5] = [
    ("--root", set_root),
    ("--threshold", set_threshold),
    ("--base", set_base),
    ("--coverage", set_coverage),
    ("--baseline", set_baseline),
];

fn apply_value_option(
    args: &[String],
    index: &mut usize,
    name: &str,
    inline_value: Option<&str>,
    state: &mut ParseState,
) -> Option<Result<Control, String>> {
    let setter = value_setter(name)?;
    Some(
        option_value(args, index, inline_value, name)
            .and_then(|value| setter(state, value))
            .map(|()| Control::Continue),
    )
}

fn value_setter(name: &str) -> Option<ValueSetter> {
    VALUE_OPTIONS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, setter)| *setter)
}

fn set_root(state: &mut ParseState, value: String) -> Result<(), String> {
    state.root = PathBuf::from(value);
    Ok(())
}

fn set_threshold(state: &mut ParseState, value: String) -> Result<(), String> {
    state.threshold = value
        .parse::<f64>()
        .map_err(|_| format!("invalid --threshold value {value:?}; expected a number"))?;
    Ok(())
}

fn set_base(state: &mut ParseState, value: String) -> Result<(), String> {
    state.base = value;
    Ok(())
}

fn set_coverage(state: &mut ParseState, value: String) -> Result<(), String> {
    state.coverage = Some(PathBuf::from(value));
    Ok(())
}

fn set_baseline(state: &mut ParseState, value: String) -> Result<(), String> {
    state.baseline = Some(PathBuf::from(value));
    Ok(())
}

fn option_value(
    args: &[String],
    index: &mut usize,
    inline_value: Option<&str>,
    name: &str,
) -> Result<String, String> {
    if let Some(value) = inline_value {
        return Ok(value.to_string());
    }
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{name} requires a value"))
}

// ---------------------------------------------------------------------------
// Main flow
// ---------------------------------------------------------------------------

struct Context {
    options: Options,
    analyzed: Vec<Function>,
    baseline: Option<Baseline>,
}

fn run(args: &[String]) -> Result<i32, String> {
    let Some(context) = prepare_context(args)? else {
        return Ok(0);
    };
    execute(context)
}

fn prepare_context(args: &[String]) -> Result<Option<Context>, String> {
    let Some(mut options) = parse_options(args)? else {
        return Ok(None);
    };
    prepare_root(&mut options)?;
    let mut analyzed = analyze_workspace(&options.root)?;
    assign_keys(&mut analyzed);
    let baseline = load_baseline(&options)?;
    Ok(Some(Context {
        options,
        analyzed,
        baseline,
    }))
}

fn prepare_root(options: &mut Options) -> Result<(), String> {
    options.root = fs::canonicalize(&options.root)
        .map_err(|error| format!("cannot resolve --root {}: {error}", options.root.display()))?;
    if options.root.is_dir() {
        return Ok(());
    }
    Err(format!(
        "--root {} is not a directory",
        options.root.display()
    ))
}

fn analyze_workspace(root: &Path) -> Result<Vec<Function>, String> {
    let mut analyzed = Vec::new();
    for file in source_files(root)? {
        analyzed.extend(analyze_file(&file, root)?);
    }
    Ok(analyzed)
}

fn load_baseline(options: &Options) -> Result<Option<Baseline>, String> {
    if full_regeneration(options) {
        return Ok(None);
    }
    explicit_or_implicit_baseline(options)
        .map(|path| Baseline::load(&path))
        .transpose()
}

fn full_regeneration(options: &Options) -> bool {
    options.update_baseline && options.all
}

fn explicit_or_implicit_baseline(options: &Options) -> Option<PathBuf> {
    if options.no_baseline {
        return None;
    }
    if let Some(path) = &options.baseline {
        return Some(path.clone());
    }
    let implicit = options.root.join(DEFAULT_BASELINE);
    implicit.exists().then_some(implicit)
}

fn execute(context: Context) -> Result<i32, String> {
    let Context {
        options,
        analyzed,
        baseline,
    } = context;
    let (base, mode, selected) = select_functions(&options, analyzed)?;
    finish_execution(
        &options,
        base.as_deref(),
        &mode,
        &selected,
        baseline.as_ref(),
    )
}

fn finish_execution(
    options: &Options,
    base: Option<&str>,
    mode: &str,
    selected: &[Function],
    baseline: Option<&Baseline>,
) -> Result<i32, String> {
    if let Some(early) = empty_selection_result(options, base, selected)? {
        return Ok(early);
    }
    let coverage = acquire_coverage(options)?;
    let measurement = measure(options, selected, &coverage);
    if options.update_baseline {
        return update_baseline_output(options, &measurement, coverage.source);
    }
    Ok(report(options, mode, &measurement, &coverage, baseline))
}

fn select_functions(
    options: &Options,
    analyzed: Vec<Function>,
) -> Result<(Option<String>, String, Vec<Function>), String> {
    if options.all {
        return Ok((None, "all".to_string(), analyzed));
    }
    select_changed_functions(options, analyzed)
}

fn select_changed_functions(
    options: &Options,
    analyzed: Vec<Function>,
) -> Result<(Option<String>, String, Vec<Function>), String> {
    require_base(options).and_then(|base| filter_by_changes(options, analyzed, base))
}

fn require_base(options: &Options) -> Result<String, String> {
    resolve_base(&options.root, &options.base)?
        .ok_or_else(|| "cannot resolve a base revision; use --all or --base".to_string())
}

fn filter_by_changes(
    options: &Options,
    analyzed: Vec<Function>,
    base: String,
) -> Result<(Option<String>, String, Vec<Function>), String> {
    let changes = changed_ranges(&options.root, &base)?;
    let selected = analyzed
        .into_iter()
        .filter(|function| function_changed(function, &changes))
        .collect();
    Ok((Some(base.clone()), format!("changed vs {base}"), selected))
}

fn function_changed(function: &Function, changes: &BTreeMap<PathBuf, Vec<LineRange>>) -> bool {
    changes
        .get(&function.file)
        .map(|ranges| intersects(function.start, function.end, ranges))
        .unwrap_or(false)
}

fn update_baseline_output(
    options: &Options,
    measurement: &Measurement,
    source: CoverageSource,
) -> Result<i32, String> {
    let target = options
        .baseline
        .clone()
        .unwrap_or_else(|| options.root.join(DEFAULT_BASELINE));
    let existing = existing_baseline_entries(options, &target)?;
    let count = write_baseline(&target, existing, measurement, options.threshold, source)?;
    println!(
        "CRAP gate: baseline updated {} ({} entries) from {} function(s); coverage={}",
        target.display(),
        count,
        measurement.rows.len(),
        source.label()
    );
    Ok(0)
}

fn existing_baseline_entries(
    options: &Options,
    target: &Path,
) -> Result<BTreeMap<String, Value>, String> {
    if options.all {
        return Ok(BTreeMap::new());
    }
    load_existing_entries(target)
}

fn load_existing_entries(target: &Path) -> Result<BTreeMap<String, Value>, String> {
    if target.exists() {
        return Baseline::load(target).map(|baseline| baseline.functions);
    }
    Ok(BTreeMap::new())
}

fn empty_selection_result(
    options: &Options,
    base: Option<&str>,
    selected: &[Function],
) -> Result<Option<i32>, String> {
    if !selected.is_empty() {
        return Ok(None);
    }
    handle_empty_selection(options, base)
}

fn handle_empty_selection(options: &Options, base: Option<&str>) -> Result<Option<i32>, String> {
    if options.all {
        return Err("no functions found under src".to_string());
    }
    empty_changed_selection(options, base)
}

fn empty_changed_selection(options: &Options, base: Option<&str>) -> Result<Option<i32>, String> {
    let base = base.unwrap_or(DEFAULT_BASE);
    if options.update_baseline {
        println!("CRAP gate: no changed production functions vs {base}; baseline unchanged");
        return Ok(Some(0));
    }
    no_changed_production_functions(options, base).map(Some)
}

/// Behavior when no production function intersects the diff.
fn no_changed_production_functions(options: &Options, base: &str) -> Result<i32, String> {
    if options.always_test {
        return run_always_test(options, base);
    }
    println!("CRAP gate: no changed production functions vs {base}");
    Ok(0)
}

fn run_always_test(options: &Options, base: &str) -> Result<i32, String> {
    if options.skip_tests {
        println!("CRAP gate: no changed production functions vs {base}; tests skipped");
        return Ok(0);
    }
    run_tests_for_no_changes(options, base)
}

fn run_tests_for_no_changes(options: &Options, base: &str) -> Result<i32, String> {
    let output = run_command(&options.root, "cargo", &["test", "--locked"])?;
    Ok(tests_outcome(base, &output))
}

fn tests_outcome(base: &str, output: &CommandOutput) -> i32 {
    if output.status == 0 {
        println!("{}", tests_passed_message(base, &output.stdout));
        return 0;
    }
    eprintln!(
        "CRAP gate: test suite failed (exit {}); fix tests before evaluating CRAP",
        output.status
    );
    print_error_detail(output);
    2
}

fn tests_passed_message(base: &str, stdout: &[u8]) -> String {
    let Some(summary) = test_summary(stdout) else {
        return format!("CRAP gate: no changed production functions vs {base}; tests passed");
    };
    format!("CRAP gate: no changed production functions vs {base}; tests passed ({summary})")
}

// ---------------------------------------------------------------------------
// Source files and syn analysis
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Function {
    file: PathBuf,
    display: String,
    name: String,
    start: usize,
    end: usize,
    cc: usize,
    /// Stable ordinal-qualified key, for example `src/ci/checks.rs::validate#1`.
    key: String,
}

#[derive(Debug, Clone)]
struct AstFunction {
    name: String,
    cc: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceFunction {
    name: String,
    start: usize,
    end: usize,
}

fn source_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let src = root.join("src");
    let metadata = fs::metadata(&src)
        .map_err(|error| format!("cannot read source directory {}: {error}", src.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", src.display()));
    }
    let mut files = Vec::new();
    walk_rust_files(&src, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_rust_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if file_type.is_dir() {
            walk_rust_files(&path, files)?;
        } else if file_type.is_file()
            && path
                .extension()
                .map(|extension| extension == "rs")
                .unwrap_or(false)
        {
            files.push(path);
        }
    }
    Ok(())
}

fn analyze_file(path: &Path, root: &Path) -> Result<Vec<Function>, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let display = display_path(path, root);
    analyze_source(path, &display, &source)
}

fn analyze_source(path: &Path, display: &str, source: &str) -> Result<Vec<Function>, String> {
    let parsed =
        syn::parse_file(source).map_err(|error| format!("cannot parse {display}: {error}"))?;

    let mut collector = FunctionCollector::default();
    collector.visit_file(&parsed);
    let mut spans = SpanMatcher::new(scan_source_functions(source));
    Ok(collector
        .functions
        .into_iter()
        .map(|ast_function| analyze_function(path, display, ast_function, &mut spans))
        .collect())
}

/// Maps AST functions, in declaration order, onto the spans recovered from the
/// source text. A missing span is skipped by searching forward.
struct SpanMatcher {
    spans: Vec<SourceFunction>,
    cursor: usize,
}

impl SpanMatcher {
    fn new(spans: Vec<SourceFunction>) -> Self {
        Self { spans, cursor: 0 }
    }

    fn next(&mut self, name: &str) -> Option<SourceFunction> {
        let current = self
            .spans
            .get(self.cursor)
            .map(|span| span.name == name)
            .unwrap_or(false);
        if current {
            let span = self.spans[self.cursor].clone();
            self.cursor += 1;
            return Some(span);
        }
        self.find_later(name)
    }

    fn find_later(&mut self, name: &str) -> Option<SourceFunction> {
        let found = self
            .spans
            .iter()
            .enumerate()
            .skip(self.cursor)
            .find(|(_, span)| span.name == name)
            .map(|(index, _)| index)?;
        self.cursor = found + 1;
        Some(self.spans[found].clone())
    }
}

fn analyze_function(
    path: &Path,
    display: &str,
    ast_function: AstFunction,
    spans: &mut SpanMatcher,
) -> Function {
    let (start, end) = spans
        .next(&ast_function.name)
        .map(|span| (span.start, span.end))
        .unwrap_or_else(|| {
            eprintln!(
                "CRAP gate: warning: cannot locate source span for function {} in {}; using line 1",
                ast_function.name, display
            );
            (1, 1)
        });
    Function {
        file: path.to_path_buf(),
        display: display.to_string(),
        name: ast_function.name,
        start,
        end,
        cc: ast_function.cc,
        key: String::new(),
    }
}

/// Assign stable `file::name#ordinal` keys. Ordinals are based on declaration
/// order within a file so overloaded methods remain distinguishable.
fn assign_keys(functions: &mut [Function]) {
    let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (index, function) in functions.iter().enumerate() {
        groups
            .entry((function.display.clone(), function.name.clone()))
            .or_default()
            .push(index);
    }
    for ((display, name), mut indexes) in groups {
        indexes.sort_by_key(|index| functions[*index].start);
        for (position, index) in indexes.into_iter().enumerate() {
            functions[index].key = format!("{display}::{name}#{}", position + 1);
        }
    }
}

fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Complexity
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FunctionCollector {
    functions: Vec<AstFunction>,
}

impl FunctionCollector {
    fn record(&mut self, ident: &syn::Ident, block: &syn::Block) {
        let mut complexity = Complexity::new();
        complexity.visit_block(block);
        self.functions.push(AstFunction {
            name: normalize_ident(&ident.to_string()),
            cc: complexity.score,
        });
    }
}

impl<'ast> Visit<'ast> for FunctionCollector {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.record(&node.sig.ident, &node.block);
        syn::visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.record(&node.sig.ident, &node.block);
        syn::visit::visit_impl_item_fn(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if let Some(block) = &node.default {
            self.record(&node.sig.ident, block);
        }
        syn::visit::visit_trait_item_fn(self, node);
    }
}

/// McCabe complexity of one function body. Nested functions and closures are
/// skipped: nested functions are reported separately and closures are not
/// functions in this gate.
struct Complexity {
    score: usize,
}

impl Complexity {
    fn new() -> Self {
        Self { score: 1 }
    }
}

impl<'ast> Visit<'ast> for Complexity {
    fn visit_item_fn(&mut self, _node: &'ast syn::ItemFn) {}

    fn visit_impl_item_fn(&mut self, _node: &'ast syn::ImplItemFn) {}

    fn visit_trait_item_fn(&mut self, _node: &'ast syn::TraitItemFn) {}

    fn visit_expr_closure(&mut self, _node: &'ast syn::ExprClosure) {}

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.score += 1;
        syn::visit::visit_expr_if(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.score += 1;
        syn::visit::visit_expr_for_loop(self, node);
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.score += 1;
        syn::visit::visit_expr_while(self, node);
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.score += 1;
        syn::visit::visit_expr_loop(self, node);
    }

    fn visit_expr_try(&mut self, node: &'ast syn::ExprTry) {
        self.score += 1;
        syn::visit::visit_expr_try(self, node);
    }

    fn visit_expr_try_block(&mut self, node: &'ast syn::ExprTryBlock) {
        self.score += 1;
        syn::visit::visit_expr_try_block(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if matches!(&node.op, BinOp::And(_) | BinOp::Or(_)) {
            self.score += 1;
        }
        syn::visit::visit_expr_binary(self, node);
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        for arm in &node.arms {
            let default_arm = arm.guard.is_none() && matches!(&arm.pat, syn::Pat::Wild(_));
            if !default_arm {
                self.score += 1;
            }
        }
        syn::visit::visit_expr_match(self, node);
    }
}

fn normalize_ident(name: &str) -> String {
    name.strip_prefix("r#").unwrap_or(name).to_string()
}

// ---------------------------------------------------------------------------
// Lightweight source lexer and function-span recovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Ident,
    Punct,
}

#[derive(Debug, Clone)]
struct Token {
    text: String,
    line: usize,
    kind: TokenKind,
}

fn tokenize(source: &str) -> Vec<Token> {
    Lexer::new(source).run()
}

/// Minimal lexer that recovers line spans. It intentionally keeps only the
/// punctuation the span scanner needs and skips everything inside comments and
/// literals.
struct Lexer {
    chars: Vec<char>,
    index: usize,
    line: usize,
    tokens: Vec<Token>,
}

impl Lexer {
    fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            index: 0,
            line: 1,
            tokens: Vec::new(),
        }
    }

    fn run(mut self) -> Vec<Token> {
        while self.index < self.chars.len() {
            self.step();
        }
        self.tokens
    }

    fn step(&mut self) {
        let character = self.chars[self.index];
        let _ = self
            .step_non_code(character)
            .or_else(|| self.step_char_or_lifetime(character))
            .or_else(|| self.step_identifier(character))
            .or_else(|| self.step_number(character))
            .or_else(|| self.step_punctuation(character));
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.index + offset).copied()
    }

    fn step_non_code(&mut self, character: char) -> Option<()> {
        if character == '\n' {
            self.line += 1;
            self.index += 1;
            return Some(());
        }
        if character.is_whitespace() {
            self.index += 1;
            return Some(());
        }
        self.skip_comment(character)
            .or_else(|| self.skip_string_literal())
    }

    fn skip_comment(&mut self, character: char) -> Option<()> {
        if character != '/' {
            return None;
        }
        if self.peek(1) == Some('/') {
            self.skip_line_comment();
            return Some(());
        }
        if self.peek(1) == Some('*') {
            self.skip_block_comment();
            return Some(());
        }
        None
    }

    fn skip_line_comment(&mut self) {
        self.index += 2;
        let rest = &self.chars[self.index..];
        let skip = rest.iter().position(|character| *character == '\n');
        self.index += skip.unwrap_or(rest.len());
    }

    fn skip_block_comment(&mut self) {
        let end = block_comment_end(&self.chars, self.index);
        self.line += newline_count(&self.chars[self.index..end]);
        self.index = end;
    }

    fn skip_string_literal(&mut self) -> Option<()> {
        let end = string_literal_end(&self.chars, self.index)?;
        self.line += newline_count(&self.chars[self.index..end]);
        self.index = end;
        Some(())
    }

    fn step_char_or_lifetime(&mut self, character: char) -> Option<()> {
        if character != '\'' {
            return None;
        }
        self.consume_char_literal();
        Some(())
    }

    fn consume_char_literal(&mut self) {
        let Some(end) = char_literal_end(&self.chars, self.index) else {
            self.index += 1;
            return;
        };
        self.line += newline_count(&self.chars[self.index..end]);
        self.index = end;
    }

    fn step_identifier(&mut self, character: char) -> Option<()> {
        if !is_ident_start(character) {
            return None;
        }
        let end = ident_end(&self.chars, self.index);
        let text: String = self.chars[self.index..end].iter().collect();
        self.tokens.push(Token {
            text,
            line: self.line,
            kind: TokenKind::Ident,
        });
        self.index = end;
        Some(())
    }

    fn step_number(&mut self, character: char) -> Option<()> {
        if !character.is_ascii_digit() {
            return None;
        }
        self.index = number_end(&self.chars, self.index);
        Some(())
    }

    fn step_punctuation(&mut self, character: char) -> Option<()> {
        if is_span_punctuation(character) {
            self.tokens.push(Token {
                text: character.to_string(),
                line: self.line,
                kind: TokenKind::Punct,
            });
            self.index += 1;
            return Some(());
        }
        self.index += punctuation_width(&self.chars, self.index);
        Some(())
    }
}

fn newline_count(chars: &[char]) -> usize {
    chars.iter().filter(|character| **character == '\n').count()
}

fn number_end(chars: &[char], start: usize) -> usize {
    let mut index = start + 1;
    while index < chars.len() && (chars[index].is_ascii_alphanumeric() || chars[index] == '_') {
        index += 1;
    }
    index
}

fn is_span_punctuation(character: char) -> bool {
    matches!(
        character,
        '{' | '}' | '(' | ')' | '[' | ']' | '<' | '>' | ';' | '!' | '#'
    )
}

fn punctuation_width(chars: &[char], index: usize) -> usize {
    let next = chars.get(index + 1).copied();
    let arrow = matches!((chars[index], next), ('-' | '=', Some('>')));
    if arrow {
        2
    } else {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentMarker {
    Open,
    Close,
    None,
}

fn block_comment_end(chars: &[char], start: usize) -> usize {
    let mut depth = 0usize;
    let mut index = start;
    while index < chars.len() {
        match comment_marker(chars, index) {
            CommentMarker::Open => {
                depth += 1;
                index += 2;
            }
            CommentMarker::Close => {
                depth = depth.saturating_sub(1);
                index += 2;
                if depth == 0 {
                    return index;
                }
            }
            CommentMarker::None => index += 1,
        }
    }
    chars.len()
}

fn comment_marker(chars: &[char], index: usize) -> CommentMarker {
    if chars[index..].starts_with(&['/', '*']) {
        return CommentMarker::Open;
    }
    if chars[index..].starts_with(&['*', '/']) {
        return CommentMarker::Close;
    }
    CommentMarker::None
}

fn string_literal_end(chars: &[char], start: usize) -> Option<usize> {
    let character = *chars.get(start)?;
    if character == '"' {
        return Some(escaped_string_end(chars, start + 1));
    }
    prefixed_string_end(chars, start, character)
}

fn prefixed_string_end(chars: &[char], start: usize, character: char) -> Option<usize> {
    if character == 'r' {
        return raw_prefixed_string_end(chars, start + 1);
    }
    if character == 'b' || character == 'c' {
        return c_prefixed_literal_end(chars, start);
    }
    None
}

fn raw_prefixed_string_end(chars: &[char], mut index: usize) -> Option<usize> {
    let mut hashes = 0usize;
    while chars.get(index) == Some(&'#') {
        hashes += 1;
        index += 1;
    }
    if chars.get(index) == Some(&'"') {
        return Some(raw_string_end(chars, index + 1, hashes));
    }
    None
}

fn c_prefixed_literal_end(chars: &[char], start: usize) -> Option<usize> {
    if chars.get(start + 1) == Some(&'"') {
        return Some(escaped_string_end(chars, start + 2));
    }
    if chars.get(start + 1) == Some(&'r') {
        return raw_prefixed_string_end(chars, start + 2);
    }
    if chars.get(start) == Some(&'b') && chars.get(start + 1) == Some(&'\'') {
        return char_literal_end(chars, start + 1);
    }
    None
}

fn escaped_string_end(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() {
        if chars[index] == '\\' {
            index += 2;
            continue;
        }
        if chars[index] == '"' {
            return index + 1;
        }
        index += 1;
    }
    chars.len()
}

fn raw_string_end(chars: &[char], start: usize, hashes: usize) -> usize {
    let mut index = start;
    while index < chars.len() {
        if chars[index] == '"' {
            if let Some(end) = raw_string_close(chars, index + 1, hashes) {
                return end;
            }
        }
        index += 1;
    }
    chars.len()
}

fn raw_string_close(chars: &[char], mut index: usize, hashes: usize) -> Option<usize> {
    for _ in 0..hashes {
        if chars.get(index) != Some(&'#') {
            return None;
        }
        index += 1;
    }
    Some(index)
}

fn char_literal_end(chars: &[char], quote: usize) -> Option<usize> {
    let mut index = quote + 1;
    if index >= chars.len() {
        return None;
    }
    index = skip_char_escape(chars, index);
    if chars.get(index) == Some(&'\'') {
        Some(index + 1)
    } else {
        None
    }
}

fn skip_char_escape(chars: &[char], index: usize) -> usize {
    if chars.get(index) != Some(&'\\') {
        return index + 1;
    }
    let escaped = index + 1;
    let Some(character) = chars.get(escaped).copied() else {
        return escaped;
    };
    if character == 'x' {
        return skip_hex_escape(chars, escaped + 1);
    }
    if character == 'u' {
        return skip_unicode_escape(chars, escaped + 1);
    }
    escaped + 1
}

fn skip_hex_escape(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() && chars[index].is_ascii_hexdigit() {
        index += 1;
    }
    index
}

fn skip_unicode_escape(chars: &[char], mut index: usize) -> usize {
    if chars.get(index) != Some(&'{') {
        return index;
    }
    index += 1;
    while index < chars.len() && chars[index] != '}' {
        index += 1;
    }
    if index < chars.len() {
        return index + 1;
    }
    index
}

fn is_ident_start(character: char) -> bool {
    character == '_'
        || character.is_alphabetic()
        || (!character.is_ascii() && !character.is_whitespace())
}

fn is_ident_continue(character: char) -> bool {
    character == '_'
        || character.is_alphanumeric()
        || (!character.is_ascii() && !character.is_whitespace())
}

fn ident_end(chars: &[char], start: usize) -> usize {
    let mut index = start;
    if chars.get(index) == Some(&'r')
        && chars.get(index + 1) == Some(&'#')
        && chars
            .get(index + 2)
            .map(|character| is_ident_start(*character))
            .unwrap_or(false)
    {
        index += 2;
    }
    while index < chars.len() && is_ident_continue(chars[index]) {
        index += 1;
    }
    index
}

/// Remove attributes and macro token trees so `fn` tokens inside them cannot be
/// mistaken for real function declarations. The syn AST does not contain items
/// from macro invocations, so this also keeps AST/source ordering aligned.
fn strip_attributes_and_macros(tokens: &[Token]) -> Vec<Token> {
    let mut result = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        let skip_to = attribute_end(tokens, index)
            .or_else(|| macro_rules_end(tokens, index))
            .or_else(|| macro_invocation_end(tokens, index));
        if let Some(next) = skip_to {
            index = next;
            continue;
        }
        result.push(tokens[index].clone());
        index += 1;
    }
    result
}

fn attribute_end(tokens: &[Token], index: usize) -> Option<usize> {
    if !token_is(tokens, index, "#") {
        return None;
    }
    let group = attribute_group(tokens, index + 1)?;
    Some(skip_delimited(tokens, group))
}

fn attribute_group(tokens: &[Token], mut index: usize) -> Option<usize> {
    if token_is(tokens, index, "!") {
        index += 1;
    }
    token_is(tokens, index, "[").then_some(index)
}

fn macro_rules_end(tokens: &[Token], index: usize) -> Option<usize> {
    if !token_is_ident(tokens, index, "macro_rules") {
        return None;
    }
    let group = macro_rules_group(tokens, index + 1)?;
    Some(skip_delimited(tokens, group))
}

fn macro_rules_group(tokens: &[Token], index: usize) -> Option<usize> {
    if !token_is(tokens, index, "!") {
        return None;
    }
    let mut group = index + 1;
    if token_kind_is(tokens, group, TokenKind::Ident) {
        group += 1;
    }
    is_open_delimiter_at(tokens, group).then_some(group)
}

fn macro_invocation_end(tokens: &[Token], index: usize) -> Option<usize> {
    let group = macro_group(tokens, index)?;
    Some(skip_delimited(tokens, group))
}

fn macro_group(tokens: &[Token], index: usize) -> Option<usize> {
    if !token_kind_is(tokens, index, TokenKind::Ident) || !token_is(tokens, index + 1, "!") {
        return None;
    }
    let group = index + 2;
    is_open_delimiter_at(tokens, group).then_some(group)
}

fn token_text(tokens: &[Token], index: usize) -> Option<&str> {
    tokens.get(index).map(|token| token.text.as_str())
}

fn token_is(tokens: &[Token], index: usize, text: &str) -> bool {
    token_text(tokens, index) == Some(text)
}

fn token_is_ident(tokens: &[Token], index: usize, text: &str) -> bool {
    tokens
        .get(index)
        .map(|token| token.kind == TokenKind::Ident && token.text == text)
        .unwrap_or(false)
}

fn token_kind_is(tokens: &[Token], index: usize, kind: TokenKind) -> bool {
    tokens
        .get(index)
        .map(|token| token.kind == kind)
        .unwrap_or(false)
}

fn is_open_delimiter_at(tokens: &[Token], index: usize) -> bool {
    token_text(tokens, index)
        .map(is_open_delimiter)
        .unwrap_or(false)
}

fn is_open_delimiter(text: &str) -> bool {
    matches!(text, "{" | "(" | "[")
}

fn closer_for(opener: &str) -> &'static str {
    match opener {
        "{" => "}",
        "(" => ")",
        "[" => "]",
        _ => "",
    }
}

/// Return the first token index after the balanced group opened at `open`.
fn skip_delimited(tokens: &[Token], open: usize) -> usize {
    let Some(open_token) = tokens.get(open) else {
        return tokens.len();
    };
    if !is_open_delimiter(&open_token.text) {
        return open + 1;
    }
    let mut stack = vec![closer_for(&open_token.text)];
    let mut index = open + 1;
    while index < tokens.len() {
        let text = tokens[index].text.as_str();
        if is_open_delimiter(text) {
            stack.push(closer_for(text));
        } else if let Some(expected) = stack.last() {
            if text == *expected {
                stack.pop();
                if stack.is_empty() {
                    return index + 1;
                }
            } else if matches!(text, "}" | ")" | "]") {
                // Mismatched close: treat it as unbalanced and stop at the end.
                return index + 1;
            }
        }
        index += 1;
    }
    tokens.len()
}

fn scan_source_functions(source: &str) -> Vec<SourceFunction> {
    let tokens = strip_attributes_and_macros(&tokenize(source));
    let mut functions = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        if tokens[index].kind == TokenKind::Ident && tokens[index].text == "fn" {
            let name_token = tokens.get(index + 1);
            if let Some(name_token) = name_token {
                if name_token.kind == TokenKind::Ident {
                    if let Some(end) = find_function_end(&tokens, index) {
                        functions.push(SourceFunction {
                            name: normalize_ident(&name_token.text),
                            start: tokens[index].line,
                            end,
                        });
                    }
                }
            }
        }
        index += 1;
    }
    functions
}

fn find_function_end(tokens: &[Token], fn_index: usize) -> Option<usize> {
    find_body_end(tokens, fn_index, true).or_else(|| find_body_end(tokens, fn_index, false))
}

fn find_body_end(tokens: &[Token], fn_index: usize, track_angles: bool) -> Option<usize> {
    let mut scan = BodyScan::new(track_angles);
    let mut index = fn_index + 2;
    while index < tokens.len() {
        match scan.step(tokens, index) {
            ScanStep::Skip(next) => index = next,
            ScanStep::Return(result) => return result,
            ScanStep::Continue => index += 1,
        }
    }
    None
}

/// Tracks the nesting state while searching for a function body.
struct BodyScan {
    parens: i32,
    brackets: i32,
    angles: i32,
    track_angles: bool,
}

enum ScanStep {
    Continue,
    Skip(usize),
    Return(Option<usize>),
}

impl BodyScan {
    fn new(track_angles: bool) -> Self {
        Self {
            parens: 0,
            brackets: 0,
            angles: 0,
            track_angles,
        }
    }

    fn step(&mut self, tokens: &[Token], index: usize) -> ScanStep {
        self.step_paren(tokens, index)
            .or_else(|| self.step_bracket(tokens, index))
            .or_else(|| self.step_angle(tokens, index))
            .or_else(|| self.step_brace(tokens, index))
            .or_else(|| self.step_semicolon(tokens, index))
            .unwrap_or(ScanStep::Continue)
    }

    fn at_top_level(&self) -> bool {
        self.parens == 0 && self.brackets == 0 && self.angles <= 0
    }

    fn step_paren(&mut self, tokens: &[Token], index: usize) -> Option<ScanStep> {
        match tokens[index].text.as_str() {
            "(" => self.parens += 1,
            ")" => self.parens = (self.parens - 1).max(0),
            _ => return None,
        }
        Some(ScanStep::Continue)
    }

    fn step_bracket(&mut self, tokens: &[Token], index: usize) -> Option<ScanStep> {
        match tokens[index].text.as_str() {
            "[" => self.brackets += 1,
            "]" => self.brackets = (self.brackets - 1).max(0),
            _ => return None,
        }
        Some(ScanStep::Continue)
    }

    fn step_angle(&mut self, tokens: &[Token], index: usize) -> Option<ScanStep> {
        if !self.track_angles {
            return None;
        }
        match tokens[index].text.as_str() {
            "<" => self.angles += 1,
            ">" => self.angles = (self.angles - 1).max(0),
            _ => return None,
        }
        Some(ScanStep::Continue)
    }

    fn step_brace(&mut self, tokens: &[Token], index: usize) -> Option<ScanStep> {
        if tokens[index].text != "{" {
            return None;
        }
        if self.at_top_level() {
            return Some(ScanStep::Return(Some(end_of_brace_group(tokens, index))));
        }
        Some(ScanStep::Skip(skip_delimited(tokens, index)))
    }

    fn step_semicolon(&mut self, tokens: &[Token], index: usize) -> Option<ScanStep> {
        if tokens[index].text != ";" || !self.at_top_level() {
            return None;
        }
        Some(ScanStep::Return(None))
    }
}

fn end_of_brace_group(tokens: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    let mut index = open;
    while index < tokens.len() {
        match tokens[index].text.as_str() {
            "{" => depth += 1,
            "}" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return tokens[index].line;
                }
            }
            _ => {}
        }
        index += 1;
    }
    tokens.last().map(|token| token.line).unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Git change selection
// ---------------------------------------------------------------------------

type LineRange = (usize, usize);

fn parse_diff(text: &str) -> BTreeMap<String, Vec<LineRange>> {
    let mut changes: BTreeMap<String, Vec<LineRange>> = BTreeMap::new();
    let mut path: Option<String> = None;
    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if let Some(rest) = line.strip_prefix("+++ b/") {
            path = Some(rest.to_string());
        } else if line.starts_with("+++ ") {
            path = None;
        } else if line.starts_with("@@") {
            if let Some(current) = &path {
                if let Some(range) = parse_hunk_header(line) {
                    changes.entry(current.clone()).or_default().push(range);
                }
            }
        }
    }
    changes
}

fn parse_hunk_header(line: &str) -> Option<LineRange> {
    let mut parts = line.strip_prefix("@@ ")?.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    parse_hunk_range(old, new)
}

fn parse_hunk_range(old: &str, new: &str) -> Option<LineRange> {
    let spec = new.strip_prefix('+')?;
    if !old.starts_with('-') {
        return None;
    }
    let (start, count) = split_hunk_numbers(spec)?;
    Some(hunk_range(start, count))
}

fn split_hunk_numbers(spec: &str) -> Option<(usize, usize)> {
    let (start_text, count_text) = spec.split_once(',').unwrap_or((spec, "1"));
    Some((start_text.parse().ok()?, count_text.parse().ok()?))
}

fn hunk_range(start: usize, count: usize) -> LineRange {
    (start, start.saturating_add(count.max(1)).saturating_sub(1))
}

fn intersects(start: usize, end: usize, ranges: &[LineRange]) -> bool {
    ranges
        .iter()
        .any(|(range_start, range_end)| *range_start <= end && start <= *range_end)
}

fn valid_revision(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value
            .chars()
            .any(|character| character.is_whitespace() || (character as u32) < 32)
}

fn resolve_base(root: &Path, requested: &str) -> Result<Option<String>, String> {
    for candidate in base_candidates(requested) {
        if revision_exists(root, &candidate)? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn base_candidates(requested: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    if !requested.is_empty() {
        candidates.push(requested.to_string());
    }
    for fallback in [DEFAULT_BASE, "main", "HEAD~1"] {
        if !candidates.iter().any(|candidate| candidate == fallback) {
            candidates.push(fallback.to_string());
        }
    }
    candidates.retain(|candidate| usable_candidate(candidate));
    candidates
}

fn usable_candidate(candidate: &str) -> bool {
    !candidate.chars().all(|character| character == '0') && valid_revision(candidate)
}

fn revision_exists(root: &Path, candidate: &str) -> Result<bool, String> {
    let output = run_git(root, &["rev-parse", "--verify", "--quiet", candidate])?;
    Ok(output.status == 0)
}

fn changed_ranges(root: &Path, base: &str) -> Result<BTreeMap<PathBuf, Vec<LineRange>>, String> {
    let repo_root = git_repo_root(root)?;
    let mut changes = tracked_changes(root, base, &repo_root)?;
    add_untracked_changes(root, &repo_root, &mut changes)?;
    Ok(changes)
}

fn tracked_changes(
    root: &Path,
    base: &str,
    repo_root: &Path,
) -> Result<BTreeMap<PathBuf, Vec<LineRange>>, String> {
    let diff = run_git(
        root,
        &["diff", "--unified=0", "--no-color", base, "--", "src"],
    )?;
    let text = checked_diff_stdout(&diff, base)?;
    Ok(paths_from_diff(&text, repo_root))
}

fn checked_diff_stdout(output: &CommandOutput, base: &str) -> Result<String, String> {
    if output.status != 0 {
        return Err(format!(
            "git diff against {base} failed (exit {}): {}",
            output.status,
            error_detail(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn paths_from_diff(text: &str, repo_root: &Path) -> BTreeMap<PathBuf, Vec<LineRange>> {
    let mut changes: BTreeMap<PathBuf, Vec<LineRange>> = BTreeMap::new();
    for (name, ranges) in parse_diff(text) {
        let path = canonical_or_lexical(&repo_root.join(&name));
        changes.entry(path).or_default().extend(ranges);
    }
    changes
}

fn add_untracked_changes(
    root: &Path,
    repo_root: &Path,
    changes: &mut BTreeMap<PathBuf, Vec<LineRange>>,
) -> Result<(), String> {
    let untracked = run_git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "--full-name",
            "--",
            "src",
        ],
    )?;
    checked_untracked_status(&untracked)?;
    add_untracked_paths(
        &String::from_utf8_lossy(&untracked.stdout),
        root,
        repo_root,
        changes,
    );
    Ok(())
}

fn checked_untracked_status(output: &CommandOutput) -> Result<(), String> {
    if output.status == 0 {
        return Ok(());
    }
    Err(format!(
        "git ls-files failed (exit {}): {}",
        output.status,
        error_detail(&output.stderr)
    ))
}

fn add_untracked_paths(
    text: &str,
    root: &Path,
    repo_root: &Path,
    changes: &mut BTreeMap<PathBuf, Vec<LineRange>>,
) {
    let src_root = canonical_or_lexical(&root.join("src"));
    for name in text.lines() {
        if !name.ends_with(".rs") {
            continue;
        }
        let path = canonical_or_lexical(&repo_root.join(name));
        if !path.starts_with(&src_root) {
            continue;
        }
        changes.entry(path).or_default().push((1, usize::MAX));
    }
}

fn git_repo_root(root: &Path) -> Result<PathBuf, String> {
    let output = run_git(root, &["rev-parse", "--show-toplevel"])?;
    parse_git_repo_root(&output)
}

fn parse_git_repo_root(output: &CommandOutput) -> Result<PathBuf, String> {
    if output.status != 0 {
        return Err(format!(
            "cannot determine git repository root: {}",
            error_detail(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Err("git rev-parse --show-toplevel returned an empty path".to_string());
    }
    Ok(canonical_or_lexical(Path::new(text)))
}

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageSource {
    Unmeasured,
    External,
    LlvmCov,
}

impl CoverageSource {
    fn label(self) -> &'static str {
        match self {
            CoverageSource::Unmeasured => "unmeasured",
            CoverageSource::External => "external",
            CoverageSource::LlvmCov => "cargo-llvm-cov",
        }
    }
}

#[derive(Debug, Default, Clone)]
struct FileCoverage {
    executable: BTreeSet<usize>,
    covered: BTreeSet<usize>,
}

impl FileCoverage {
    fn has_executable_in_span(&self, start: usize, end: usize) -> bool {
        start <= end && self.executable.range(start..=end).next().is_some()
    }

    fn function_coverage(&self, start: usize, end: usize) -> f64 {
        if start > end {
            return 0.0;
        }
        let executable = self.executable.range(start..=end).count();
        if executable == 0 {
            return 0.0;
        }
        let covered = self
            .covered
            .range(start..=end)
            .filter(|line| self.executable.contains(line))
            .count();
        covered as f64 / executable as f64
    }
}

#[derive(Debug, Clone)]
struct CoverageData {
    source: CoverageSource,
    files: BTreeMap<String, FileCoverage>,
}

impl CoverageData {
    fn unmeasured() -> Self {
        Self {
            source: CoverageSource::Unmeasured,
            files: BTreeMap::new(),
        }
    }

    fn lookup(&self, path: &Path, root: &Path) -> Option<&FileCoverage> {
        let canonical = canonical_or_lexical(path);
        let mut candidates = Vec::new();
        candidates.push(path.to_string_lossy().into_owned());
        candidates.push(canonical.to_string_lossy().into_owned());
        if let Ok(relative) = canonical.strip_prefix(root) {
            candidates.push(relative.to_string_lossy().into_owned());
        }
        for candidate in candidates {
            if let Some(file) = self.files.get(&candidate) {
                return Some(file);
            }
        }
        None
    }
}

fn acquire_coverage(options: &Options) -> Result<CoverageData, String> {
    if let Some(path) = &options.coverage {
        return external_coverage(path, &options.root);
    }
    if options.skip_tests {
        return Ok(CoverageData::unmeasured());
    }
    run_coverage(&options.root)
}

fn external_coverage(path: &Path, root: &Path) -> Result<CoverageData, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read coverage file {}: {error}", path.display()))?;
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        format!(
            "cannot parse llvm-cov JSON from {}: {error}",
            path.display()
        )
    })?;
    let files = parse_coverage_value(&value, root)?;
    Ok(CoverageData {
        source: CoverageSource::External,
        files,
    })
}

fn run_coverage(root: &Path) -> Result<CoverageData, String> {
    generate_coverage(root).map(|files| CoverageData {
        source: CoverageSource::LlvmCov,
        files,
    })
}

fn generate_coverage(root: &Path) -> Result<BTreeMap<String, FileCoverage>, String> {
    ensure_llvm_cov(root)?;
    collect_llvm_cov(root)
}

fn ensure_llvm_cov(root: &Path) -> Result<(), String> {
    if cargo_llvm_cov_available(root) {
        return Ok(());
    }
    Err(
        "cargo-llvm-cov is not available; install it (`cargo install cargo-llvm-cov`) \
         or pass --coverage FILE / --skip-tests"
            .to_string(),
    )
}

fn collect_llvm_cov(root: &Path) -> Result<BTreeMap<String, FileCoverage>, String> {
    let temp = coverage_tempdir()?;
    let output_path = temp.path().join("coverage.json");
    read_exported_coverage(root, &output_path)
}

fn coverage_tempdir() -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix("herdr-crap-")
        .tempdir()
        .map_err(|error| format!("cannot create temporary coverage directory: {error}"))
}

fn read_exported_coverage(
    root: &Path,
    output_path: &Path,
) -> Result<BTreeMap<String, FileCoverage>, String> {
    run_coverage_export(root, output_path)?;
    load_coverage_file(root, output_path)
}

fn run_coverage_export(root: &Path, output_path: &Path) -> Result<(), String> {
    let output = run_command(
        root,
        "cargo",
        &[
            "llvm-cov",
            "--locked",
            "--json",
            "--output-path",
            &output_path.to_string_lossy(),
        ],
    )?;
    check_coverage_export(&output)
}

fn check_coverage_export(output: &CommandOutput) -> Result<(), String> {
    if output.status == 0 {
        return Ok(());
    }
    Err(format!(
        "cargo llvm-cov failed (exit {}): {}",
        output.status,
        error_detail(&output.stderr)
    ))
}

fn load_coverage_file(
    root: &Path,
    output_path: &Path,
) -> Result<BTreeMap<String, FileCoverage>, String> {
    let value = read_coverage_value(output_path)?;
    parse_coverage_value(&value, root)
}

fn read_coverage_value(output_path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(output_path).map_err(|error| {
        format!(
            "cargo llvm-cov did not produce {}: {error}",
            output_path.display()
        )
    })?;
    serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse cargo llvm-cov JSON: {error}"))
}

fn cargo_llvm_cov_available(root: &Path) -> bool {
    Command::new("cargo")
        .args(["llvm-cov", "--version"])
        .current_dir(root)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn parse_coverage_value(
    value: &Value,
    root: &Path,
) -> Result<BTreeMap<String, FileCoverage>, String> {
    let data = coverage_data_array(value)?;
    let mut files: BTreeMap<String, FileCoverage> = BTreeMap::new();
    let mut file_entries = 0usize;
    for entry in data {
        file_entries += collect_entry_files(entry, root, &mut files);
    }
    if file_entries == 0 {
        return Err("coverage JSON contains no file entries".to_string());
    }
    Ok(files)
}

fn coverage_data_array(value: &Value) -> Result<&[Value], String> {
    value
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| "coverage JSON is missing the data[] array".to_string())
}

fn collect_entry_files(
    entry: &Value,
    root: &Path,
    files: &mut BTreeMap<String, FileCoverage>,
) -> usize {
    let Some(entry_files) = entry.get("files").and_then(Value::as_array) else {
        return 0;
    };
    let mut count = 0usize;
    for file in entry_files {
        count += collect_file_coverage(file, root, files);
    }
    count
}

fn collect_file_coverage(
    file: &Value,
    root: &Path,
    files: &mut BTreeMap<String, FileCoverage>,
) -> usize {
    let Some(filename) = file.get("filename").and_then(Value::as_str) else {
        return 0;
    };
    let mut coverage = FileCoverage::default();
    if let Some(segments) = file.get("segments").and_then(Value::as_array) {
        for segment in segments {
            add_segment(&mut coverage, segment);
        }
    }
    add_coverage_aliases(files, filename, &coverage, root);
    1
}

fn add_segment(coverage: &mut FileCoverage, segment: &Value) {
    let Some(segment) = segment.as_array() else {
        return;
    };
    if segment.len() < 4 {
        return;
    }
    let Some(line) = json_usize(&segment[0]) else {
        return;
    };
    let has_count = json_bool(&segment[3]).unwrap_or(false);
    if !has_count || line == 0 {
        return;
    }
    let count = json_f64(&segment[2]).unwrap_or(0.0);
    coverage.executable.insert(line);
    if count > 0.0 {
        coverage.covered.insert(line);
    }
}

fn json_usize(value: &Value) -> Option<usize> {
    if let Some(number) = value.as_u64() {
        return usize::try_from(number).ok();
    }
    let number = value.as_i64()?;
    if number < 0 {
        None
    } else {
        usize::try_from(number).ok()
    }
}

fn json_f64(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| {
        if value.as_bool() == Some(true) {
            Some(1.0)
        } else {
            None
        }
    })
}

fn json_bool(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| {
        value
            .as_u64()
            .map(|number| number != 0)
            .or_else(|| value.as_f64().map(|number| number != 0.0))
    })
}

fn add_coverage_aliases(
    files: &mut BTreeMap<String, FileCoverage>,
    filename: &str,
    coverage: &FileCoverage,
    root: &Path,
) {
    let path = Path::new(filename);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canonical = canonical_or_lexical(&absolute);
    let mut aliases = BTreeSet::new();
    aliases.insert(filename.to_string());
    aliases.insert(path.to_string_lossy().into_owned());
    aliases.insert(absolute.to_string_lossy().into_owned());
    aliases.insert(canonical.to_string_lossy().into_owned());
    aliases.insert(lexical_normalize(&absolute).to_string_lossy().into_owned());
    if let Ok(relative) = canonical.strip_prefix(root) {
        aliases.insert(relative.to_string_lossy().into_owned());
    }

    for alias in aliases {
        files
            .entry(alias)
            .and_modify(|existing| {
                existing
                    .executable
                    .extend(coverage.executable.iter().copied());
                existing.covered.extend(coverage.covered.iter().copied());
            })
            .or_insert_with(|| coverage.clone());
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Row {
    key: String,
    display: String,
    name: String,
    line: usize,
    cc: usize,
    coverage: f64,
    crap: f64,
}

fn crap(complexity: usize, coverage: f64) -> f64 {
    let coverage = coverage.clamp(0.0, 1.0);
    let cc = complexity as f64;
    cc * cc * (1.0 - coverage).powi(3) + cc
}

struct Measurement {
    rows: Vec<Row>,
    measured_files: BTreeSet<PathBuf>,
    line_coverage: f64,
    functions_without_line_data: usize,
    missing_files: Vec<PathBuf>,
    files_without_lines: Vec<PathBuf>,
    total_executable: usize,
}

fn measure(options: &Options, selected: &[Function], coverage: &CoverageData) -> Measurement {
    let mut measured_files: BTreeSet<PathBuf> = BTreeSet::new();
    let mut rows = Vec::new();
    let mut functions_without_line_data = 0usize;

    for function in selected {
        measured_files.insert(function.file.clone());
        let file_coverage = coverage.lookup(&function.file, &options.root);
        if coverage.source != CoverageSource::Unmeasured
            && !file_coverage
                .map(|file| file.has_executable_in_span(function.start, function.end))
                .unwrap_or(false)
        {
            functions_without_line_data += 1;
        }
        let function_coverage = file_coverage
            .map(|file| file.function_coverage(function.start, function.end))
            .unwrap_or(0.0);
        rows.push(Row {
            key: function.key.clone(),
            display: function.display.clone(),
            name: function.name.clone(),
            line: function.start,
            cc: function.cc,
            coverage: function_coverage,
            crap: crap(function.cc, function_coverage),
        });
    }

    let mut total_executable = 0usize;
    let mut total_covered = 0usize;
    let mut missing_files = Vec::new();
    let mut files_without_lines = Vec::new();
    for file in &measured_files {
        match coverage.lookup(file, &options.root) {
            Some(file_coverage) => {
                total_executable += file_coverage.executable.len();
                total_covered += file_coverage
                    .covered
                    .intersection(&file_coverage.executable)
                    .count();
                if file_coverage.executable.is_empty() {
                    files_without_lines.push(file.clone());
                }
            }
            None => missing_files.push(file.clone()),
        }
    }
    let line_coverage = if total_executable == 0 {
        0.0
    } else {
        total_covered as f64 / total_executable as f64
    };

    Measurement {
        rows,
        measured_files,
        line_coverage,
        functions_without_line_data,
        missing_files,
        files_without_lines,
        total_executable,
    }
}

#[derive(Debug, Clone)]
struct Baseline {
    path: PathBuf,
    functions: BTreeMap<String, Value>,
}

impl Baseline {
    fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("cannot read baseline {}: {error}", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("invalid baseline {}: {error}", path.display()))?;
        if value.get("version").and_then(Value::as_u64) != Some(1) {
            return Err(format!(
                "baseline {} must declare version 1",
                path.display()
            ));
        }
        let Some(functions) = value.get("functions").and_then(Value::as_object) else {
            return Err(format!(
                "baseline {} must contain a functions object",
                path.display()
            ));
        };
        let mut entries = BTreeMap::new();
        for (key, entry) in functions {
            if entry.get("crap").and_then(Value::as_f64).is_none() {
                return Err(format!(
                    "baseline entry {key} must contain a numeric crap value"
                ));
            }
            entries.insert(key.clone(), entry.clone());
        }
        Ok(Self {
            path: path.to_path_buf(),
            functions: entries,
        })
    }

    fn metric(&self, key: &str, unmeasured: bool) -> Option<f64> {
        let entry = self.functions.get(key)?;
        if !unmeasured {
            return entry.get("crap").and_then(Value::as_f64);
        }
        entry.get("max_crap").and_then(Value::as_f64).or_else(|| {
            entry
                .get("cc")
                .and_then(Value::as_u64)
                .map(|cc| crap(cc as usize, 0.0))
        })
    }

    fn label(&self) -> String {
        format!("{} ({} entries)", self.path.display(), self.functions.len())
    }
}

fn write_baseline(
    path: &Path,
    existing: BTreeMap<String, Value>,
    measurement: &Measurement,
    threshold: f64,
    source: CoverageSource,
) -> Result<usize, String> {
    let mut functions = existing;
    for row in &measurement.rows {
        functions.insert(
            row.key.clone(),
            serde_json::json!({
                "cc": row.cc,
                "coverage": row.coverage,
                "crap": row.crap,
                "max_crap": crap(row.cc, 0.0)
            }),
        );
    }
    let document = serde_json::json!({
        "version": 1,
        "threshold": threshold,
        "generated_at": crate::util::now(),
        "coverage": source.label(),
        "functions": functions
    });
    crate::util::atomic_write_json(path, &document)
        .map_err(|error| format!("cannot write baseline {}: {error}", path.display()))?;
    Ok(functions.len())
}

fn report(
    options: &Options,
    mode: &str,
    measurement: &Measurement,
    coverage: &CoverageData,
    baseline: Option<&Baseline>,
) -> i32 {
    if !options.json {
        print_report_header(options, mode, measurement, coverage, baseline);
    }
    let classification = classify_rows(options, measurement, coverage.source, baseline);
    if options.json {
        print_json_report(options, measurement, &classification, baseline);
        return exit_code(&classification);
    }
    print_text_report(&classification)
}

fn print_report_header(
    options: &Options,
    mode: &str,
    measurement: &Measurement,
    coverage: &CoverageData,
    baseline: Option<&Baseline>,
) {
    let tests = tests_label(options, coverage.source);
    println!(
        "CRAP gate: mode={mode} tests={tests} files={} functions={} line_coverage={:.1}% threshold={} coverage={} baseline={}",
        measurement.measured_files.len(),
        measurement.rows.len(),
        measurement.line_coverage * 100.0,
        options.threshold,
        coverage.source.label(),
        baseline
            .map(Baseline::label)
            .unwrap_or_else(|| "none (absolute threshold)".to_string())
    );
    print_coverage_warnings(measurement, coverage.source);
}

fn print_coverage_warnings(measurement: &Measurement, source: CoverageSource) {
    for line in coverage_warning_lines(measurement, source) {
        println!("{line}");
    }
}

fn coverage_warning_lines(measurement: &Measurement, source: CoverageSource) -> Vec<String> {
    if source == CoverageSource::Unmeasured {
        return vec![
            "CRAP gate: coverage unmeasured (--skip-tests without --coverage); function and line coverage are reported as 0%".to_string(),
        ];
    }
    measured_coverage_warning_lines(measurement)
}

fn measured_coverage_warning_lines(measurement: &Measurement) -> Vec<String> {
    let mut lines = Vec::new();
    if !measurement.missing_files.is_empty() {
        lines.push(format!(
            "CRAP gate: coverage missing line data for {} of {} measured file(s); those functions are reported as 0%",
            measurement.missing_files.len(),
            measurement.measured_files.len()
        ));
    }
    if !measurement.files_without_lines.is_empty() {
        lines.push(format!(
            "CRAP gate: coverage has no executable lines for {} measured file(s); those functions are reported as 0%",
            measurement.files_without_lines.len()
        ));
    }
    if measurement.functions_without_line_data > 0 {
        lines.push(format!(
            "CRAP gate: coverage has no line data for {} selected function(s); those functions are reported as 0%",
            measurement.functions_without_line_data
        ));
    }
    if measurement.total_executable == 0 {
        lines.push(
            "CRAP gate: coverage contains no executable line data for the measured files; line coverage is reported as 0%".to_string(),
        );
    }
    lines
}

#[derive(Default)]
struct Classification<'a> {
    violations: Vec<Violation<'a>>,
    new_count: usize,
    worsened_count: usize,
    incremental: bool,
}

#[derive(Clone)]
struct Violation<'a> {
    row: &'a Row,
    reason: &'static str,
    previous: Option<f64>,
}

struct RowClassification {
    reason: &'static str,
    previous: Option<f64>,
    new: bool,
    worsened: bool,
    violate: bool,
}

impl<'a> Classification<'a> {
    fn record(&mut self, row: &'a Row, result: RowClassification) {
        self.new_count += usize::from(result.new);
        self.worsened_count += usize::from(result.worsened);
        if result.violate {
            self.violations.push(Violation {
                row,
                reason: result.reason,
                previous: result.previous,
            });
        }
    }
}

fn classify_rows<'a>(
    options: &Options,
    measurement: &'a Measurement,
    source: CoverageSource,
    baseline: Option<&Baseline>,
) -> Classification<'a> {
    let unmeasured = source == CoverageSource::Unmeasured;
    let mut classification = Classification {
        incremental: baseline.is_some(),
        ..Classification::default()
    };
    for row in &measurement.rows {
        let result = classify_row(options, row, unmeasured, baseline);
        classification.record(row, result);
    }
    classification
}

fn classify_row(
    options: &Options,
    row: &Row,
    unmeasured: bool,
    baseline: Option<&Baseline>,
) -> RowClassification {
    let current = current_crap(row, unmeasured);
    let Some(baseline) = baseline else {
        return row_classification("absolute", current > options.threshold, None, false, false);
    };
    let previous = baseline.metric(&row.key, unmeasured);
    match previous {
        None => row_classification("new", current > options.threshold, None, true, false),
        Some(previous) if current > previous + 1e-9 => row_classification(
            "worse",
            current > options.threshold,
            Some(previous),
            false,
            true,
        ),
        Some(previous) => row_classification("grandfathered", false, Some(previous), false, false),
    }
}

fn row_classification(
    reason: &'static str,
    violate: bool,
    previous: Option<f64>,
    new: bool,
    worsened: bool,
) -> RowClassification {
    RowClassification {
        reason,
        previous,
        new,
        worsened,
        violate,
    }
}

fn current_crap(row: &Row, unmeasured: bool) -> f64 {
    if unmeasured {
        crap(row.cc, 0.0)
    } else {
        row.crap
    }
}

fn exit_code(classification: &Classification) -> i32 {
    if classification.violations.is_empty() {
        0
    } else {
        1
    }
}

fn print_json_report(
    options: &Options,
    measurement: &Measurement,
    classification: &Classification,
    baseline: Option<&Baseline>,
) {
    let rows: Vec<Value> = classification
        .violations
        .iter()
        .map(|violation| {
            serde_json::json!({
                "key": violation.row.key,
                "file": violation.row.display,
                "name": violation.row.name,
                "line": violation.row.line,
                "cc": violation.row.cc,
                "coverage": violation.row.coverage,
                "crap": violation.row.crap,
                "baseline_crap": violation.previous,
                "reason": violation.reason
            })
        })
        .collect();
    let output = serde_json::json!({
        "threshold": options.threshold,
        "functions": measurement.rows.len(),
        "line_coverage": measurement.line_coverage,
        "new": classification.new_count,
        "worsened": classification.worsened_count,
        "baseline": baseline.map(Baseline::label),
        "violations": rows
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&output)
            .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"))
    );
}

fn print_text_report(classification: &Classification) -> i32 {
    if classification.violations.is_empty() {
        println!(
            "CRAP gate: OK (new={}, worsened={}, baseline={})",
            classification.new_count,
            classification.worsened_count,
            if classification.incremental {
                "incremental"
            } else {
                "absolute"
            }
        );
        return 0;
    }

    let mut violations = classification.violations.clone();
    violations.sort_by(compare_violations);
    println!(
        "Violations: {} (new={}, worsened={}, top {MAX_VIOLATIONS})",
        violations.len(),
        classification.new_count,
        classification.worsened_count
    );
    println!(
        "{:>8} {:>8} {:>4} {:>7}  reason location",
        "CRAP", "BASE", "CC", "Cov"
    );
    for violation in violations.iter().take(MAX_VIOLATIONS) {
        println!("{}", violation_line(violation));
    }
    println!("CRAP gate: FAIL");
    1
}

fn violation_line(violation: &Violation) -> String {
    let base = violation
        .previous
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{:>8.2} {:>8} {:>4} {:>7.1}%  {} {}:{} {}",
        violation.row.crap,
        base,
        violation.row.cc,
        violation.row.coverage * 100.0,
        violation.reason,
        violation.row.display,
        violation.row.line,
        violation.row.name
    )
}

fn compare_violations(left: &Violation, right: &Violation) -> std::cmp::Ordering {
    right
        .row
        .crap
        .partial_cmp(&left.row.crap)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| left.row.display.cmp(&right.row.display))
        .then_with(|| left.row.line.cmp(&right.row.line))
        .then_with(|| left.row.name.cmp(&right.row.name))
}

impl CoverageSource {
    fn tests_label(self) -> &'static str {
        if self == CoverageSource::LlvmCov {
            "llvm-cov"
        } else {
            "skipped"
        }
    }
}

fn tests_label(options: &Options, source: CoverageSource) -> &'static str {
    if options.skip_tests {
        return "skipped";
    }
    source.tests_label()
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CommandOutput {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_command(root: &Path, program: &str, args: &[&str]) -> Result<CommandOutput, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    let status = output.status.code().unwrap_or(-1);
    Ok(CommandOutput {
        status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn run_git(root: &Path, args: &[&str]) -> Result<CommandOutput, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    let status = output.status.code().unwrap_or(-1);
    Ok(CommandOutput {
        status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn test_summary(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut found = false;
    for line in text.lines() {
        let Some(rest) = line.split("test result:").nth(1) else {
            continue;
        };
        let words: Vec<&str> = rest.split_whitespace().collect();
        for pair in words.windows(2) {
            if pair[1].starts_with("passed") {
                if let Ok(value) = pair[0].parse::<usize>() {
                    passed += value;
                    found = true;
                }
            } else if pair[1].starts_with("failed") {
                if let Ok(value) = pair[0].parse::<usize>() {
                    failed += value;
                    found = true;
                }
            }
        }
    }
    if found {
        Some(format!("{passed} passed, {failed} failed"))
    } else {
        None
    }
}

fn print_error_detail(output: &CommandOutput) {
    let detail = error_detail(&output.stderr);
    if !detail.is_empty() {
        eprintln!("{detail}");
        return;
    }
    print_stdout_detail(&output.stdout);
}

fn print_stdout_detail(stdout: &[u8]) {
    let detail = error_detail(stdout);
    if !detail.is_empty() {
        eprintln!("{detail}");
    }
}

fn error_detail(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let collapsed = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() <= ERROR_DETAIL_LIMIT {
        collapsed
    } else {
        let mut truncated: String = collapsed.chars().take(ERROR_DETAIL_LIMIT - 3).collect();
        truncated.push_str("...");
        truncated
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn canonical_or_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        lexical_normalize(&absolute)
    })
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_functions(source: &str) -> Vec<Function> {
        analyze_source(Path::new("src/lib.rs"), "src/lib.rs", source)
            .expect("inline source should parse")
    }

    fn first_complexity(source: &str) -> usize {
        let file = syn::parse_file(source).expect("inline source should parse");
        let mut collector = FunctionCollector::default();
        collector.visit_file(&file);
        collector.functions.first().expect("a function").cc
    }

    #[test]
    fn crap_formula_matches_python_gate() {
        assert!((crap(10, 1.0) - 10.0).abs() < 1e-9);
        assert!((crap(10, 0.0) - 110.0).abs() < 1e-9);
        assert!((crap(5, 0.5) - 8.125).abs() < 1e-9);
    }

    #[test]
    fn complexity_counts_branches_but_not_nested_functions_or_closures() {
        let source = "\
fn outer(x: bool) -> i32 {
    fn inner(y: bool) -> i32 {
        if y { 1 } else { 2 }
    }
    let closure = |flag: bool| if flag { 1 } else { 2 };
    if x && x {
        return inner(true) + closure(true);
    }
    0
}";
        assert_eq!(first_complexity(source), 3);
    }

    #[test]
    fn complexity_counts_else_if_but_not_bare_else() {
        let source = "\
fn f(x: i32) -> i32 {
    if x == 0 {
        0
    } else if x == 1 {
        1
    } else {
        2
    }
}";
        assert_eq!(first_complexity(source), 3);
    }

    #[test]
    fn complexity_counts_match_arms_loops_try_and_logical_operators() {
        let source = "\
fn f(items: &[u8], gate: bool) -> Option<usize> {
    let mut total = 0;
    for item in items {
        total += *item as usize;
    }
    while gate && total > 0 {
        total -= 1;
    }
    loop {
        break;
    }
    let value = items.first()?;
    match value {
        0 => total += 1,
        1 => total += 2,
        _ => total += 3,
    }
    Some(total)
}";
        // 1 base + for + while + && + loop + ? + two non-default match arms.
        assert_eq!(first_complexity(source), 8);
    }

    #[test]
    fn source_spans_cover_nested_impl_and_trait_default_functions() {
        let source = "\
pub fn free(x: bool) -> i32 {
    fn nested() -> i32 {
        1
    }
    if x { nested() } else { 0 }
}
struct S;
impl S {
    pub fn method(&self) -> i32 {
        2
    }
}
trait T {
    fn required(&self) -> i32;
    fn provided(&self) -> i32 {
        3
    }
}";
        let functions = parse_functions(source);
        let names: Vec<&str> = functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        assert_eq!(names, vec!["free", "nested", "method", "provided"]);
        assert_eq!((functions[0].start, functions[0].end), (1, 6));
        assert_eq!((functions[1].start, functions[1].end), (2, 4));
        assert_eq!((functions[2].start, functions[2].end), (9, 11));
        assert_eq!((functions[3].start, functions[3].end), (15, 17));
        assert_eq!(functions[0].cc, 2);
        assert_eq!(functions[1].cc, 1);
    }

    #[test]
    fn source_spans_ignore_macros_attributes_and_function_pointer_types() {
        let source = "\
#[cfg(test)]
mod tests {
    macro_rules! make {
        () => { fn generated() {} };
    }
    fn real(callback: fn(i32) -> i32) -> i32 {
        let value = vec![1, 2].iter().map(|item| item + 1).sum();
        callback(value)
    }
}";
        let functions = parse_functions(source);
        let names: Vec<&str> = functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        assert_eq!(names, vec!["real"]);
        assert_eq!((functions[0].start, functions[0].end), (6, 9));
    }

    #[test]
    fn parse_diff_returns_changed_ranges_and_deletion_points() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,0 +2,2 @@
+a
+b
@@ -10,2 +12,0 @@
-c
-d
";
        let changes = parse_diff(diff);
        assert_eq!(changes.get("src/a.rs"), Some(&vec![(2, 3), (12, 12)]));
    }

    #[test]
    fn intersects_includes_deletion_points() {
        assert!(intersects(5, 10, &[(10, 12)]));
        assert!(intersects(5, 10, &[(10, 10)]));
        assert!(!intersects(5, 10, &[(11, 12)]));
    }

    #[test]
    fn function_coverage_uses_covered_executable_lines_in_span() {
        let mut coverage = FileCoverage::default();
        coverage.executable.extend([10, 11, 12]);
        coverage.covered.extend([10, 12]);
        assert!((coverage.function_coverage(10, 12) - (2.0 / 3.0)).abs() < 1e-9);
        assert_eq!(coverage.function_coverage(20, 30), 0.0);
    }

    #[test]
    fn parse_coverage_marks_executable_and_covered_lines() {
        let value = serde_json::json!({
            "data": [{
                "files": [{
                    "filename": "src/lib.rs",
                    "segments": [
                        [1, 1, 1, true, true, false],
                        [2, 1, 0, true, false, false],
                        [3, 1, 5, false, true, false]
                    ]
                }]
            }]
        });
        let root = std::env::current_dir().expect("current directory");
        let files = parse_coverage_value(&value, &root).expect("valid coverage");
        let data = CoverageData {
            source: CoverageSource::External,
            files,
        };
        let coverage = data
            .lookup(Path::new("src/lib.rs"), &root)
            .expect("coverage should match relative path");
        assert_eq!(
            coverage.executable.iter().copied().collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            coverage.covered.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn unmeasured_coverage_reports_zero_without_fake_data() {
        let coverage = CoverageData::unmeasured();
        assert!(coverage
            .lookup(Path::new("src/lib.rs"), Path::new("."))
            .is_none());
    }

    #[test]
    fn cargo_test_summary_sums_unit_and_doctest_results() {
        let stdout = b"\
running 2 tests
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        assert_eq!(test_summary(stdout), Some("2 passed, 0 failed".to_string()));
        assert_eq!(test_summary(b"no test output"), None);
    }
    #[test]
    fn tokenizer_tracks_lines_through_comments_literals_and_raw_idents() {
        let source = r###"
// line comment
/* outer /* nested */ end */
fn f<'a>(x: &'a str) -> char {
    let text = "a\"b";
    let raw = r#"raw "quoted" text"#;
    let simple_raw = r"simple";
    let bytes = b"bytes";
    let raw_bytes = br#"raw bytes"#;
    let c_string = c"c string";
    let hex = '\x41';
    let unicode = '\u{1F600}';
    let quoted = '"';
    let newline = '\n';
    let byte_char = b'B';
    let number = 42usize;
    let r#type = number;
    match number {
        0 => 1,
        _ => 2,
    }
}
"###;
        let tokens = tokenize(source);
        let function = tokens
            .iter()
            .find(|token| token.text == "f")
            .expect("function token");
        assert_eq!(function.line, 4);
        assert_eq!(tokens.last().expect("closing brace").line, 22);
        assert!(tokens.iter().any(|token| token.text == "r#type"));
        let spans = scan_source_functions(source);
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (4, 22));

        assert!(tokenize("/* unterminated").is_empty());
        assert!(tokenize("\"unterminated").is_empty());
        assert!(tokenize("'").is_empty());
        assert_eq!(tokenize("r#foo")[0].text, "r#foo");
    }

    #[test]
    fn source_spans_handle_generics_and_block_closures() {
        let source = "\
fn generic<T>(value: T) -> T {
    consume(|item| { item });
    value
}
fn plain() {}
";
        let functions = parse_functions(source);
        let names: Vec<&str> = functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        assert_eq!(names, vec!["generic", "plain"]);
        assert_eq!((functions[0].start, functions[0].end), (1, 4));
        assert_eq!((functions[1].start, functions[1].end), (5, 5));
    }

    #[test]
    fn parse_hunk_header_accepts_implicit_counts_and_rejects_bad_headers() {
        assert_eq!(parse_hunk_header("@@ -1,0 +2,2 @@"), Some((2, 3)));
        assert_eq!(parse_hunk_header("@@ -7 +8 @@"), Some((8, 8)));
        assert_eq!(parse_hunk_header("@@ -1,2 +3 @@"), Some((3, 3)));
        assert_eq!(parse_hunk_header("@@ 7 +8 @@"), None);
        assert_eq!(parse_hunk_header("@@ bad"), None);
        assert_eq!(parse_hunk_header("@@"), None);
    }

    #[test]
    fn parse_options_accepts_values_in_both_forms() {
        let options = parse_options(&[
            "--threshold=5".to_string(),
            "--base".to_string(),
            "HEAD".to_string(),
            "--coverage".to_string(),
            "cov.json".to_string(),
            "--baseline=base.json".to_string(),
            "--root".to_string(),
            ".".to_string(),
            "--all".to_string(),
            "--skip-tests".to_string(),
            "--always-test".to_string(),
            "--json".to_string(),
        ])
        .expect("valid options")
        .expect("help was not requested");
        assert_eq!(options.root, PathBuf::from("."));
        assert_eq!(options.threshold, 5.0);
        assert_eq!(options.base, "HEAD");
        assert_eq!(options.coverage, Some(PathBuf::from("cov.json")));
        assert_eq!(options.baseline, Some(PathBuf::from("base.json")));
        assert!(options.all);
        assert!(options.skip_tests);
        assert!(options.always_test);
        assert!(options.json);
        assert!(!options.update_baseline);
        assert!(!options.no_baseline);
    }

    #[test]
    fn parse_options_rejects_invalid_combinations_and_inline_flags() {
        assert!(parse_options(&["--help".to_string()])
            .expect("help parses")
            .is_none());
        assert!(parse_options(&["-h".to_string()])
            .expect("help parses")
            .is_none());

        let inline_flag = parse_options(&["--all=1".to_string()]).expect_err("inline flag");
        assert!(inline_flag.contains("unrecognized argument: --all"));

        let missing = parse_options(&["--threshold".to_string()]).expect_err("missing value");
        assert!(missing.contains("--threshold requires a value"));

        let invalid = parse_options(&["--threshold".to_string(), "nope".to_string()])
            .expect_err("invalid threshold");
        assert!(invalid.contains("invalid --threshold value"));

        let infinite = parse_options(&["--threshold".to_string(), "inf".to_string()])
            .expect_err("infinite threshold");
        assert!(infinite.contains("finite"));

        let exclusive =
            parse_options(&["--update-baseline".to_string(), "--no-baseline".to_string()])
                .expect_err("mutually exclusive");
        assert!(exclusive.contains("mutually exclusive"));
    }

    #[test]
    fn revision_candidates_filter_invalid_values() {
        assert_eq!(
            base_candidates("HEAD"),
            vec![
                "HEAD".to_string(),
                DEFAULT_BASE.to_string(),
                "main".to_string(),
                "HEAD~1".to_string()
            ]
        );
        assert_eq!(
            base_candidates(DEFAULT_BASE),
            vec![
                DEFAULT_BASE.to_string(),
                "main".to_string(),
                "HEAD~1".to_string()
            ]
        );
        assert!(valid_revision("v1.0"));
        assert!(valid_revision("HEAD~1"));
        assert!(!valid_revision(""));
        assert!(!valid_revision("-x"));
        assert!(!valid_revision("a\tb"));
        assert!(!usable_candidate(""));
        assert!(!usable_candidate("0000"));
        assert!(!usable_candidate("-x"));
        assert!(!usable_candidate("a b"));
        assert!(usable_candidate("main"));
    }

    fn run_git_ok(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=CRAP Test",
                "-c",
                "user.email=crap@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn changed_ranges_and_resolve_base_inspect_a_git_repository() {
        let directory = tempfile::TempDir::new().expect("temporary repository");
        let root = directory.path();
        fs::create_dir_all(root.join("src")).expect("create src");
        fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").expect("write source");
        run_git_ok(root, &["init", "-q"]);
        run_git_ok(root, &["add", "src/lib.rs"]);
        run_git_ok(root, &["commit", "-q", "-m", "initial"]);

        assert_eq!(
            git_repo_root(root).expect("repository root"),
            canonical_or_lexical(root)
        );
        assert_eq!(
            resolve_base(root, "HEAD").expect("resolve base"),
            Some("HEAD".to_string())
        );

        fs::write(root.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n")
            .expect("modify tracked source");
        fs::write(root.join("src/untracked.rs"), "pub fn c() {}\n").expect("write untracked");
        fs::write(root.join("src/notes.txt"), "not rust\n").expect("write non-rust");

        let changes = changed_ranges(root, "HEAD").expect("changed ranges");
        let tracked = canonical_or_lexical(&root.join("src/lib.rs"));
        let ranges = changes.get(&tracked).expect("tracked changes");
        assert!(ranges.contains(&(2, 2)), "ranges: {ranges:?}");
        let untracked = canonical_or_lexical(&root.join("src/untracked.rs"));
        assert_eq!(changes.get(&untracked), Some(&vec![(1, usize::MAX)]));
        assert!(!changes.contains_key(&canonical_or_lexical(&root.join("src/notes.txt"))));

        assert!(changed_ranges(root, "does-not-exist").is_err());
        let plain = tempfile::TempDir::new().expect("plain directory");
        assert!(git_repo_root(plain.path()).is_err());
        assert_eq!(
            resolve_base(plain.path(), "HEAD").expect("no repository"),
            None
        );
    }

    #[test]
    fn untracked_paths_are_filtered_to_rust_files_under_src() {
        let directory = tempfile::TempDir::new().expect("root");
        fs::create_dir_all(directory.path().join("src")).expect("src");
        let mut changes = BTreeMap::new();
        add_untracked_paths(
            "src/keep.rs\nsrc/notes.txt\n../outside.rs\n",
            directory.path(),
            directory.path(),
            &mut changes,
        );
        assert_eq!(changes.len(), 1);
        assert!(changes
            .keys()
            .next()
            .expect("one path")
            .ends_with("src/keep.rs"));
    }

    fn empty_measurement() -> Measurement {
        Measurement {
            rows: Vec::new(),
            measured_files: BTreeSet::new(),
            line_coverage: 0.0,
            functions_without_line_data: 0,
            missing_files: Vec::new(),
            files_without_lines: Vec::new(),
            total_executable: 1,
        }
    }

    #[test]
    fn coverage_warnings_reflect_measurement_state() {
        let unmeasured = coverage_warning_lines(&empty_measurement(), CoverageSource::Unmeasured);
        assert_eq!(unmeasured.len(), 1);
        assert!(unmeasured[0].contains("coverage unmeasured"));
        assert!(coverage_warning_lines(&empty_measurement(), CoverageSource::External).is_empty());

        let mut measurement = empty_measurement();
        measurement.functions_without_line_data = 2;
        measurement
            .missing_files
            .push(PathBuf::from("src/missing.rs"));
        measurement
            .files_without_lines
            .push(PathBuf::from("src/empty.rs"));
        measurement
            .measured_files
            .insert(PathBuf::from("src/missing.rs"));
        measurement.total_executable = 0;
        let lines = coverage_warning_lines(&measurement, CoverageSource::LlvmCov);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("missing line data for 1 of 1"));
        assert!(lines[1].contains("no executable lines for 1"));
        assert!(lines[2].contains("no line data for 2"));
        assert!(lines[3].contains("no executable line data"));

        assert_eq!(CoverageSource::LlvmCov.tests_label(), "llvm-cov");
        assert_eq!(CoverageSource::External.tests_label(), "skipped");
        assert_eq!(CoverageSource::Unmeasured.tests_label(), "skipped");
    }

    #[test]
    fn parse_coverage_skips_malformed_segments_and_rejects_missing_data() {
        assert!(parse_coverage_value(&serde_json::json!({}), Path::new(".")).is_err());
        assert!(parse_coverage_value(&serde_json::json!({"data": []}), Path::new(".")).is_err());

        let value = serde_json::json!({
            "data": [
                {"no_files": true},
                {"files": [
                    {"filename": "src/lib.rs", "segments": ["oops", [1], [2, 1, 0]]},
                    {"no_filename": true},
                    {"filename": "src/other.rs"}
                ]}
            ]
        });
        let root = std::env::current_dir().expect("current directory");
        let files = parse_coverage_value(&value, &root).expect("valid coverage");
        let data = CoverageData {
            source: CoverageSource::External,
            files,
        };
        assert!(data
            .lookup(Path::new("src/other.rs"), &root)
            .expect("file entry")
            .executable
            .is_empty());
    }

    fn options_for(root: PathBuf) -> Options {
        Options {
            root,
            threshold: DEFAULT_THRESHOLD,
            all: true,
            base: DEFAULT_BASE.to_string(),
            skip_tests: true,
            always_test: false,
            coverage: None,
            baseline: None,
            update_baseline: false,
            no_baseline: false,
            json: false,
        }
    }

    #[test]
    fn baseline_selection_honors_flags_and_implicit_file() {
        let directory = tempfile::TempDir::new().expect("root");
        let mut options = options_for(directory.path().to_path_buf());
        assert!(explicit_or_implicit_baseline(&options).is_none());
        fs::write(directory.path().join(DEFAULT_BASELINE), "{}").expect("baseline");
        assert_eq!(
            explicit_or_implicit_baseline(&options),
            Some(directory.path().join(DEFAULT_BASELINE))
        );
        let explicit = PathBuf::from("/tmp/explicit-baseline.json");
        options.baseline = Some(explicit.clone());
        assert_eq!(explicit_or_implicit_baseline(&options), Some(explicit));
        options.no_baseline = true;
        assert!(explicit_or_implicit_baseline(&options).is_none());
    }

    #[test]
    fn json_report_lists_violations_and_absolute_success() {
        let directory = tempfile::TempDir::new().expect("root");
        fs::create_dir_all(directory.path().join("src")).expect("src");
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn branchy(value: bool) -> i32 { if value { 1 } else { 2 } }\n",
        )
        .expect("write source");
        let root = directory.path().to_string_lossy().into_owned();
        let failing = vec![
            "--root".to_string(),
            root.clone(),
            "--all".to_string(),
            "--skip-tests".to_string(),
            "--no-baseline".to_string(),
            "--json".to_string(),
            "--threshold".to_string(),
            "3".to_string(),
        ];
        assert_eq!(cli(&failing), 1);
        let passing = vec![
            "--root".to_string(),
            root,
            "--all".to_string(),
            "--skip-tests".to_string(),
            "--no-baseline".to_string(),
            "--json".to_string(),
            "--threshold".to_string(),
            "6".to_string(),
        ];
        assert_eq!(cli(&passing), 0);
    }

    #[test]
    fn no_change_test_messages_include_and_omit_summary() {
        assert_eq!(
            tests_passed_message("HEAD", b"test result: ok. 3 passed; 0 failed"),
            "CRAP gate: no changed production functions vs HEAD; tests passed (3 passed, 0 failed)"
        );
        assert_eq!(
            tests_passed_message("HEAD", b"no summary"),
            "CRAP gate: no changed production functions vs HEAD; tests passed"
        );
    }
}
