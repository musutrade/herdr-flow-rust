//! End-to-end CLI tests for the Rust CRAP gate. These deliberately use
//! `--all --skip-tests` (or an explicit coverage export) so they never invoke
//! git, cargo test or cargo-llvm-cov from inside the test suite.

pub mod common;

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

fn project(files: &[(&str, &str)]) -> TempDir {
    let directory = common::tempdir().expect("temporary crate root");
    for (relative, contents) in files {
        let path = directory.path().join(relative);
        if let Some(parent) = path.parent() {
            common::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(path, contents).expect("write fixture source");
    }
    directory
}

fn args(root: &Path, extra: &[&str]) -> Vec<String> {
    let mut values = vec!["--root".to_string(), root.to_string_lossy().into_owned()];
    values.extend(extra.iter().map(|value| (*value).to_string()));
    values
}

fn coverage_file(root: &Path, source: &Path, segments: &str) -> PathBuf {
    let path = root.join("coverage.json");
    let json = format!(
        "{{\"data\":[{{\"files\":[{{\"filename\":{},\"segments\":{}}}]}}]}}",
        serde_json::to_string(&source.to_string_lossy()).expect("path JSON"),
        segments
    );
    fs::write(&path, json).expect("write coverage fixture");
    path
}

#[test]
fn all_skip_tests_reports_violations_and_success() {
    let directory = project(&[(
        "src/lib.rs",
        "pub fn simple() -> i32 { 1 }\n\
         pub fn branchy(value: bool) -> i32 { if value { 1 } else { 2 } }\n",
    )]);
    let root = directory.path();

    // branchy has CC=2 and no coverage, so CRAP=6 > 3.
    let failing = args(root, &["--all", "--skip-tests", "--threshold", "3"]);
    assert_eq!(herdr_flow::crap::cli(&failing), 1);

    let passing = args(root, &["--all", "--skip-tests", "--threshold=10"]);
    assert_eq!(herdr_flow::crap::cli(&passing), 0);
}

#[test]
fn external_coverage_changes_the_exit_code() {
    let directory = project(&[(
        "src/lib.rs",
        "pub fn branchy(value: bool) -> i32 { if value { 1 } else { 2 } }\n",
    )]);
    let root = directory.path();
    let source = root.join("src/lib.rs");

    let covered = coverage_file(root, &source, "[[1,1,1,true,true,false]]");
    let covered = covered.to_string_lossy().into_owned();

    // CC=2, coverage=100% => CRAP=2.
    let failing = args(
        root,
        &[
            "--all",
            "--skip-tests",
            "--coverage",
            &covered,
            "--threshold",
            "1",
        ],
    );
    assert_eq!(herdr_flow::crap::cli(&failing), 1);

    let passing = args(
        root,
        &[
            "--all",
            "--skip-tests",
            "--coverage",
            &covered,
            "--threshold",
            "2",
        ],
    );
    assert_eq!(herdr_flow::crap::cli(&passing), 0);

    let uncovered = coverage_file(root, &source, "[[1,1,0,true,true,false]]");
    let uncovered = uncovered.to_string_lossy().into_owned();
    let failing = args(
        root,
        &[
            "--all",
            "--skip-tests",
            "--coverage",
            &uncovered,
            "--threshold",
            "5",
        ],
    );
    assert_eq!(herdr_flow::crap::cli(&failing), 1);
}

#[test]
fn malformed_inputs_are_tool_errors() {
    let directory = project(&[("src/lib.rs", "pub fn f() {}\n")]);
    let root = directory.path();

    let unknown = args(root, &["--all", "--skip-tests", "--bogus"]);
    assert_eq!(herdr_flow::crap::cli(&unknown), 2);

    let invalid_threshold = args(
        root,
        &["--all", "--skip-tests", "--threshold", "not-a-number"],
    );
    assert_eq!(herdr_flow::crap::cli(&invalid_threshold), 2);

    let missing_coverage = args(
        root,
        &[
            "--all",
            "--skip-tests",
            "--coverage",
            root.join("missing.json").to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(herdr_flow::crap::cli(&missing_coverage), 2);

    let bad_coverage = root.join("bad.json");
    fs::write(&bad_coverage, "not json").expect("write invalid coverage");
    let bad_coverage = bad_coverage.to_string_lossy().into_owned();
    let malformed = args(
        root,
        &["--all", "--skip-tests", "--coverage", &bad_coverage],
    );
    assert_eq!(herdr_flow::crap::cli(&malformed), 2);
}

#[test]
fn empty_or_missing_source_tree_is_a_tool_error() {
    let empty = common::tempdir().expect("temporary root");
    common::create_dir_all(empty.path().join("src")).expect("create src");
    let no_functions = args(empty.path(), &["--all", "--skip-tests"]);
    assert_eq!(herdr_flow::crap::cli(&no_functions), 2);

    let missing = common::tempdir().expect("temporary root");
    let no_src = args(missing.path(), &["--all", "--skip-tests"]);
    assert_eq!(herdr_flow::crap::cli(&no_src), 2);
}

#[test]
fn help_returns_success() {
    assert_eq!(herdr_flow::crap::cli(&["--help".to_string()]), 0);
}

#[test]
fn branchy_closure_cannot_escape_the_gate() {
    let directory = project(&[(
        "src/lib.rs",
        "pub fn wrapper() { let run = || { if true { if false {} } }; run(); }\n",
    )]);
    let root = directory.path();
    // Only the closure branches: CC=3, CRAP=12 at zero coverage.
    assert_eq!(
        herdr_flow::crap::cli(&args(
            root,
            &[
                "--all",
                "--skip-tests",
                "--no-baseline",
                "--threshold",
                "10"
            ]
        )),
        1
    );
}

#[test]
fn incremental_baseline_grandfathers_existing_and_rejects_new_or_worse() {
    let simple = "pub fn simple() -> i32 { 1 }\n\
                  pub fn branchy(value: bool) -> i32 { if value { 1 } else { 2 } }\n";
    let directory = project(&[("src/lib.rs", simple)]);
    let root = directory.path();
    let baseline = root.join("crap-baseline.json");

    let update = args(
        root,
        &[
            "--all",
            "--skip-tests",
            "--threshold",
            "10",
            "--update-baseline",
        ],
    );
    assert_eq!(herdr_flow::crap::cli(&update), 0);
    assert!(baseline.exists(), "baseline was not written");

    // branchy has CRAP 6 at 0% coverage. It is grandfathered even though the
    // requested threshold is lower; only new or worsened functions are gated.
    let grandfathered = args(root, &["--all", "--skip-tests", "--threshold", "3"]);
    assert_eq!(herdr_flow::crap::cli(&grandfathered), 0);

    let with_new = format!(
        "{simple}pub fn new_branchy(a: bool, b: bool) -> i32 {{ if a {{ if b {{ 1 }} else {{ 2 }} }} else {{ 3 }} }}\n"
    );
    fs::write(root.join("src/lib.rs"), with_new).expect("write new function");
    let new_function = args(root, &["--all", "--skip-tests", "--threshold", "10"]);
    assert_eq!(herdr_flow::crap::cli(&new_function), 1);

    // Accept the new function into the baseline, then make an existing
    // function worse; that regression must fail.
    let update = args(
        root,
        &[
            "--all",
            "--skip-tests",
            "--threshold",
            "10",
            "--update-baseline",
        ],
    );
    assert_eq!(herdr_flow::crap::cli(&update), 0);
    fs::write(
        root.join("src/lib.rs"),
        "pub fn simple() -> i32 { 1 }\n\
         pub fn branchy(value: bool, other: bool) -> i32 { if value && other { 1 } else { 2 } }\n\
         pub fn new_branchy(a: bool, b: bool) -> i32 { if a { if b { 1 } else { 2 } } else { 3 } }\n",
    )
    .expect("worsen function");
    let worse = args(root, &["--all", "--skip-tests", "--threshold", "10"]);
    assert_eq!(herdr_flow::crap::cli(&worse), 1);
}

#[test]
fn unmeasured_runs_use_the_baseline_complexity_ceiling() {
    let source = "pub fn branchy(value: bool) -> i32 { if value { 1 } else { 2 } }\n";
    let directory = project(&[("src/lib.rs", source)]);
    let root = directory.path();
    let source_path = root.join("src/lib.rs");
    let covered = coverage_file(root, &source_path, "[[1,1,1,true,true,false]]");
    let covered = covered.to_string_lossy().into_owned();

    // Measured baseline: CC=2, 100% coverage => CRAP=2.
    let update = args(
        root,
        &[
            "--all",
            "--skip-tests",
            "--coverage",
            &covered,
            "--threshold",
            "10",
            "--update-baseline",
        ],
    );
    assert_eq!(herdr_flow::crap::cli(&update), 0);

    // Without coverage the unmeasured CRAP is 6, but the baseline's max_crap
    // is also 6; this is not a regression and must not be treated as new.
    let unmeasured = args(root, &["--all", "--skip-tests", "--threshold", "1"]);
    assert_eq!(herdr_flow::crap::cli(&unmeasured), 0);
}
