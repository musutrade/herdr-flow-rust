//! Reject Clippy suppressions, including inactive attributes and macro bodies.

use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::ext::IdentExt;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Meta, Token};

/// Check repository Rust sources. Findings exit 1; invalid input exits 2.
pub fn cli(args: &[String]) -> i32 {
    if args == ["--help"] || args == ["-h"] {
        println!("Usage: herdr-flow lint-policy [--root DIR]");
        return 0;
    }
    match root(args).and_then(|root| check_tree(&root)) {
        Ok(0) => {
            println!("Clippy suppression policy passed");
            0
        }
        Ok(_) => 1,
        Err(error) => {
            eprintln!("Clippy suppression policy: {error}");
            2
        }
    }
}

fn root(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
        [flag, path] if flag == "--root" => Ok(PathBuf::from(path)),
        _ => Err("expected lint-policy [--root DIR]".into()),
    }
}

fn check_tree(root: &Path) -> Result<usize, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut findings = 0;
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|error| error.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            findings += check_entry(&entry, &mut pending)?;
        }
    }
    Ok(findings)
}

fn check_entry(entry: &fs::DirEntry, pending: &mut Vec<PathBuf>) -> Result<usize, String> {
    let path = entry.path();
    let kind = entry.file_type().map_err(|error| error.to_string())?;
    if kind.is_dir() {
        if !matches!(
            entry.file_name().to_str(),
            Some(".git" | "target" | "node_modules")
        ) {
            pending.push(path);
        }
        return Ok(0);
    }
    if path.extension().and_then(|value| value.to_str()) == Some("rs") {
        return check_file(&path);
    }
    Ok(0)
}

fn check_file(path: &Path) -> Result<usize, String> {
    let source = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let violations =
        suppressions(&source).map_err(|error| format!("{}: {error}", path.display()))?;
    for (line, lint) in &violations {
        eprintln!(
            "{}:{line}: Clippy suppression is forbidden: {lint}",
            path.display()
        );
    }
    Ok(violations.len())
}

fn suppressions(source: &str) -> Result<Vec<(usize, String)>, String> {
    let tokens = source
        .parse::<TokenStream>()
        .map_err(|error| error.to_string())?;
    let mut violations = Vec::new();
    scan_tokens(tokens, &mut violations)?;
    Ok(violations)
}

fn scan_tokens(tokens: TokenStream, violations: &mut Vec<(usize, String)>) -> Result<(), String> {
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        if matches!(&token, TokenTree::Punct(punct) if punct.as_char() == '#') {
            if matches!(tokens.peek(), Some(TokenTree::Punct(punct)) if punct.as_char() == '!') {
                tokens.next();
            }
            scan_attribute(tokens.peek(), violations)?;
        }
        if let TokenTree::Group(group) = token {
            scan_tokens(group.stream(), violations)?;
        }
    }
    Ok(())
}

fn scan_attribute(
    token: Option<&TokenTree>,
    violations: &mut Vec<(usize, String)>,
) -> Result<(), String> {
    let Some(TokenTree::Group(group)) = token else {
        return Ok(());
    };
    if group.delimiter() == Delimiter::Bracket {
        // Macro placeholders may not parse as concrete Rust attributes.
        if let Ok(meta) = syn::parse2::<Meta>(group.stream()) {
            check_meta(&meta, group.span().start().line, violations)?;
        }
    }
    Ok(())
}

fn check_meta(
    meta: &Meta,
    line: usize,
    violations: &mut Vec<(usize, String)>,
) -> Result<(), String> {
    let Meta::List(list) = meta else {
        return Ok(());
    };
    if named(&list.path, "cfg_attr") {
        for nested in meta_items(list)?.iter().skip(1) {
            check_meta(nested, line, violations)?;
        }
    }
    if named(&list.path, "allow") || named(&list.path, "expect") {
        for lint in meta_items(list)? {
            check_lint(&lint, line, violations);
        }
    }
    Ok(())
}

fn meta_items(list: &syn::MetaList) -> Result<Punctuated<Meta, Token![,]>, String> {
    Punctuated::<Meta, Token![,]>::parse_terminated
        .parse2(list.tokens.clone())
        .map_err(|error| format!("cannot inspect lint attribute: {error}"))
}

fn check_lint(lint: &Meta, line: usize, violations: &mut Vec<(usize, String)>) {
    let path = lint.path();
    let is_clippy = path
        .segments
        .first()
        .is_some_and(|part| part.ident.unraw() == "clippy");
    if is_clippy || named(path, "warnings") {
        let name = path
            .segments
            .iter()
            .map(|part| part.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        violations.push((line, name));
    }
}

fn named(path: &syn::Path, name: &str) -> bool {
    path.get_ident().is_some_and(|ident| ident.unraw() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_inner_outer_multiline_and_multiple_lints() {
        let source =
            "#![allow(clippy::all)]\n#[allow(dead_code,\n clippy::type_complexity)] fn f() {}";
        assert_eq!(
            suppressions(source).unwrap(),
            vec![
                (1, "clippy::all".into()),
                (2, "clippy::type_complexity".into())
            ]
        );
    }

    #[test]
    fn rejects_expectations_and_warning_groups_even_with_reason() {
        let source = "#[expect(clippy::pedantic, reason = \"test\")] #[allow(warnings)] #[expect(warnings)] fn f() {}";
        assert_eq!(suppressions(source).unwrap().len(), 3);
    }

    #[test]
    fn rejects_inactive_nested_cfg_attributes() {
        let source = "#[cfg_attr(any(), cfg_attr(all(), allow(clippy::nursery)))] fn f() {}";
        assert_eq!(
            suppressions(source).unwrap(),
            vec![(1, "clippy::nursery".into())]
        );
    }

    #[test]
    fn rejects_raw_identifiers() {
        let source = "#[r#allow(r#clippy::pedantic)] #[r#expect(r#warnings)] fn f() {}";
        assert_eq!(suppressions(source).unwrap().len(), 2);
    }

    #[test]
    fn scans_macro_bodies_without_treating_strings_or_comments_as_attributes() {
        let source = r###"
            // #[allow(clippy::all)]
            const TEXT: &str = "#[expect(clippy::all)]";
            macro_rules! f { () => { #[allow(clippy::too_many_arguments)] fn f() {} }; }
        "###;
        assert_eq!(
            suppressions(source).unwrap(),
            vec![(4, "clippy::too_many_arguments".into())]
        );
    }

    #[test]
    fn allows_enforcement_and_non_clippy_lints() {
        let source = "#![forbid(clippy::all)] #[deny(clippy::too_many_arguments)] #[allow(dead_code)] fn f() {}";
        assert!(suppressions(source).unwrap().is_empty());
    }

    #[test]
    fn fails_closed_on_unreadable_lint_arguments() {
        assert!(
            suppressions("macro_rules! f { ($lint:path) => { #[allow($lint)] fn f() {} }; }")
                .is_err()
        );
    }

    #[test]
    fn cli_checks_all_rust_files_and_reports_invalid_input() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("tests")).unwrap();
        let file = root.path().join("tests/fixture.rs");
        fs::write(&file, "#[cfg_attr(any(), allow(clippy::all))] fn f() {}").unwrap();
        let args = vec!["--root".into(), root.path().display().to_string()];
        assert_eq!(cli(&args), 1);
        fs::write(&file, "fn f() {}").unwrap();
        assert_eq!(cli(&args), 0);
        assert_eq!(cli(&["--unknown".into()]), 2);
        assert_eq!(cli(&["--help".into()]), 0);
    }
}
