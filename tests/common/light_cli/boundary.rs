use crate::support::*;
use std::{fs, path::Path};
use syn::visit::Visit;

fn traced(
    f: &Fixture,
    args: &[&str],
    input: Option<&serde_json::Value>,
    name: &str,
) -> serde_json::Value {
    let trace = f.temp.path().join(format!("{name}.trace"));
    let mut c = f.command("/usr/bin/strace");
    c.args([
        "-f",
        "-qq",
        "-yy",
        "-s",
        "4096",
        "-e",
        "trace=%file,%process,%network,getdents64",
        "-o",
    ])
    .arg(&trace)
    .arg("--")
    .arg(env!("CARGO_BIN_EXE_herdr-flow"))
    .args(["light", "--data-root"])
    .arg(&f.data)
    .args(args);
    let result = success(output(c, input));
    let calls = fs::read_to_string(trace).unwrap();
    assert_eq!(calls.matches("execve(").count(), 1, "{calls}");
    for forbidden in [
        "clone(",
        "clone3(",
        "fork(",
        "vfork(",
        "connect(",
        "socket(",
        "getdents64(",
    ] {
        assert!(!calls.contains(forbidden), "{forbidden}: {calls}");
    }
    for old in legacy_paths(f) {
        assert!(
            !calls.contains(path(&old)),
            "legacy access {}: {calls}",
            old.display()
        );
    }
    assert!(
        !relative_legacy_access(&calls),
        "relative legacy access: {calls}"
    );
    f.no_calls();
    result
}
// strace prints relative paths separately from AT_FDCWD/dirfd annotations.
// Inspect quoted path components as well as the absolute sentinel paths.
fn relative_legacy_access(calls: &str) -> bool {
    let quoted = regex::Regex::new(r#""[^"\n]*""#).unwrap();
    let found = quoted.find_iter(calls).any(|m| {
        m.as_str().trim_matches('"').split('/').any(|component| {
            [
                ".herdr",
                "herdr-issue-flow",
                "queue.json",
                "authority.json",
                "guard",
                "receiver.json",
                "lifecycle.json",
                ".herdr-issue-flow.toml",
            ]
            .contains(&component)
        })
    });
    found
}
#[test]
fn trace_guard_catches_relative_file_and_directory_access() {
    assert!(relative_legacy_access(
        r#"openat(AT_FDCWD</tmp/project>, "queue.json", O_RDONLY) = 3"#
    ));
    assert!(relative_legacy_access(
        r#"openat(4</tmp/home>, ".herdr/flow/registry.json", O_RDONLY) = 5"#
    ));
    assert!(!relative_legacy_access(
        r#"openat(4</tmp/data>, "records.sqlite3", O_RDONLY) = 5"#
    ));
}
fn legacy_paths(f: &Fixture) -> Vec<std::path::PathBuf> {
    [
        f.home.join(".herdr"),
        f.home.join(".config/herdr-issue-flow"),
        f.cwd.join("queue.json"),
        f.cwd.join("authority.json"),
        f.cwd.join("guard"),
        f.cwd.join("receiver.json"),
        f.cwd.join("lifecycle.json"),
        f.cwd.join(".herdr-issue-flow.toml"),
    ]
    .into()
}
#[test]
fn every_route_ignores_legacy_sentinels_and_never_spawns_or_connects() {
    let f = Fixture::new();
    let sentinels: Vec<_> = legacy_paths(&f)
        .into_iter()
        .map(|p| {
            if p.extension().is_none() {
                p.join("state.json")
            } else {
                p
            }
        })
        .collect();
    for p in &sentinels {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"not valid legacy state").unwrap();
    }
    traced(&f, &["init"], None, "init");
    let p = traced(
        &f,
        &[
            "project",
            "register",
            "--root",
            path(&f.cwd),
            "--label",
            "traced",
        ],
        None,
        "register",
    )["id"]
        .as_i64()
        .unwrap();
    traced(&f, &["project", "list"], None, "projects");
    let mut add = task("trace-add", p, Some("batch"));
    add["refs"] = serde_json::json!([{"target":path(&sentinels[0]),"temporary":true}]);
    let t = traced(&f, &["task", "add", "--input", "-"], Some(&add), "add")["task_id"]
        .as_i64()
        .unwrap();
    traced(
        &f,
        &["task", "report", "--input", "-"],
        Some(&report("trace-report", t, 1)),
        "report",
    );
    traced(
        &f,
        &["binding", "attach", "--input", "-"],
        Some(&attach("trace-bind", t, 2, "developer")),
        "attach",
    );
    traced(
        &f,
        &["task", "list", "--project", &p.to_string()],
        None,
        "tasks",
    );
    traced(
        &f,
        &[
            "task",
            "show",
            "--project",
            &p.to_string(),
            "--task",
            &t.to_string(),
        ],
        None,
        "task",
    );
    traced(
        &f,
        &[
            "binding",
            "show",
            "--project",
            &p.to_string(),
            "--task",
            &t.to_string(),
        ],
        None,
        "bindings",
    );
    traced(
        &f,
        &[
            "backup",
            "--output-root",
            path(&f.temp.path().join("snapshot")),
        ],
        None,
        "backup",
    );
    for p in sentinels {
        assert_eq!(fs::read(p).unwrap(), b"not valid legacy state");
    }
}

// Check the entire transitive repository closure, not only imports in cli.rs:
// light's modules may use sibling modules and these external leaf crates only.
struct Boundary {
    wire: bool,
    notify: bool,
}
impl<'ast> Visit<'ast> for Boundary {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let parts: Vec<_> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        assert!(
            !parts.iter().any(|s| s == "crate" || s == "herdr_flow"),
            "repository escape: {parts:?}"
        );
        assert!(
            parts.iter().filter(|s| *s == "super").count() <= 1,
            "parent escape: {parts:?}"
        );
        assert!(
            !parts
                .iter()
                .any(|s| (s == "process" && !self.notify) || (s == "net" && !self.wire)),
            "non-leaf API: {parts:?}"
        );
        if parts.first().is_some_and(|s| s == "libc") {
            assert!(
                parts.len() == 2
                    && [
                        "geteuid",
                        "getpid",
                        "openat",
                        "mkdirat",
                        "fstatfs",
                        "statfs",
                        "O_WRONLY",
                        "O_CREAT",
                        "O_EXCL",
                        "O_NOFOLLOW",
                        "O_CLOEXEC",
                        "O_RDONLY",
                        "O_DIRECTORY",
                        "O_NONBLOCK",
                        "flock",
                        "LOCK_EX",
                        "LOCK_NB"
                    ]
                    .contains(&parts[1].as_str())
                    || (self.wire && wire_ffi(&parts)),
                "unreviewed FFI: {parts:?}"
            );
        }
        syn::visit::visit_path(self, p);
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if let syn::UseTree::Path(root) = &item.tree {
            let name = root.ident.to_string();
            assert!(
                [
                    "super",
                    "self",
                    "std",
                    "serde",
                    "serde_json",
                    "rusqlite",
                    "tempfile",
                    "libc",
                    "error",
                    "model",
                    "store"
                ]
                .contains(&name.as_str()),
                "import escape: {name}"
            );
        }
        check_use(&item.tree, self.wire, self.notify);
        syn::visit::visit_item_use(self, item);
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        // The only path override is the explicitly test-only library suite.
        if item.attrs.iter().any(|a| a.path().is_ident("path")) {
            assert_eq!(item.ident, "tests");
            assert!(item.attrs.iter().any(|a| matches!(&a.meta, syn::Meta::List(m) if m.path.is_ident("cfg") && m.tokens.to_string() == "test")));
        }
        syn::visit::visit_item_mod(self, item);
    }
}
fn check_use(tree: &syn::UseTree, wire: bool, notify: bool) {
    match tree {
        syn::UseTree::Path(p) => {
            assert!(allowed_use(&p.ident.to_string(), wire, notify));
            if p.ident == "super" {
                assert!(
                    !matches!(p.tree.as_ref(), syn::UseTree::Path(next) if next.ident == "super")
                );
            }
            check_use(&p.tree, wire, notify);
        }
        syn::UseTree::Group(g) => {
            for child in &g.items {
                check_use(child, wire, notify);
            }
        }
        syn::UseTree::Name(n) => assert!(allowed_use(&n.ident.to_string(), wire, notify)),
        syn::UseTree::Rename(n) => assert!(allowed_use(&n.ident.to_string(), wire, notify)),
        _ => {}
    }
}
#[test]
fn all_light_modules_have_a_closed_repository_dependency_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/light");
    let mut count = 0;
    for file in fs::read_dir(root).unwrap() {
        let path = file.unwrap().path();
        assert_eq!(path.extension().unwrap(), "rs", "review new module layout");
        let ast = syn::parse_file(&fs::read_to_string(&path).unwrap()).unwrap();
        Boundary {
            wire: path.ends_with("observe_wire.rs"),
            notify: path.ends_with("observe_notify.rs"),
        }
        .visit_file(&ast);
        count += 1;
    }
    assert_eq!(count, 20, "new modules need an explicit boundary review");
}

fn allowed_use(name: &str, wire: bool, notify: bool) -> bool {
    !["crate", "herdr_flow"].contains(&name)
        && (name != "net" || wire)
        && (name != "process" || notify)
}
fn wire_ffi(parts: &[String]) -> bool {
    parts.len() == 2
        && [
            "ucred",
            "socklen_t",
            "getsockopt",
            "SOL_SOCKET",
            "SO_PEERCRED",
            "sockaddr_un",
            "AF_UNIX",
            "sa_family_t",
            "c_char",
            "socket",
            "SOCK_STREAM",
            "SOCK_CLOEXEC",
            "SOCK_NONBLOCK",
            "connect",
            "EINPROGRESS",
            "pollfd",
            "POLLOUT",
            "poll",
            "c_int",
            "SO_ERROR",
        ]
        .contains(&parts[1].as_str())
}
