//! One newline-delimited JSON frame per connection, with whole-frame deadlines.
use crate::{ErrorKind, FlowError, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub(super) const REQUEST_LIMIT: usize = 1024 * 1024;
// Native RPC already accepts a 16 MiB frame. Allow its result plus our envelope.
pub(super) const RESPONSE_LIMIT: usize = crate::codex::rpc::MAX_MESSAGE + 1024;

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(FlowError::new(
            ErrorKind::RpcTimeout,
            "broker frame deadline exceeded",
        ));
    }
    Ok(remaining)
}

pub(super) fn read(
    stream: &mut UnixStream,
    limit: usize,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    loop {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let mut buffer = [0; 8192];
        let size = buffer.len().min(limit.saturating_sub(bytes.len()) + 1);
        let count = match stream.read(&mut buffer[..size]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Err(FlowError::rpc_protocol("broker frame ended before newline"));
        }
        let end = buffer[..count].iter().position(|byte| *byte == b'\n');
        let payload = end.unwrap_or(count);
        if payload > limit.saturating_sub(bytes.len()) {
            return Err(FlowError::rpc_protocol("broker frame exceeds byte limit"));
        }
        bytes.extend_from_slice(&buffer[..payload]);
        if end.is_some() {
            return serde_json::from_slice(&bytes)
                .map_err(|_| FlowError::rpc_protocol("invalid broker JSON frame"));
        }
    }
}

pub(super) fn write(
    stream: &mut UnixStream,
    value: &serde_json::Value,
    limit: usize,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > limit {
        return Err(FlowError::rpc_protocol("broker frame exceeds byte limit"));
    }
    bytes.push(b'\n');
    write_bytes(stream, &bytes, deadline)
}

fn write_bytes(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> Result<()> {
    let mut remaining_bytes = bytes;
    while !remaining_bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(remaining_bytes) {
            Ok(0) => return Err(FlowError::io("broker connection closed during write")),
            Ok(count) => remaining_bytes = &remaining_bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_limit_and_fragmented_frames_are_accepted() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"{\"x\":").unwrap();
        writer.write_all(b"1}\n").unwrap();
        assert_eq!(
            read(&mut reader, 7, Duration::from_secs(1)).unwrap(),
            serde_json::json!({"x":1})
        );
    }

    #[test]
    fn malformed_empty_truncated_and_oversized_frames_are_rejected() {
        for bytes in [b"\n".as_slice(), b"x\n", b"{}", b"123456789"] {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(bytes).unwrap();
            drop(writer);
            assert!(read(&mut reader, 4, Duration::from_secs(1)).is_err());
        }
    }

    #[test]
    fn slow_drip_cannot_extend_the_total_read_deadline() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let task = std::thread::spawn(move || {
            for _ in 0..20 {
                if writer.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        assert!(read(&mut reader, 1024, Duration::from_millis(45)).is_err());
        assert!(start.elapsed() < Duration::from_millis(180));
        drop(reader);
        task.join().unwrap();
    }

    #[test]
    fn nonreading_peer_and_response_budget_fail_within_bounds() {
        let (_reader, mut writer) = UnixStream::pair().unwrap();
        let value = serde_json::json!("x".repeat(1024 * 1024));
        let start = Instant::now();
        assert!(write(
            &mut writer,
            &value,
            2 * 1024 * 1024,
            Duration::from_millis(40)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        let error = write(&mut writer, &value, 1024, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::RpcProtocol);
    }
}
