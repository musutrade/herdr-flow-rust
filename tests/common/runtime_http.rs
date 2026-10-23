//! Deliberately synthetic native responses; never live acceptance evidence.
use serde_json::{json, Value as J};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
};
pub fn serve(listener: TcpListener, cwd: &Path) {
    let mut messages = json!([]);
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        let Some((method, path, body)) = request(&mut stream) else {
            continue;
        };
        let id = if path.contains("sescoordinator") {
            "sescoordinator"
        } else {
            "sesfixture"
        };
        let session =
            json!({"id":id,"directory":cwd,"model":{"providerID":"openai","id":"fixture"}});
        let response = match (method.as_str(), path.as_str()) {
            ("GET", "/global/health") => json!({"healthy":true,"version":"1.19.4"}),
            ("GET", "/session/status") => json!({}),
            ("GET", "/permission" | "/question") => json!([]),
            ("POST", "/session") => session,
            ("GET", p) if p.ends_with("/message") => messages.clone(),
            ("GET", _) => session,
            ("POST", p) if p.ends_with("/prompt_async") => {
                let mut model = body["model"].clone();
                model["variant"] = body["variant"].clone();
                messages = json!([
                    {"info":{"id":body["messageID"],"role":"user","sessionID":id,"model":model},"parts":body["parts"]},
                    {"info":{"id":"msgreply","role":"assistant","parentID":body["messageID"],"sessionID":id,
                        "providerID":"openai","modelID":"fixture","path":{"cwd":cwd},"time":{"completed":1},"finish":"stop"},
                     "parts":[{"type":"text","text":"synthetic report"}]}
                ]);
                json!(null)
            }
            _ => panic!("unexpected native fixture request"),
        };
        let text = response.to_string();
        let _=write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",text.len());
    }
}
fn request(stream: &mut TcpStream) -> Option<(String, String, J)> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).ok()?;
        bytes.push(byte[0]);
        assert!(bytes.len() < 16384);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let mut line = headers.lines().next().unwrap().split_whitespace();
    let method = line.next().unwrap().to_string();
    let path = line.next().unwrap().split('?').next().unwrap().to_string();
    let len = headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).unwrap();
    Some((
        method,
        path,
        if len == 0 {
            json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    ))
}
