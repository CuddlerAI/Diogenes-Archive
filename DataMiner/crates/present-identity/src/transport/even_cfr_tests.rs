use super::*;

fn select(schema: u16, required: bool) -> Envelope<Decision> {
    Envelope {
        schema,
        stream: StreamEpoch {
            identity: WireIdentity::new("raw-rgb-test", "r0000").unwrap(),
            generation: 1,
            resource_epoch: 1,
        },
        ordinal: 3,
        payload: Decision::Select {
            session_frame: 1,
            present_seq: 200,
            depth_required: required,
            duplicate: false,
            source_superseded: 0,
            source_qpc: 100,
            callback_start_qpc: 110,
            callback_ready_qpc: 120,
            decision_qpc: 130,
        },
    }
}

#[test]
fn v3_bytes_stay_unchanged_and_cannot_encode_skipped_depth() {
    let message = select(FEEDBACK_SCHEMA_V3, true);
    let bytes = encode_decision(&message).unwrap();
    assert_eq!(bytes[153], 0);
    assert_eq!(decode_decision(&bytes).unwrap(), message);
    assert!(encode_decision(&select(FEEDBACK_SCHEMA_V3, false)).is_err());
    let mut corrupt = bytes;
    corrupt[153] = 1;
    assert!(decode_decision(&corrupt).is_err());
}

#[test]
fn v4_skip_and_required_decisions_and_acks_roundtrip_without_losing_identity() {
    for required in [false, true] {
        let message = select(FEEDBACK_SCHEMA_V4, required);
        let bytes = encode_decision(&message).unwrap();
        assert_eq!(decode_decision(&bytes).unwrap(), message);
        let ack = Envelope {
            schema: FEEDBACK_SCHEMA_V4,
            stream: message.stream,
            ordinal: 3,
            payload: Ack::SelectAccepted {
                decision_ordinal: 3,
                session_frame: 1,
                present_seq: 200,
                depth_required: required,
                depth_already_selected: false,
            },
        };
        let bytes = encode_ack(&ack).unwrap();
        assert_eq!(decode_ack(&bytes).unwrap(), ack);
        let mut corrupt = bytes;
        corrupt[161] = 2;
        assert!(decode_ack(&corrupt).is_err());
    }
}
