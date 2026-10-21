//! The wire format of section 3: length prefixes, the `CastMessage` fields
//! and the 64 KB cap.

use audiosync_cast::framing::{FrameError, FrameReader, MAX_MESSAGE, decode_message, encode_frame};
use audiosync_cast::message::{
    CastMessage, Namespace, PayloadType, ProtocolVersion, SENDER, request_id,
};
use serde_json::json;

fn sample() -> CastMessage {
    CastMessage::json(
        "receiver-0",
        Namespace::Receiver,
        &json!({"type": "GET_STATUS", "requestId": 5}),
    )
}

// covers: C23
#[test]
fn messages_split_across_reads_or_joined_in_one_are_parsed() {
    let one = encode_frame(&sample()).expect("frame");
    let length = u32::from_be_bytes(one[..4].try_into().expect("prefix")) as usize;
    assert_eq!(length, one.len() - 4, "the length counts only the message");

    let mut reader = FrameReader::new();
    for byte in &one[..one.len() - 1] {
        reader.push(std::slice::from_ref(byte));
        assert_eq!(reader.next_message(), Ok(None));
    }
    reader.push(&one[one.len() - 1..]);
    assert_eq!(reader.next_message(), Ok(Some(sample())));
    assert_eq!(reader.buffered(), 0);

    let mut joined = one.clone();
    joined.extend_from_slice(&one);
    joined.extend_from_slice(&one[..3]);
    reader.push(&joined);
    assert_eq!(reader.next_message(), Ok(Some(sample())));
    assert_eq!(reader.next_message(), Ok(Some(sample())));
    assert_eq!(reader.next_message(), Ok(None));
    reader.push(&one[3..]);
    assert_eq!(reader.next_message(), Ok(Some(sample())));
}

// covers: C24
#[test]
fn outbound_messages_decode_as_the_fields_of_the_spec() {
    let frame = encode_frame(&sample()).expect("frame");
    let body = &frame[4..];
    // Field by field in the standard wire format: tag (field << 3 | wire
    // type), then the value.
    let mut fields = Vec::new();
    let mut at = 0;
    while at < body.len() {
        let tag = body[at];
        at += 1;
        let (field, wire) = (tag >> 3, tag & 7);
        match wire {
            0 => {
                fields.push((field, vec![body[at]]));
                at += 1;
            }
            2 => {
                let length = body[at] as usize;
                at += 1;
                fields.push((field, body[at..at + length].to_vec()));
                at += length;
            }
            other => panic!("unexpected wire type {other}"),
        }
    }
    let numbers: Vec<u8> = fields.iter().map(|(field, _)| *field).collect();
    assert_eq!(numbers, [1, 2, 3, 4, 5, 6], "field 7 is absent for STRING");
    assert_eq!(fields[0].1, [0], "CASTV2_1_0");
    assert_eq!(fields[1].1, SENDER.as_bytes());
    assert_eq!(fields[2].1, b"receiver-0");
    assert_eq!(fields[3].1, b"urn:x-cast:com.google.cast.receiver");
    assert_eq!(fields[4].1, [0], "STRING");
    let payload: serde_json::Value = serde_json::from_slice(&fields[5].1).expect("json");
    assert_eq!(payload, json!({"type": "GET_STATUS", "requestId": 5}));

    let decoded = decode_message(body).expect("decodes");
    assert_eq!(decoded.protocol_version, ProtocolVersion::Castv210 as i32);
    assert_eq!(decoded.payload_type, PayloadType::String as i32);
    assert_eq!(decoded.payload_binary, None);
    assert_eq!(request_id(&decoded.json_payload().expect("object")), 5);

    let binary = CastMessage {
        payload_type: PayloadType::Binary as i32,
        payload_utf8: None,
        payload_binary: Some(vec![1, 2, 3]),
        ..sample()
    };
    let frame = encode_frame(&binary).expect("frame");
    assert_eq!(decode_message(&frame[4..]), Ok(binary));
}

// covers: C25
#[test]
fn the_cap_is_64_kb_both_ways_and_a_broken_message_is_a_fault() {
    assert_eq!(MAX_MESSAGE, 65_536);
    let mut reader = FrameReader::new();
    reader.push(&(65_537u32).to_be_bytes());
    assert_eq!(reader.next_message(), Err(FrameError::TooLarge(65_537)));

    let mut reader = FrameReader::new();
    reader.push(&[0, 0, 0, 3, 0xff, 0xff, 0xff]);
    assert_eq!(reader.next_message(), Err(FrameError::Malformed));

    let large = CastMessage {
        payload_utf8: Some("x".repeat(MAX_MESSAGE)),
        ..sample()
    };
    assert!(matches!(
        encode_frame(&large),
        Err(FrameError::TooLarge(length)) if length > MAX_MESSAGE
    ));
}
