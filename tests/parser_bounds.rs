//! Bounded, offline parser regressions; no service, network or task state.
use herdr_flow::codex::rpc;
use herdr_flow::ErrorKind;

#[test]
fn short_masks_are_rejected_without_panicking() {
    for mask in [&[1][..], &[1, 2][..], &[1, 2, 3][..]] {
        let error = rpc::apply_mask(b"four", mask, 4).unwrap_err();
        assert_eq!(error.kind, ErrorKind::RpcProtocol);
    }
}

#[test]
fn masking_preserves_bounds_and_round_trips() {
    for length in 0..=64 {
        let payload: Vec<_> = (0..length).map(|i| (i * 17) as u8).collect();
        assert_eq!(rpc::apply_mask(&payload, &[], 64).unwrap(), payload);
        for mask in [&[1, 2, 3, 4][..], &[1, 2, 3, 4, 5][..]] {
            let encoded = rpc::apply_mask(&payload, mask, 64).unwrap();
            assert_eq!(rpc::apply_mask(&encoded, mask, 64).unwrap(), payload);
        }
    }
    assert!(rpc::apply_mask(&[], &[1], 0).unwrap().is_empty());
    assert!(rpc::apply_mask(b"xx", &[1], 1)
        .unwrap_err()
        .message
        .contains("maximum size"));
}

#[test]
fn seeded_frame_inputs_are_bounded_and_reproducible() {
    // Fixed xorshift seed and finite cases: at most 512 * 256 input bytes.
    // Each decoder has a 128-byte message budget and gets chunks <= 17 bytes.
    let mut seed = 0x4f52_5043_u32;
    let mut rejected = 0;
    let mut accepted = 0;
    for case in 0..512 {
        let mut bytes = vec![0; case % 257];
        for byte in &mut bytes {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *byte = seed as u8;
        }
        let mut decoder = rpc::FrameDecoder::new(case % 2 == 0, 128);
        let mut failed = false;
        for chunk in bytes.chunks(1 + case % 17) {
            match decoder.feed(chunk) {
                Ok(frames) => assert!(frames.iter().all(|(_, payload)| payload.len() <= 128)),
                Err(error) => {
                    assert_eq!(error.kind, ErrorKind::RpcProtocol, "case {case}");
                    rejected += 1;
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            accepted += 1;
        }
    }
    assert!(rejected > 0);
    assert!(accepted > 0); // Includes incomplete/empty input, not necessarily a message.
}

#[test]
fn valid_frames_survive_every_chunk_boundary() {
    for length in [0, 1, 2, 125, 126, 127, 256] {
        let payload: Vec<_> = (0..length).map(|i| (i * 31) as u8).collect();
        let wire = rpc::encode_frame(&payload, 2, false, true).unwrap();
        for boundary in 0..=wire.len() {
            let mut decoder = rpc::FrameDecoder::new(false, 256);
            let mut messages = decoder.feed(&wire[..boundary]).unwrap();
            messages.extend(decoder.feed(&wire[boundary..]).unwrap());
            assert_eq!(
                messages,
                vec![(2, payload.clone())],
                "length={length} split={boundary}"
            );
        }
    }
}
