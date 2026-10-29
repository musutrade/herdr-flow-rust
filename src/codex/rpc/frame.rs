//! WebSocket frame bytes and fragmentation state; no socket or RPC lifecycle.

use super::{protocol_error, MAX_MESSAGE};
use crate::error::Result;
use crate::util::pyjson::random_bytes;

/// RFC 6455 XOR masking. Empty masks leave payload bytes unchanged; otherwise
/// at least four mask bytes are required and only the first four are used.
/// Empty payloads return immediately; size-limit errors precede mask validation.
pub fn apply_mask(payload: &[u8], mask: &[u8], max_message: usize) -> Result<Vec<u8>> {
    if payload.is_empty() {
        return Ok(Vec::new());
    }
    if payload.len() > max_message {
        return Err(protocol_error("payload exceeds maximum size"));
    }
    let mask = if mask.is_empty() { &[0u8; 4][..] } else { mask };
    if mask.len() < 4 {
        return Err(protocol_error("WebSocket mask must contain four bytes"));
    }
    Ok(payload
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ mask[index % 4])
        .collect())
}

/// Encode one RFC 6455 frame. Client frames must be masked.
pub fn encode_frame(payload: &[u8], opcode: u8, masked: bool, final_: bool) -> Result<Vec<u8>> {
    encode_frame_with_limit(payload, opcode, masked, final_, MAX_MESSAGE)
}

pub fn encode_frame_with_limit(
    payload: &[u8],
    opcode: u8,
    masked: bool,
    final_: bool,
    limit: usize,
) -> Result<Vec<u8>> {
    let length = payload.len();
    if length > limit {
        return Err(protocol_error("payload exceeds maximum size"));
    }
    let mut frame = Vec::with_capacity(length + 14);
    frame.push(if final_ { 0x80 } else { 0 } | opcode);
    let flag = if masked { 0x80 } else { 0 };
    if length < 126 {
        frame.push(flag | length as u8);
    } else if length < 65_536 {
        frame.push(flag | 126);
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(flag | 127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    if !masked {
        frame.extend_from_slice(payload);
        return Ok(frame);
    }
    let mask = random_bytes(4)?;
    frame.extend_from_slice(&mask);
    frame.extend_from_slice(&apply_mask(payload, &mask, limit)?);
    Ok(frame)
}

pub fn validate_close(payload: &[u8]) -> Result<()> {
    if payload.len() == 1 {
        return Err(protocol_error("invalid WebSocket close payload"));
    }
    validate_close_reason(payload)
}

fn validate_close_reason(payload: &[u8]) -> Result<()> {
    if payload.len() < 2 {
        return Ok(());
    }
    validate_close_fields(payload)
}

fn validate_close_fields(payload: &[u8]) -> Result<()> {
    validate_close_code(payload)?;
    validate_close_text(payload)
}

fn validate_close_code(payload: &[u8]) -> Result<()> {
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if invalid_close_code(code) {
        return Err(protocol_error("invalid WebSocket close code"));
    }
    Ok(())
}

fn invalid_close_code(code: u16) -> bool {
    matches!(
        code,
        0..=999 | 1004 | 1005 | 1006 | 1015 | 1016..=2999 | 5000..=u16::MAX
    )
}

fn validate_close_text(payload: &[u8]) -> Result<()> {
    std::str::from_utf8(&payload[2..])
        .map_err(|_| protocol_error("invalid WebSocket close reason"))?;
    Ok(())
}

pub struct FrameDecoder {
    pub masked: bool,
    pub max_message: usize,
    buffer: Vec<u8>,
    fragments: Vec<u8>,
    fragment_opcode: Option<u8>,
}

impl FrameDecoder {
    /// Includes an unfinished fragmented message even when its first fragment
    /// is empty. Its admission must survive until completion or disconnection.
    pub(crate) fn has_pending(&self) -> bool {
        !self.buffer.is_empty() || self.fragment_opcode.is_some()
    }

    pub fn new(masked: bool, max_message: usize) -> Self {
        Self {
            masked,
            max_message,
            buffer: Vec::new(),
            fragments: Vec::new(),
            fragment_opcode: None,
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<(u8, Vec<u8>)>> {
        self.buffer.extend_from_slice(data);
        let mut messages = Vec::new();
        while let Some((final_, opcode, length, offset)) = self.header()? {
            let payload = self.take_payload(length, offset)?;
            self.apply_frame(final_, opcode, payload, &mut messages)?;
        }
        Ok(messages)
    }

    fn header(&self) -> Result<Option<(bool, u8, usize, usize)>> {
        let Some((first, second)) = self.first_two_bytes() else {
            return Ok(None);
        };
        validate_frame_header(first, second, self.masked)?;
        self.frame_layout(second, first & 0x80 != 0, first & 0x0f)
    }

    fn first_two_bytes(&self) -> Option<(u8, u8)> {
        if self.buffer.len() < 2 {
            return None;
        }
        Some((self.buffer[0], self.buffer[1]))
    }

    fn frame_layout(
        &self,
        second: u8,
        final_: bool,
        opcode: u8,
    ) -> Result<Option<(bool, u8, usize, usize)>> {
        let Some((length, offset)) = self.payload_length(second)? else {
            return Ok(None);
        };
        self.check_frame_layout(final_, opcode, length, offset)
    }

    fn check_frame_layout(
        &self,
        final_: bool,
        opcode: u8,
        length: usize,
        offset: usize,
    ) -> Result<Option<(bool, u8, usize, usize)>> {
        validate_frame_constraints(
            final_,
            opcode,
            length,
            self.fragments.len(),
            self.max_message,
        )?;
        self.available_frame(final_, opcode, length, offset)
    }

    fn available_frame(
        &self,
        final_: bool,
        opcode: u8,
        length: usize,
        offset: usize,
    ) -> Result<Option<(bool, u8, usize, usize)>> {
        let offset = masked_offset(offset, self.masked);
        if self.buffer.len() < offset + length {
            return Ok(None);
        }
        Ok(Some((final_, opcode, length, offset)))
    }

    fn payload_length(&self, second: u8) -> Result<Option<(usize, usize)>> {
        if second & 0x7f < 126 {
            return Ok(Some((short_length(second), 2)));
        }
        self.extended_length(second & 0x7f)
    }

    fn extended_length(&self, marker: u8) -> Result<Option<(usize, usize)>> {
        if marker == 126 {
            return self.length_16();
        }
        self.length_64()
    }

    fn length_16(&self) -> Result<Option<(usize, usize)>> {
        let Some(bytes) = self.buffer.get(2..4) else {
            return Ok(None);
        };
        let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
        validate_canonical_length(length)?;
        Ok(Some((length, 4)))
    }

    fn length_64(&self) -> Result<Option<(usize, usize)>> {
        let Some(bytes) = self.buffer.get(2..10) else {
            return Ok(None);
        };
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        let length = u64::from_be_bytes(array);
        validate_long_length(length)?;
        Ok(Some((length as usize, 10)))
    }

    fn take_payload(&mut self, length: usize, offset: usize) -> Result<Vec<u8>> {
        let mut payload = self.buffer[offset..offset + length].to_vec();
        if self.masked {
            let mask = self.buffer[offset - 4..offset].to_vec();
            payload = apply_mask(&payload, &mask, self.max_message)?;
        }
        self.buffer.drain(..offset + length);
        Ok(payload)
    }

    fn apply_frame(
        &mut self,
        final_: bool,
        opcode: u8,
        payload: Vec<u8>,
        messages: &mut Vec<(u8, Vec<u8>)>,
    ) -> Result<()> {
        if opcode >= 8 {
            return self.apply_control_frame(opcode, payload, messages);
        }
        self.apply_non_control(final_, opcode, payload, messages)
    }

    fn apply_non_control(
        &mut self,
        final_: bool,
        opcode: u8,
        payload: Vec<u8>,
        messages: &mut Vec<(u8, Vec<u8>)>,
    ) -> Result<()> {
        if opcode == 0 {
            return self.apply_continuation(payload, final_, messages);
        }
        self.apply_data_frame(final_, opcode, payload, messages)
    }

    fn apply_control_frame(
        &mut self,
        opcode: u8,
        payload: Vec<u8>,
        messages: &mut Vec<(u8, Vec<u8>)>,
    ) -> Result<()> {
        validate_control_payload(opcode, &payload)?;
        messages.push((opcode, payload));
        Ok(())
    }

    fn apply_continuation(
        &mut self,
        payload: Vec<u8>,
        final_: bool,
        messages: &mut Vec<(u8, Vec<u8>)>,
    ) -> Result<()> {
        self.require_fragmented_message()?;
        self.fragments.extend_from_slice(&payload);
        self.finish_if_final(final_, messages);
        Ok(())
    }

    fn require_fragmented_message(&self) -> Result<()> {
        if self.fragment_opcode.is_none() {
            return Err(protocol_error("unexpected continuation frame"));
        }
        Ok(())
    }

    fn finish_if_final(&mut self, final_: bool, messages: &mut Vec<(u8, Vec<u8>)>) {
        if final_ {
            messages.push((
                self.fragment_opcode.take().unwrap_or(0),
                std::mem::take(&mut self.fragments),
            ));
        }
    }

    fn apply_data_frame(
        &mut self,
        final_: bool,
        opcode: u8,
        payload: Vec<u8>,
        messages: &mut Vec<(u8, Vec<u8>)>,
    ) -> Result<()> {
        if self.fragment_opcode.is_some() {
            return Err(protocol_error("data frame during fragmented message"));
        }
        self.store_data_frame(final_, opcode, payload, messages);
        Ok(())
    }

    fn store_data_frame(
        &mut self,
        final_: bool,
        opcode: u8,
        payload: Vec<u8>,
        messages: &mut Vec<(u8, Vec<u8>)>,
    ) {
        if final_ {
            messages.push((opcode, payload));
        } else {
            self.fragment_opcode = Some(opcode);
            self.fragments.extend_from_slice(&payload);
        }
    }
}

fn short_length(second: u8) -> usize {
    (second & 0x7f) as usize
}

fn validate_frame_header(first: u8, second: u8, masked: bool) -> Result<()> {
    if !is_supported_header(first) {
        return Err(protocol_error("unsupported WebSocket frame"));
    }
    validate_mask_bit(second, masked)
}

fn is_supported_header(first: u8) -> bool {
    first & 0x70 == 0 && matches!(first & 0x0f, 0 | 1 | 2 | 8 | 9 | 10)
}

fn validate_mask_bit(second: u8, masked: bool) -> Result<()> {
    if (second & 0x80 != 0) != masked {
        return Err(protocol_error("invalid WebSocket masking"));
    }
    Ok(())
}

fn validate_canonical_length(length: usize) -> Result<()> {
    if length < 126 {
        return Err(protocol_error("noncanonical WebSocket length"));
    }
    Ok(())
}

fn validate_long_length(length: u64) -> Result<()> {
    if !(65_536..(1u64 << 63)).contains(&length) {
        return Err(protocol_error("invalid WebSocket length"));
    }
    Ok(())
}

fn validate_frame_constraints(
    final_: bool,
    opcode: u8,
    length: usize,
    fragments: usize,
    max_message: usize,
) -> Result<()> {
    validate_control_frame(final_, opcode, length)?;
    validate_frame_size(opcode, length, fragments, max_message)
}

fn validate_control_frame(final_: bool, opcode: u8, length: usize) -> Result<()> {
    if invalid_control_frame(final_, opcode, length) {
        return Err(protocol_error("invalid WebSocket control frame"));
    }
    Ok(())
}

fn invalid_control_frame(final_: bool, opcode: u8, length: usize) -> bool {
    opcode >= 8 && malformed_control_frame(final_, length)
}

fn malformed_control_frame(final_: bool, length: usize) -> bool {
    !final_ || length > 125
}

fn validate_frame_size(
    opcode: u8,
    length: usize,
    fragments: usize,
    max_message: usize,
) -> Result<()> {
    if exceeds_max_frame(opcode, length, fragments, max_message) {
        return Err(protocol_error(format!("WebSocket message exceeds limit: frame={length}, fragments={fragments}, maximum={max_message}, opcode={opcode}")));
    }
    Ok(())
}

fn exceeds_max_frame(opcode: u8, length: usize, fragments: usize, max_message: usize) -> bool {
    length > max_message || continuation_exceeds(opcode, length, fragments, max_message)
}

fn continuation_exceeds(opcode: u8, length: usize, fragments: usize, max_message: usize) -> bool {
    opcode == 0 && length + fragments > max_message
}

fn masked_offset(offset: usize, masked: bool) -> usize {
    if masked {
        return offset + 4;
    }
    offset
}

fn validate_control_payload(opcode: u8, payload: &[u8]) -> Result<()> {
    if opcode == 8 {
        return validate_close(payload);
    }
    Ok(())
}
