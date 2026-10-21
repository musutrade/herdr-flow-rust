//! Observe WebSocket bytes without owning sockets or changing forwarded data.

use std::cell::RefCell;
use std::rc::Rc;

use super::SessionObserver;
use crate::codex::rpc::{decode_message, protocol_error, FrameDecoder, MAX_HEADERS, MAX_MESSAGE};
use crate::error::Result;
use crate::model::J;

/// Observe one direction without altering any forwarded byte.
pub struct WireObserver {
    client: bool,
    observer: Rc<RefCell<SessionObserver>>,
    headers: Vec<u8>,
    upgraded: bool,
    decoder: FrameDecoder,
}

impl WireObserver {
    pub fn new(client: bool, observer: Rc<RefCell<SessionObserver>>) -> Self {
        Self {
            client,
            observer,
            headers: Vec::new(),
            upgraded: false,
            decoder: FrameDecoder::new(client, MAX_MESSAGE),
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<()> {
        if !self.upgraded {
            self.headers.extend_from_slice(data);
            let boundary = self
                .headers
                .windows(4)
                .position(|window| window == b"\r\n\r\n");
            if boundary.unwrap_or(self.headers.len()) > MAX_HEADERS {
                return Err(protocol_error("proxy handshake exceeds limit"));
            }
            let Some(boundary) = boundary else {
                return Ok(());
            };
            let head = &self.headers[..boundary];
            let first_line = head
                .windows(2)
                .position(|window| window == b"\r\n")
                .map(|position| &head[..position])
                .unwrap_or(head);
            let upgraded = if self.client {
                first_line.starts_with(b"GET ")
            } else {
                first_line.starts_with(b"HTTP/1.1 101 ")
            };
            if !upgraded {
                return Err(protocol_error("proxy did not observe a WebSocket upgrade"));
            }
            let remaining: Vec<u8> = self.headers[boundary + 4..].to_vec();
            self.headers.clear();
            self.upgraded = true;
            return self.frames(&remaining);
        }
        self.frames(data)
    }

    fn frames(&mut self, data: &[u8]) -> Result<()> {
        let frames = self.decoder.feed(data)?;
        self.handle_frames(frames)
    }

    fn handle_frames(&mut self, frames: Vec<(u8, Vec<u8>)>) -> Result<()> {
        frames
            .into_iter()
            .try_for_each(|(opcode, payload)| self.handle_frame(opcode, &payload))
    }

    fn handle_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        if opcode == 1 {
            return self.handle_text_frame(payload);
        }
        reject_unsupported_frame(opcode)
    }

    fn handle_text_frame(&mut self, payload: &[u8]) -> Result<()> {
        let message = decode_message(payload)?;
        self.observe_message(&message)
    }

    fn observe_message(&mut self, message: &J) -> Result<()> {
        if self.client {
            self.observer.borrow_mut().request(message)
        } else {
            self.observer.borrow_mut().response(message)
        }
    }
}

fn reject_unsupported_frame(opcode: u8) -> Result<()> {
    if opcode == 8 {
        return Err(protocol_error("native WebSocket sent close"));
    }
    reject_binary_frame(opcode)
}

fn reject_binary_frame(opcode: u8) -> Result<()> {
    if opcode == 2 {
        return Err(protocol_error(
            "native WebSocket used unsupported binary JSON-RPC",
        ));
    }
    Ok(())
}
