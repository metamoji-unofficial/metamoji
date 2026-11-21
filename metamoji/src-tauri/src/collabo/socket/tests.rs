use super::*;
use crate::collabo::wire::Decoder;

/// Encodes a command the way the writer task would, then reads it back — the
/// two halves of the protocol checked against each other.
fn round_trip(command: &Command) -> Frame {
    let bytes = command.to_frame().encode("C7");
    let mut decoder = Decoder::default();
    decoder.push(&bytes);
    decoder
        .next_frame()
        .expect("within limits")
        .expect("one whole frame")
}

fn push(line: &str) -> Frame {
    let mut decoder = Decoder::default();
    decoder.push(format!("\n{line}\n").as_bytes());
    decoder
        .next_frame()
        .expect("within limits")
        .expect("one whole frame")
}

#[test]
fn login_carries_the_four_identifiers() {
    let frame = round_trip(&Command::Login {
        room_id: "R1".into(),
        device_id: "DEV1".into(),
        session_id: "S1".into(),
        nickname: "山田".into(),
    });
    assert_eq!(frame.command, "LoginRoom");
    assert_eq!(
        frame.booth_id, "*",
        "room-level commands use the `*` channel"
    );
    assert_eq!(frame.get("rid"), Some("R1"));
    // `did` is the collabo device id, not the drive id. Send the drive id and
    // the relay lets you in as a `visitor` who cannot attach a single booth.
    assert_eq!(frame.get("did"), Some("DEV1"));
    assert_eq!(frame.get("sid"), Some("S1"));
    // Base64: parameters are space-separated, and a nickname may contain one.
    assert_eq!(frame.get("name"), Some("5bGx55Sw"));
}

#[test]
fn a_result_reports_success_as_status_zero() {
    // `status` is an error code, not a boolean. Reading it as `"true"` marks
    // every successful login and booth attach as a failure.
    let ok = decode("*\tS1\tcmd:AttachBoothResult bid:b1 status:0");
    assert!(matches!(
        event_for(&ok),
        Some(CollaboEvent::BoothAttached { ok: true, .. })
    ));
    let refused = decode("*\tS1\tcmd:AttachBoothResult bid:b1 status:12");
    assert!(matches!(
        event_for(&refused),
        Some(CollaboEvent::BoothAttached { ok: false, .. })
    ));
}

#[test]
fn an_edit_may_arrive_base64_in_a_parameter_instead_of_as_bytes() {
    // §5: `binaryData` and `data` are exclusive. Reading only the binary path
    // silently drops every edit that came the other way.
    let frame = decode("b1\tS1\tcmd:PostData seq:5 data:AQID");
    match event_for(&frame) {
        Some(CollaboEvent::Direction {
            payload, sequence, ..
        }) => {
            assert_eq!(payload, vec![1, 2, 3]);
            assert_eq!(sequence, 5);
        }
        other => panic!("{other:?}"),
    }
}

fn decode(line: &str) -> Frame {
    let mut decoder = Decoder::default();
    decoder.push(format!("\n{line}\n").as_bytes());
    decoder
        .next_frame()
        .expect("within limits")
        .expect("one whole frame")
}

#[test]
fn post_data_goes_to_its_booth_and_declares_its_length() {
    let frame = round_trip(&Command::PostData {
        booth_id: "booth-3".into(),
        payload: vec![9, 8, 7, 6],
        send_back: true,
        save: false,
        rip_off_size: "0".into(),
    });
    assert_eq!(frame.booth_id, "booth-3");
    // `loop`/`save` are ints on the wire, not booleans.
    assert_eq!(frame.get("loop"), Some("1"));
    assert_eq!(frame.get("save"), Some("0"));
    assert_eq!(frame.get("binary"), Some("4"));
    assert_eq!(frame.payload, vec![9, 8, 7, 6]);
}

#[test]
fn a_payload_containing_newlines_survives_the_round_trip() {
    // Serialised models are arbitrary bytes; this is the case a line-oriented
    // transport would corrupt.
    let payload: Vec<u8> = (0u8..=255).collect();
    let frame = round_trip(&Command::PostData {
        booth_id: "b".into(),
        payload: payload.clone(),
        send_back: false,
        save: true,
        rip_off_size: "0".into(),
    });
    assert_eq!(frame.payload, payload);
}

#[test]
fn ping_result_takes_no_parameters() {
    let frame = round_trip(&Command::PingResult);
    assert_eq!(frame.command, "PingResult");
    assert!(frame.params.is_empty());
}

#[test]
fn a_successful_login_reports_the_role_list() {
    let event = event_for(&push(
        "*\tS1\tcmd:LoginRoomResult status:true rid:R1 uid:U9 rtype:formal role:presenter,speaker",
    ))
    .expect("login result is an event");

    match event {
        CollaboEvent::LoggedIn {
            ok,
            user_id,
            room_type,
            roles,
            ..
        } => {
            assert!(ok);
            assert_eq!(user_id.as_deref(), Some("U9"));
            assert_eq!(room_type.as_deref(), Some("formal"));
            assert_eq!(roles, ["presenter", "speaker"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_refused_login_keeps_the_servers_reason() {
    let event = event_for(&push(
        "*\tS1\tcmd:LoginRoomResult status:false msg:room_closed",
    ))
    .expect("login result is an event");
    match event {
        CollaboEvent::LoggedIn { ok, message, .. } => {
            assert!(!ok);
            assert_eq!(message.as_deref(), Some("room_closed"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_incoming_direction_takes_its_booth_from_the_frame() {
    // `bid` is not a parameter on this push — the channel is the frame's own.
    let mut decoder = Decoder::default();
    decoder.push(b"\nbooth-5\tS2\tcmd:PostData seq:41 uid:U3 self:1 binary:2\nhi");
    let frame = decoder.next_frame().unwrap().unwrap();

    match event_for(&frame).unwrap() {
        CollaboEvent::Direction {
            booth_id,
            sequence,
            user_id,
            own_echo,
            payload,
        } => {
            assert_eq!(booth_id, "booth-5");
            assert_eq!(sequence, 41);
            assert_eq!(user_id.as_deref(), Some("U3"));
            assert!(own_echo, "self:1 marks the relay echoing our own post");
            assert_eq!(payload, b"hi");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn mode_and_role_changes_read_enable_as_the_only_true() {
    match event_for(&push("*\tS1\tcmd:ModeChanged key:presenter value:enable")).unwrap() {
        CollaboEvent::ModeChanged { key, enabled } => {
            assert_eq!(key, "presenter");
            assert!(enabled);
        }
        other => panic!("{other:?}"),
    }
    match event_for(&push("*\tS1\tcmd:RoleChanged key:speaker value:disable")).unwrap() {
        CollaboEvent::RoleChanged { enabled, .. } => assert!(!enabled),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_join_arrives_as_a_room_update() {
    match event_for(&push("*\tS1\tcmd:RoomUpdated key:user value:login uid:U4")).unwrap() {
        CollaboEvent::RoomUpdated {
            key,
            value,
            user_id,
        } => {
            assert_eq!((key.as_str(), value.as_str()), ("user", "login"));
            assert_eq!(user_id.as_deref(), Some("U4"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_pushes_the_original_ignores_produce_no_event() {
    // Recognised on the wire, no-ops in the client (§5). Turning them into
    // events would put noise in front of the UI that it cannot act on.
    for line in [
        "*\tS1\tcmd:OpenConnection",
        "*\tS1\tcmd:PropertyChanged",
        "*\tS1\tcmd:ChangeModeResult status:true",
        "*\tS1\tcmd:LogoutRoomResult status:true",
    ] {
        assert!(event_for(&push(line)).is_none(), "{line}");
    }
}

#[test]
fn ping_is_not_an_event() {
    // It is answered inside the reader; nothing above can use it.
    assert!(event_for(&push("*\tS1\tcmd:Ping")).is_none());
}
