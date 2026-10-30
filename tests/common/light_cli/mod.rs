use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

pub struct Fixture {
    pub temp: TempDir,
    pub data: PathBuf,
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub bin: PathBuf,
}
impl Fixture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let cwd = temp.path().join("project");
        let home = temp.path().join("home");
        let bin = temp.path().join("bin");
        for p in [&cwd, &home, &bin] {
            fs::create_dir(p).unwrap();
        }
        for name in ["herdr", "git", "gh", "curl", "herdr-flow"] {
            let p = bin.join(name);
            fs::write(
                &p,
                "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> \"$LIGHT_CALL_LOG\"\nexit 99\n",
            )
            .unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            temp,
            data,
            cwd,
            home,
            bin,
        }
    }
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        c.env_clear()
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.temp.path().join("state"))
            .env("PATH", &self.bin)
            .env("LIGHT_CALL_LOG", self.temp.path().join("calls"));
        if let Some(v) = std::env::var_os("LLVM_PROFILE_FILE") {
            c.env("LLVM_PROFILE_FILE", v);
        }
        c
    }
    pub fn run(&self, args: &[&str], input: Option<&Value>) -> Output {
        let mut c = self.command(env!("CARGO_BIN_EXE_herdr-flow"));
        c.args(["light", "--data-root"]).arg(&self.data).args(args);
        output(c, input)
    }
    pub fn ok(&self, args: &[&str], input: Option<&Value>) -> Value {
        success(self.run(args, input))
    }
    pub fn init_project(&self) -> i64 {
        self.ok(&["init"], None);
        self.ok(
            &[
                "project",
                "register",
                "--root",
                self.cwd.to_str().unwrap(),
                "--label",
                "first",
            ],
            None,
        )["id"]
            .as_i64()
            .unwrap()
    }
    pub fn request_file(&self, name: &str, request: &Value) -> PathBuf {
        let p = self.temp.path().join(name);
        fs::write(&p, serde_json::to_vec(request).unwrap()).unwrap();
        p
    }
    pub fn no_calls(&self) {
        assert!(!self.temp.path().join("calls").exists());
    }
}
pub fn output(mut c: Command, input: Option<&Value>) -> Output {
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    if let Some(value) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(value.to_string().as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}
pub fn success(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    v["data"].clone()
}
pub fn error(out: Output, code: i32, kind: &str) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    let v: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], kind);
    v["error"].clone()
}
pub fn request(key: &str, change: Value) -> Value {
    json!({"request_id":key,"reporter":"developer","summary":"milestone","refs":[],"change":change})
}
pub fn task(key: &str, project: i64, batch: Option<&str>) -> Value {
    request(
        key,
        json!({"kind":"create_task","task":{"project_id":project,"title":"same issue","source_ref":"https://example.test/issues/1","batch_ref":batch,"reported_state":"open"}}),
    )
}
pub fn report(key: &str, id: i64, revision: i64) -> Value {
    request(
        key,
        json!({"kind":"report","task_id":id,"expected_revision":revision,"reported_state":"done"}),
    )
}
pub fn attach(key: &str, id: i64, revision: i64, role: &str) -> Value {
    request(
        key,
        json!({"kind":"attach_binding","task_id":id,"expected_revision":revision,"binding":{"role":role,"herdr_location":{"service":"offline","workspace":"wR","pane":"p1"},"cwd":"/missing/worktree"}}),
    )
}
pub fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}
