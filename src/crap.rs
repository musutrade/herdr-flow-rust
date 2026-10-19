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
    let mut root = std::env::current_dir()
        .map_err(|error| format!("cannot determine current directory: {error}"))?;
    let mut threshold = DEFAULT_THRESHOLD;
    let mut all = false;
    let mut base = DEFAULT_BASE.to_string();
    let mut skip_tests = false;
    let mut always_test = false;
    let mut coverage: Option<PathBuf> = None;
    let mut baseline: Option<PathBuf> = None;
    let mut update_baseline = false;
    let mut no_baseline = false;
    let mut json = false;

    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        let (name, inline_value) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument.as_str(), None),
        };
        match name {
            "-h" | "--help" if inline_value.is_none() => {
                print_usage();
                return Ok(None);
            }
            "--root" => {
                let value = option_value(args, &mut index, inline_value, "--root")?;
                root = PathBuf::from(value);
            }
            "--threshold" => {
                let value = option_value(args, &mut index, inline_value, "--threshold")?;
                threshold = value.parse::<f64>().map_err(|_| {
                    format!("invalid --threshold value {value:?}; expected a number")
                })?;
            }
            "--all" if inline_value.is_none() => all = true,
            "--base" => {
                base = option_value(args, &mut index, inline_value, "--base")?;
            }
            "--skip-tests" if inline_value.is_none() => skip_tests = true,
            "--always-test" if inline_value.is_none() => always_test = true,
            "--coverage" => {
                let value = option_value(args, &mut index, inline_value, "--coverage")?;
                coverage = Some(PathBuf::from(value));
            }
            "--baseline" => {
                let value = option_value(args, &mut index, inline_value, "--baseline")?;
                baseline = Some(PathBuf::from(value));
            }
            "--update-baseline" if inline_value.is_none() => update_baseline = true,
            "--no-baseline" if inline_value.is_none() => no_baseline = true,
            "--json" if inline_value.is_none() => json = true,
            other => {
                return Err(format!("unrecognized argument: {other}\n\n{}", usage()));
            }
        }
        index += 1;
    }

    if !threshold.is_finite() {
        return Err("--threshold must be a finite number".to_string());
    }

    if update_baseline && no_baseline {
        return Err("--update-baseline and --no-baseline are mutually exclusive".to_string());
    }
    Ok(Some(Options {
        root,
        threshold,
        all,
        base,
        skip_tests,
        always_test,
        coverage,
        baseline,
        update_baseline,
        no_baseline,
        json,
    }))
}

fn option_value(
    args: &[String],
    index: &mut usize,
    inline_value: Option<String>,
    name: &str,
) -> Result<String, String> {
    if let Some(value) = inline_value {
        return Ok(value);
    }
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{name} requires a value"))
}

// ---------------------------------------------------------------------------
// Main flow
// ---------------------------------------------------------------------------

fn run(args: &[String]) -> Result<i32, String> {
    let Some(mut options) = parse_options(args)? else {
        return Ok(0);
    };

    options.root = fs::canonicalize(&options.root)
        .map_err(|error| format!("cannot resolve --root {}: {error}", options.root.display()))?;
    if !options.root.is_dir() {
        return Err(format!(
            "--root {} is not a directory",
            options.root.display()
        ));
    }

    let files = source_files(&options.root)?;
    let mut analyzed = Vec::new();
    for file in &files {
        analyzed.extend(analyze_file(file, &options.root)?);
    }
    assign_keys(&mut analyzed);

    let implicit_baseline = options.root.join(DEFAULT_BASELINE);
    let baseline_path = if options.no_baseline {
        None
    } else if let Some(path) = &options.baseline {
        Some(path.clone())
    } else if implicit_baseline.exists() {
        Some(implicit_baseline)
    } else {
        None
    };
    let baseline = if options.update_baseline && options.all {
        // Full regeneration must not be blocked by an unreadable old baseline.
        None
    } else {
        baseline_path
            .as_ref()
            .map(|path| Baseline::load(path))
            .transpose()?
    };

    let (base, mode, selected) = if options.all {
        (None, "all".to_string(), analyzed)
    } else {
        let Some(base) = resolve_base(&options.root, &options.base)? else {
            return Err("cannot resolve a base revision; use --all or --base".to_string());
        };
        let changes = changed_ranges(&options.root, &base)?;
        let selected: Vec<Function> = analyzed
            .into_iter()
            .filter(|function| {
                changes
                    .get(&function.file)
                    .map(|ranges| intersects(function.start, function.end, ranges))
                    .unwrap_or(false)
            })
            .collect();
        let mode = format!("changed vs {base}");
        (Some(base), mode, selected)
    };

    if selected.is_empty() && !options.all {
        let base = base.as_deref().unwrap_or(DEFAULT_BASE);
        if options.update_baseline {
            println!("CRAP gate: no changed production functions vs {base}; baseline unchanged");
            return Ok(0);
        }
        return no_changed_production_functions(&options, base);
    }
    if selected.is_empty() {
        return Err("no functions found under src".to_string());
    }

    let coverage = acquire_coverage(&options)?;
    let measurement = measure(&options, &selected, &coverage);

    if options.update_baseline {
        let target = options
            .baseline
            .clone()
            .unwrap_or_else(|| options.root.join(DEFAULT_BASELINE));
        let existing = if options.all {
            BTreeMap::new()
        } else if target.exists() {
            Baseline::load(&target)?.functions
        } else {
            BTreeMap::new()
        };
        let count = write_baseline(
            &target,
            existing,
            &measurement,
            options.threshold,
            coverage.source,
        )?;
        println!(
            "CRAP gate: baseline updated {} ({} entries) from {} function(s); coverage={}",
            target.display(),
            count,
            measurement.rows.len(),
            coverage.source.label()
        );
        return Ok(0);
    }

    let result = report(&options, &mode, &measurement, &coverage, baseline.as_ref());
    Ok(result)
}

/// Behavior when no production function intersects the diff.
fn no_changed_production_functions(options: &Options, base: &str) -> Result<i32, String> {
    if !options.always_test {
        println!("CRAP gate: no changed production functions vs {base}");
        return Ok(0);
    }
    if options.skip_tests {
        println!("CRAP gate: no changed production functions vs {base}; tests skipped");
        return Ok(0);
    }

    let output = run_command(&options.root, "cargo", &["test"])?;
    if output.status != 0 {
        eprintln!(
            "CRAP gate: test suite failed (exit {}); fix tests before evaluating CRAP",
            output.status
        );
        print_error_detail(&output);
        return Ok(2);
    }
    let summary = test_summary(&output.stdout);
    match summary {
        Some(summary) => {
            println!(
                "CRAP gate: no changed production functions vs {base}; tests passed ({summary})"
            );
        }
        None => println!("CRAP gate: no changed production functions vs {base}; tests passed"),
    }
    Ok(0)
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
            && path.file_name().and_then(|name| name.to_str()) != Some("crap.rs")
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

    let spans = scan_source_functions(source);
    let mut functions = Vec::new();
    let mut cursor = 0usize;

    for ast_function in collector.functions {
        let matched = match spans.get(cursor) {
            Some(span) if span.name == ast_function.name => {
                cursor += 1;
                Some(span)
            }
            _ => {
                let found = spans
                    .iter()
                    .enumerate()
                    .skip(cursor)
                    .find(|(_, span)| span.name == ast_function.name)
                    .map(|(index, _)| index);
                match found {
                    Some(index) => {
                        cursor = index + 1;
                        spans.get(index)
                    }
                    None => None,
                }
            }
        };

        let (start, end) = match matched {
            Some(span) => (span.start, span.end),
            None => {
                eprintln!(
                    "CRAP gate: warning: cannot locate source span for function {} in {}; using line 1",
                    ast_function.name, display
                );
                (1, 1)
            }
        };

        functions.push(Function {
            file: path.to_path_buf(),
            display: display.to_string(),
            name: ast_function.name,
            start,
            end,
            cc: ast_function.cc,
            key: String::new(),
        });
    }

    Ok(functions)
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
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut line = 1usize;

    while index < chars.len() {
        let character = chars[index];

        if character == '\n' {
            line += 1;
            index += 1;
            continue;
        }
        if character.is_whitespace() {
            index += 1;
            continue;
        }

        // Comments.
        if character == '/' && chars.get(index + 1) == Some(&'/') {
            index += 2;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if character == '/' && chars.get(index + 1) == Some(&'*') {
            let end = block_comment_end(&chars, index);
            line += chars[index..end].iter().filter(|c| **c == '\n').count();
            index = end;
            continue;
        }

        // String-like literals (including raw and byte/C variants).
        if let Some(end) = string_literal_end(&chars, index) {
            line += chars[index..end].iter().filter(|c| **c == '\n').count();
            index = end;
            continue;
        }

        // Character literal or lifetime.
        if character == '\'' {
            if let Some(end) = char_literal_end(&chars, index) {
                line += chars[index..end].iter().filter(|c| **c == '\n').count();
                index = end;
            } else {
                // Lifetime: drop the quote; the following identifier is
                // tokenized normally.
                index += 1;
            }
            continue;
        }

        if is_ident_start(character) {
            let end = ident_end(&chars, index);
            let text: String = chars[index..end].iter().collect();
            tokens.push(Token {
                text,
                line,
                kind: TokenKind::Ident,
            });
            index = end;
            continue;
        }

        if character.is_ascii_digit() {
            index += 1;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
            {
                index += 1;
            }
            continue;
        }

        // Only the punctuation needed by the span scanner is retained.
        match character {
            '{' | '}' | '(' | ')' | '[' | ']' | '<' | '>' | ';' | '!' | '#' => {
                tokens.push(Token {
                    text: character.to_string(),
                    line,
                    kind: TokenKind::Punct,
                });
                index += 1;
            }
            '-' if chars.get(index + 1) == Some(&'>') => {
                index += 2;
            }
            '=' if chars.get(index + 1) == Some(&'>') => {
                index += 2;
            }
            _ => {
                index += 1;
            }
        }
    }

    tokens
}

fn block_comment_end(chars: &[char], start: usize) -> usize {
    let mut depth = 0usize;
    let mut index = start;
    while index < chars.len() {
        if chars[index] == '/' && chars.get(index + 1) == Some(&'*') {
            depth += 1;
            index += 2;
        } else if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
            depth = depth.saturating_sub(1);
            index += 2;
            if depth == 0 {
                return index;
            }
        } else {
            index += 1;
        }
    }
    chars.len()
}

fn string_literal_end(chars: &[char], start: usize) -> Option<usize> {
    let character = *chars.get(start)?;
    if character == '"' {
        return Some(escaped_string_end(chars, start + 1));
    }
    if character == 'r' {
        let mut index = start + 1;
        let mut hashes = 0usize;
        while chars.get(index) == Some(&'#') {
            hashes += 1;
            index += 1;
        }
        if chars.get(index) == Some(&'"') {
            return Some(raw_string_end(chars, index + 1, hashes));
        }
        return None;
    }
    if character == 'b' || character == 'c' {
        if chars.get(start + 1) == Some(&'"') {
            return Some(escaped_string_end(chars, start + 2));
        }
        if chars.get(start + 1) == Some(&'r') {
            let mut index = start + 2;
            let mut hashes = 0usize;
            while chars.get(index) == Some(&'#') {
                hashes += 1;
                index += 1;
            }
            if chars.get(index) == Some(&'"') {
                return Some(raw_string_end(chars, index + 1, hashes));
            }
        }
        if character == 'b' && chars.get(start + 1) == Some(&'\'') {
            return char_literal_end(chars, start + 1);
        }
    }
    None
}

fn escaped_string_end(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            '"' => return index + 1,
            _ => index += 1,
        }
    }
    chars.len()
}

fn raw_string_end(chars: &[char], mut index: usize, hashes: usize) -> usize {
    while index < chars.len() {
        if chars[index] == '"' {
            let mut check = index + 1;
            let mut found = 0usize;
            while found < hashes && chars.get(check) == Some(&'#') {
                found += 1;
                check += 1;
            }
            if found == hashes {
                return check;
            }
        }
        index += 1;
    }
    chars.len()
}

fn char_literal_end(chars: &[char], quote: usize) -> Option<usize> {
    let mut index = quote + 1;
    if index >= chars.len() {
        return None;
    }
    if chars[index] == '\\' {
        index += 1;
        match chars.get(index).copied()? {
            'x' => {
                index += 1;
                while index < chars.len() && chars[index].is_ascii_hexdigit() {
                    index += 1;
                }
            }
            'u' => {
                index += 1;
                if chars.get(index) == Some(&'{') {
                    index += 1;
                    while index < chars.len() && chars[index] != '}' {
                        index += 1;
                    }
                    if index < chars.len() {
                        index += 1;
                    }
                }
            }
            _ => index += 1,
        }
    } else {
        index += 1;
    }
    if chars.get(index) == Some(&'\'') {
        Some(index + 1)
    } else {
        None
    }
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
        let token = &tokens[index];
        let next = tokens.get(index + 1);
        let next_next = tokens.get(index + 2);

        if token.text == "#" {
            let mut group = index + 1;
            if tokens.get(group).map(|token| token.text.as_str()) == Some("!") {
                group += 1;
            }
            if tokens.get(group).map(|token| token.text.as_str()) == Some("[") {
                index = skip_delimited(tokens, group);
                continue;
            }
        }

        if token.kind == TokenKind::Ident && token.text == "macro_rules" {
            if let Some(bang) = next {
                if bang.text == "!" {
                    let mut group = index + 2;
                    if tokens
                        .get(group)
                        .map(|token| token.kind == TokenKind::Ident)
                        .unwrap_or(false)
                    {
                        group += 1;
                    }
                    if tokens
                        .get(group)
                        .map(|token| is_open_delimiter(&token.text))
                        .unwrap_or(false)
                    {
                        index = skip_delimited(tokens, group);
                        continue;
                    }
                }
            }
        }

        if token.kind == TokenKind::Ident && next.map(|token| token.text.as_str()) == Some("!") {
            if let Some(group) = next_next {
                if is_open_delimiter(&group.text) {
                    index = skip_delimited(tokens, index + 2);
                    continue;
                }
            }
        }

        result.push(token.clone());
        index += 1;
    }
    result
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
    let mut parens = 0i32;
    let mut brackets = 0i32;
    let mut angles = 0i32;
    let mut index = fn_index + 2;
    while index < tokens.len() {
        match tokens[index].text.as_str() {
            "(" => parens += 1,
            ")" => parens = (parens - 1).max(0),
            "[" => brackets += 1,
            "]" => brackets = (brackets - 1).max(0),
            "<" if track_angles => angles += 1,
            ">" if track_angles => angles = (angles - 1).max(0),
            "{" => {
                if parens == 0 && brackets == 0 && angles <= 0 {
                    return Some(end_of_brace_group(tokens, index));
                }
                index = skip_delimited(tokens, index);
                continue;
            }
            ";" if parens == 0 && brackets == 0 && angles <= 0 => return None,
            _ => {}
        }
        index += 1;
    }
    None
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
    if !line.starts_with("@@ ") {
        return None;
    }
    let mut parts = line.split_whitespace();
    parts.next()?;
    let old = parts.next()?;
    let new = parts.next()?;
    if !old.starts_with('-') || !new.starts_with('+') {
        return None;
    }
    let spec = new.strip_prefix('+')?;
    let (start_text, count_text) = match spec.split_once(',') {
        Some((start, count)) => (start, Some(count)),
        None => (spec, None),
    };
    let start = start_text.parse::<usize>().ok()?;
    let count = match count_text {
        Some(count) => count.parse::<usize>().ok()?,
        None => 1,
    };
    Some((start, start.saturating_add(count.max(1)).saturating_sub(1)))
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
    let mut candidates = Vec::new();
    if !requested.is_empty() {
        candidates.push(requested.to_string());
    }
    for fallback in [DEFAULT_BASE, "main", "HEAD~1"] {
        if !candidates.iter().any(|candidate| candidate == fallback) {
            candidates.push(fallback.to_string());
        }
    }

    for candidate in candidates {
        if candidate.chars().all(|character| character == '0') || !valid_revision(&candidate) {
            continue;
        }
        let output = run_git(root, &["rev-parse", "--verify", "--quiet", &candidate])?;
        if output.status == 0 {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn changed_ranges(root: &Path, base: &str) -> Result<BTreeMap<PathBuf, Vec<LineRange>>, String> {
    let repo_root = git_repo_root(root)?;
    let diff = run_git(
        root,
        &["diff", "--unified=0", "--no-color", base, "--", "src"],
    )?;
    if diff.status != 0 {
        return Err(format!(
            "git diff against {base} failed (exit {}): {}",
            diff.status,
            error_detail(&diff.stderr)
        ));
    }

    let mut changes: BTreeMap<PathBuf, Vec<LineRange>> = BTreeMap::new();
    for (name, ranges) in parse_diff(&String::from_utf8_lossy(&diff.stdout)) {
        let path = canonical_or_lexical(&repo_root.join(&name));
        changes.entry(path).or_default().extend(ranges);
    }

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
    if untracked.status != 0 {
        return Err(format!(
            "git ls-files failed (exit {}): {}",
            untracked.status,
            error_detail(&untracked.stderr)
        ));
    }
    let src_root = canonical_or_lexical(&root.join("src"));
    for name in String::from_utf8_lossy(&untracked.stdout).lines() {
        if !name.ends_with(".rs") {
            continue;
        }
        let path = canonical_or_lexical(&repo_root.join(name));
        if !path.starts_with(&src_root) {
            continue;
        }
        changes.entry(path).or_default().push((1, usize::MAX));
    }

    Ok(changes)
}

fn git_repo_root(root: &Path) -> Result<PathBuf, String> {
    let output = run_git(root, &["rev-parse", "--show-toplevel"])?;
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
        let text = fs::read_to_string(path)
            .map_err(|error| format!("cannot read coverage file {}: {error}", path.display()))?;
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            format!(
                "cannot parse llvm-cov JSON from {}: {error}",
                path.display()
            )
        })?;
        let files = parse_coverage_value(&value, &options.root)?;
        return Ok(CoverageData {
            source: CoverageSource::External,
            files,
        });
    }

    if options.skip_tests {
        return Ok(CoverageData::unmeasured());
    }

    if !cargo_llvm_cov_available(&options.root) {
        return Err(
            "cargo-llvm-cov is not available; install it (`cargo install cargo-llvm-cov`) \
             or pass --coverage FILE / --skip-tests"
                .to_string(),
        );
    }

    let temp = tempfile::Builder::new()
        .prefix("herdr-crap-")
        .tempdir()
        .map_err(|error| format!("cannot create temporary coverage directory: {error}"))?;
    let output_path = temp.path().join("coverage.json");
    let output = run_command(
        &options.root,
        "cargo",
        &[
            "llvm-cov",
            "--json",
            "--output-path",
            &output_path.to_string_lossy(),
        ],
    )?;
    if output.status != 0 {
        return Err(format!(
            "cargo llvm-cov failed (exit {}): {}",
            output.status,
            error_detail(&output.stderr)
        ));
    }
    let text = fs::read_to_string(&output_path).map_err(|error| {
        format!(
            "cargo llvm-cov did not produce {}: {error}",
            output_path.display()
        )
    })?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse cargo llvm-cov JSON: {error}"))?;
    let files = parse_coverage_value(&value, &options.root)?;
    Ok(CoverageData {
        source: CoverageSource::LlvmCov,
        files,
    })
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
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "coverage JSON is missing the data[] array".to_string())?;

    let mut files: BTreeMap<String, FileCoverage> = BTreeMap::new();
    let mut file_entries = 0usize;

    for entry in data {
        let Some(entry_files) = entry.get("files").and_then(Value::as_array) else {
            continue;
        };
        for file in entry_files {
            let Some(filename) = file.get("filename").and_then(Value::as_str) else {
                continue;
            };
            let mut coverage = FileCoverage::default();
            if let Some(segments) = file.get("segments").and_then(Value::as_array) {
                for segment in segments {
                    let Some(segment) = segment.as_array() else {
                        continue;
                    };
                    if segment.len() < 4 {
                        continue;
                    }
                    let Some(line) = json_usize(&segment[0]) else {
                        continue;
                    };
                    let has_count = json_bool(&segment[3]).unwrap_or(false);
                    if !has_count || line == 0 {
                        continue;
                    }
                    let count = json_f64(&segment[2]).unwrap_or(0.0);
                    coverage.executable.insert(line);
                    if count > 0.0 {
                        coverage.covered.insert(line);
                    }
                }
            }
            file_entries += 1;
            add_coverage_aliases(&mut files, filename, &coverage, root);
        }
    }

    if file_entries == 0 {
        return Err("coverage JSON contains no file entries".to_string());
    }
    Ok(files)
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
    let tests = tests_label(options, coverage.source);
    if !options.json {
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

        match coverage.source {
            CoverageSource::Unmeasured => {
                println!(
                    "CRAP gate: coverage unmeasured (--skip-tests without --coverage); function and line coverage are reported as 0%"
                );
            }
            CoverageSource::External | CoverageSource::LlvmCov => {
                if !measurement.missing_files.is_empty() {
                    println!(
                        "CRAP gate: coverage missing line data for {} of {} measured file(s); those functions are reported as 0%",
                        measurement.missing_files.len(),
                        measurement.measured_files.len()
                    );
                }
                if !measurement.files_without_lines.is_empty() {
                    println!(
                        "CRAP gate: coverage has no executable lines for {} measured file(s); those functions are reported as 0%",
                        measurement.files_without_lines.len()
                    );
                }
                if measurement.functions_without_line_data > 0 {
                    println!(
                        "CRAP gate: coverage has no line data for {} selected function(s); those functions are reported as 0%",
                        measurement.functions_without_line_data
                    );
                }
                if measurement.total_executable == 0 {
                    println!(
                        "CRAP gate: coverage contains no executable line data for the measured files; line coverage is reported as 0%"
                    );
                }
            }
        }
    }
    let unmeasured = coverage.source == CoverageSource::Unmeasured;
    let mut violations: Vec<(&Row, &'static str, Option<f64>)> = Vec::new();
    let mut new_count = 0usize;
    let mut worsened_count = 0usize;
    for row in &measurement.rows {
        let current = if unmeasured {
            crap(row.cc, 0.0)
        } else {
            row.crap
        };
        let baseline_value = baseline.and_then(|baseline| baseline.metric(&row.key, unmeasured));
        let (reason, enforce) = match (baseline, baseline_value) {
            (None, _) => ("absolute", true),
            (Some(_), None) => {
                new_count += 1;
                ("new", true)
            }
            (Some(_), Some(previous)) if current > previous + 1e-9 => {
                worsened_count += 1;
                ("worse", true)
            }
            (Some(_), Some(_)) => ("grandfathered", false),
        };
        if enforce && current > options.threshold {
            violations.push((row, reason, baseline_value));
        }
    }
    if options.json {
        let rows: Vec<Value> = violations
            .iter()
            .map(|(row, reason, previous)| {
                serde_json::json!({
                    "key": row.key,
                    "file": row.display,
                    "name": row.name,
                    "line": row.line,
                    "cc": row.cc,
                    "coverage": row.coverage,
                    "crap": row.crap,
                    "baseline_crap": previous,
                    "reason": reason
                })
            })
            .collect();
        let output = serde_json::json!({
            "threshold": options.threshold,
            "functions": measurement.rows.len(),
            "line_coverage": measurement.line_coverage,
            "new": new_count,
            "worsened": worsened_count,
            "baseline": baseline.map(Baseline::label),
            "violations": rows
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"))
        );
        return if violations.is_empty() { 0 } else { 1 };
    }
    if violations.is_empty() {
        println!(
            "CRAP gate: OK (new={new_count}, worsened={worsened_count}, baseline={})",
            if baseline.is_some() {
                "incremental"
            } else {
                "absolute"
            }
        );
        return 0;
    }

    violations.sort_by(|left, right| {
        right
            .0
            .crap
            .partial_cmp(&left.0.crap)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.display.cmp(&right.0.display))
            .then_with(|| left.0.line.cmp(&right.0.line))
            .then_with(|| left.0.name.cmp(&right.0.name))
    });

    println!(
        "Violations: {} (new={new_count}, worsened={worsened_count}, top {MAX_VIOLATIONS})",
        violations.len()
    );
    println!(
        "{:>8} {:>8} {:>4} {:>7}  reason location",
        "CRAP", "BASE", "CC", "Cov"
    );
    for (row, reason, previous) in violations.iter().take(MAX_VIOLATIONS) {
        let base = previous
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:>8.2} {:>8} {:>4} {:>7.1}%  {} {}:{} {}",
            row.crap,
            base,
            row.cc,
            row.coverage * 100.0,
            reason,
            row.display,
            row.line,
            row.name
        );
    }
    println!("CRAP gate: FAIL");
    1
}

fn tests_label(options: &Options, source: CoverageSource) -> &'static str {
    if options.skip_tests {
        return "skipped";
    }
    match source {
        CoverageSource::LlvmCov => "llvm-cov",
        CoverageSource::External => "skipped",
        CoverageSource::Unmeasured => "skipped",
    }
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
    } else {
        let detail = error_detail(&output.stdout);
        if !detail.is_empty() {
            eprintln!("{detail}");
        }
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
}
