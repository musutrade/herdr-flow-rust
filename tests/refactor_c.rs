//! Offline regression coverage for the refactored RPC frame helpers.

use herdr_flow::codex::rpc::{self, FrameDecoder};

#[test]
fn close_validation_matches_rfc_boundaries() {
    assert!(rpc::validate_close(&[]).is_ok());
    assert!(rpc::validate_close(&[0x03]).is_err());
    for code in [999u16, 1004, 1005, 1006, 1015, 1016, 2999, 5000, 65535] {
        assert!(
            rpc::validate_close(&code.to_be_bytes()).is_err(),
            "code {code} must be rejected"
        );
    }
    for code in [1000u16, 1001, 1003, 1007, 1014, 3000, 4999] {
        assert!(
            rpc::validate_close(&code.to_be_bytes()).is_ok(),
            "code {code} must be accepted"
        );
    }
    let mut with_reason = 1000u16.to_be_bytes().to_vec();
    with_reason.extend_from_slice(b"bye");
    assert!(rpc::validate_close(&with_reason).is_ok());
    let mut bad_reason = 1000u16.to_be_bytes().to_vec();
    bad_reason.push(0xff);
    assert!(rpc::validate_close(&bad_reason).is_err());
}

#[test]
fn decoder_reassembles_fragments_across_partial_reads() {
    let mut wire = rpc::encode_frame(b"hel", 1, false, false).unwrap();
    wire.extend(rpc::encode_frame(b"lo", 0, false, true).unwrap());
    let mut decoder = FrameDecoder::new(false, 1024);
    let mut messages = Vec::new();
    for byte in &wire {
        messages.extend(decoder.feed(std::slice::from_ref(byte)).unwrap());
    }
    assert_eq!(messages, vec![(1, b"hello".to_vec())]);
}

#[test]
fn decoder_validates_control_payloads() {
    let mut payload = 1000u16.to_be_bytes().to_vec();
    payload.extend_from_slice(b"bye");
    let frame = rpc::encode_frame(&payload, 8, false, true).unwrap();
    let mut decoder = FrameDecoder::new(false, 1024);
    assert_eq!(decoder.feed(&frame).unwrap(), vec![(8, payload)]);

    let invalid = rpc::encode_frame(&1005u16.to_be_bytes(), 8, false, true).unwrap();
    let mut decoder = FrameDecoder::new(false, 1024);
    assert!(decoder.feed(&invalid).is_err());

    let ping = rpc::encode_frame(b"x", 9, false, true).unwrap();
    let mut decoder = FrameDecoder::new(false, 1024);
    assert_eq!(decoder.feed(&ping).unwrap(), vec![(9, b"x".to_vec())]);
}

#[test]
fn decoder_handles_masked_and_extended_length_frames() {
    let mut masked = FrameDecoder::new(true, 128);
    let frame = rpc::encode_frame(b"hello", 1, true, true).unwrap();
    assert_eq!(masked.feed(&frame).unwrap(), vec![(1, b"hello".to_vec())]);

    let mut extended = FrameDecoder::new(false, 128);
    let payload = vec![7u8; 126];
    let frame = rpc::encode_frame(&payload, 2, false, true).unwrap();
    assert_eq!(extended.feed(&frame).unwrap(), vec![(2, payload)]);

    let mut huge = FrameDecoder::new(false, 70_000);
    let payload = vec![9u8; 65_536];
    let frame = rpc::encode_frame(&payload, 2, false, true).unwrap();
    assert_eq!(huge.feed(&frame).unwrap(), vec![(2, payload)]);
}

#[test]
fn decoder_rejects_noncanonical_and_invalid_extended_lengths() {
    let mut decoder = FrameDecoder::new(false, 1024);
    assert!(decoder.feed(&[0x82, 126, 0, 125]).is_err());

    let mut frame = vec![0x82, 127];
    frame.extend_from_slice(&65_535u64.to_be_bytes());
    let mut decoder = FrameDecoder::new(false, 1024);
    assert!(decoder.feed(&frame).is_err());
}
