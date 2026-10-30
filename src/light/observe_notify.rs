//! Bark leaf: a key is read privately and supplied on stdin, never argv or storage.
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(super) fn valid_endpoint(endpoint: &str) -> bool {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return false;
    };
    let Some(authority) = rest.strip_suffix("/push") else {
        return false;
    };
    if authority.is_empty()
        || authority
            .chars()
            .any(|c| c.is_whitespace() || "/@?#\\".contains(c))
    {
        return false;
    }
    scheme == "https" || (scheme == "http" && loopback(authority))
}
fn loopback(authority: &str) -> bool {
    let host = authority.split(':').next().unwrap_or("");
    host == "127.0.0.1" || authority.starts_with("[::1]:") || authority == "[::1]"
}
pub(super) struct Outcome {
    pub status: &'static str,
    pub reason: &'static str,
}
fn outcome(status: &'static str, reason: &'static str) -> Outcome {
    Outcome { status, reason }
}
pub(super) fn send(path: &Path, endpoint: &str, task: i64, binding: i64) -> Outcome {
    if !valid_endpoint(endpoint) {
        return outcome("failed", "invalid_endpoint");
    }
    let Ok(key) = key(path) else {
        return outcome("failed", "key_unavailable_or_invalid");
    };
    let payload=json!({"device_key":key,"title":"Herdr light: intervention needed","body":format!("Task {task}, binding {binding}: blocked. Inspect the registered session; no recovery was attempted.")}).to_string();
    match exchange(endpoint, payload) {
        Ok(output) => classify(&output),
        Err(_) => outcome("unknown", "transport_or_bounded_response_failure"),
    }
}
fn key(path: &Path) -> io::Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o600
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let mut bytes = Vec::new();
    file.take(257).read_to_end(&mut bytes)?;
    parse_key(bytes)
}
fn parse_key(bytes: Vec<u8>) -> io::Result<String> {
    if bytes.len() > 256 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let value = String::from_utf8(bytes).map_err(|_| io::ErrorKind::InvalidData)?;
    let value = value.trim();
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(value.into())
}
fn exchange(endpoint: &str, payload: String) -> io::Result<Vec<u8>> {
    // Keep existing curl network configuration. Override tracing/output only.
    let mut child = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--no-verbose",
            "--trace",
            "/dev/null",
            "--max-time",
            "3",
            "--connect-timeout",
            "2",
            "--max-filesize",
            "4096",
            "--retry",
            "0",
            "--max-redirs",
            "0",
            "--no-location",
            "--output",
            "-",
            "--request",
            "POST",
            "--header",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            "--write-out",
            "\n%{http_code}",
            "--url",
            endpoint,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?;
    let writer = std::thread::spawn(move || input.write_all(payload.as_bytes()));
    let output = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output.take(4100).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(4);
    let status = wait(&mut child, deadline);
    let _ = writer.join();
    let bytes = reader.join().map_err(|_| io::ErrorKind::BrokenPipe)??;
    if !status? || bytes.len() >= 4100 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}
fn wait(child: &mut std::process::Child, deadline: Instant) -> io::Result<bool> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn classify(bytes: &[u8]) -> Outcome {
    let Some(split) = bytes.iter().rposition(|b| *b == b'\n') else {
        return outcome("unknown", "missing_http_status");
    };
    let http = std::str::from_utf8(&bytes[split + 1..])
        .ok()
        .and_then(|s| s.parse::<u16>().ok());
    if http.is_some_and(|code| (400..600).contains(&code)) {
        return outcome("failed", "http_rejected");
    }
    if http != Some(200) {
        return outcome("unknown", "ambiguous_http_status");
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes[..split]) else {
        return outcome("unknown", "invalid_api_response");
    };
    match value["code"].as_i64() {
        Some(200) => outcome("accepted", "api_accepted_not_delivery_confirmation"),
        Some(_) => outcome("failed", "api_rejected"),
        None => outcome("unknown", "missing_api_status"),
    }
}
