#![expect(clippy::unwrap_used, reason = "test")]

mod fixtures;
mod limits;

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::time::Duration;

use crate::EventKind;
use crate::{
    Client, ClientMsg, Envelope, HoldReason, MAX_LINE, ProtoError, ServerMsg, decode, encode,
    explain_events,
};
use fixtures::{client_msgs, event, limit_changed, opened, server_msgs, sid, status};

#[test]
fn round_trips_every_message() {
    for msg in client_msgs() {
        let line = encode(&msg).unwrap();
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(decode::<ClientMsg>(&line).unwrap(), msg);
    }
    for msg in server_msgs() {
        let line = encode(&msg).unwrap();
        assert_eq!(decode::<ServerMsg>(&line).unwrap(), msg);
    }
}

#[test]
fn snapshot_wire_format() {
    let open = ClientMsg::Open {
        cwd: "/work/demo".into(),
        subcommand: "build".into(),
        shim_pid: 100,
        parent_pid: 99,
        nested: false,
    };
    insta::assert_snapshot!("open", String::from_utf8(encode(&open).unwrap()).unwrap());
    let status = ServerMsg::Status(status());
    insta::assert_snapshot!(
        "status",
        String::from_utf8(encode(&status).unwrap()).unwrap()
    );
    let events = ServerMsg::Events {
        events: vec![limit_changed(5)],
        truncated: false,
    };
    insta::assert_snapshot!(
        "events",
        String::from_utf8(encode(&events).unwrap()).unwrap()
    );
}

#[test]
fn rejects_other_version() {
    let err = decode::<ClientMsg>(br#"{"v":2,"type":"status"}"#).unwrap_err();
    assert!(matches!(err, ProtoError::Version { got: 2 }));
    let err = decode::<ClientMsg>(br#"{"v":2,"type":"from_the_future"}"#).unwrap_err();
    assert!(matches!(err, ProtoError::Version { got: 2 }));
}

#[test]
fn rejects_missing_version_and_garbage() {
    assert!(matches!(
        decode::<ClientMsg>(br#"{"type":"status"}"#),
        Err(ProtoError::Json(_))
    ));
    assert!(matches!(
        decode::<ClientMsg>(b"not json"),
        Err(ProtoError::Json(_))
    ));
}

#[test]
fn rejects_long_line() {
    let mut line = br#"{"v":1,"type":"error","message":""#.to_vec();
    line.resize(MAX_LINE + 1, b'a');
    assert!(matches!(
        decode::<ServerMsg>(&line),
        Err(ProtoError::LineTooLong)
    ));
    let too_big = ServerMsg::Error {
        message: "a".repeat(MAX_LINE),
    };
    assert!(matches!(encode(&too_big), Err(ProtoError::LineTooLong)));
}

#[test]
fn envelope_carries_version() {
    let line = encode(&ClientMsg::Status).unwrap();
    let env: Envelope<ClientMsg> = serde_json::from_slice(&line).unwrap();
    assert_eq!(env.v, 1);
}

#[test]
fn explain_events_picks_session_and_limit_changes() {
    let a = sid("a");
    let b = sid("b");
    let all = vec![
        limit_changed(1),
        opened(10, &a),
        event(
            11,
            EventKind::SessionStarted {
                session: b.clone(),
                cargo_pid: 7,
            },
        ),
        limit_changed(12),
        opened(13, &b),
        event(
            20,
            EventKind::SessionClosed {
                session: a.clone(),
                tokens_recovered: 0,
                duration_ms: 10,
            },
        ),
        limit_changed(30),
    ];
    let got = explain_events(&all, &a);
    let stamps: Vec<u64> = got.iter().map(|e| e.ts_ms).collect();
    assert_eq!(stamps, vec![10, 12, 20]);
}

#[test]
fn explain_events_open_session_runs_to_the_end() {
    let a = sid("a");
    let all = vec![limit_changed(1), opened(10, &a), limit_changed(30)];
    let stamps: Vec<u64> = explain_events(&all, &a).iter().map(|e| e.ts_ms).collect();
    assert_eq!(stamps, vec![10, 30]);
}

fn socket_pair() -> (tempfile::TempDir, UnixListener, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let listener = UnixListener::bind(&path).unwrap();
    (dir, listener, path)
}

#[test]
fn client_times_out_on_silent_server() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let err = client.recv(Some(Duration::from_millis(50))).unwrap_err();
    assert!(matches!(err, ProtoError::Timeout));
}

#[test]
fn client_exchanges_messages_and_sees_eof() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    client.send(&ClientMsg::Status).unwrap();
    let mut buf = [0_u8; 64];
    let n = std::io::Read::read(&mut peer, &mut buf).unwrap();
    assert_eq!(decode::<ClientMsg>(&buf[..n]).unwrap(), ClientMsg::Status);
    peer.write_all(
        &encode(&ServerMsg::Held {
            reason: HoldReason::PressureCritical,
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        client.recv(None).unwrap(),
        ServerMsg::Held {
            reason: HoldReason::PressureCritical
        }
    );
    drop(peer);
    let err = client.recv(None).unwrap_err();
    assert!(matches!(err, ProtoError::Closed), "{err:?}");
}

#[test]
fn client_sees_eof_when_the_timeout_changes_after_the_peer_closed() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.write_all(&encode(&ServerMsg::Heartbeat).unwrap())
        .unwrap();
    assert_eq!(client.recv(None).unwrap(), ServerMsg::Heartbeat);
    drop(peer);
    let err = client.recv(Some(Duration::from_millis(50))).unwrap_err();
    assert!(matches!(err, ProtoError::Closed), "{err:?}");
}

#[test]
fn client_delivers_buffered_data_when_the_timeout_changes_after_the_peer_closed() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.write_all(&encode(&ServerMsg::Heartbeat).unwrap())
        .unwrap();
    assert_eq!(client.recv(None).unwrap(), ServerMsg::Heartbeat);
    peer.write_all(&encode(&ServerMsg::Heartbeat).unwrap())
        .unwrap();
    drop(peer);
    assert_eq!(
        client.recv(Some(Duration::from_millis(50))).unwrap(),
        ServerMsg::Heartbeat
    );
    let err = client.recv(Some(Duration::from_millis(50))).unwrap_err();
    assert!(matches!(err, ProtoError::Closed), "{err:?}");
}

#[test]
fn client_never_blocks_when_the_peer_shut_down_only_one_direction() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.write_all(&encode(&ServerMsg::Heartbeat).unwrap())
        .unwrap();
    assert_eq!(client.recv(None).unwrap(), ServerMsg::Heartbeat);
    peer.shutdown(std::net::Shutdown::Read).unwrap();
    let started = std::time::Instant::now();
    let result = client.recv(Some(Duration::from_millis(50)));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        matches!(result, Err(ProtoError::Timeout | ProtoError::Io(_))),
        "{result:?}"
    );
}

#[test]
fn recv_timeout_is_a_deadline_not_a_per_read_timeout() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let trickler = std::thread::spawn(move || {
        for _ in 0..40 {
            if peer.write_all(b"a").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        peer
    });
    let started = std::time::Instant::now();
    let err = client.recv(Some(Duration::from_millis(150))).unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(err, ProtoError::Timeout), "{err:?}");
    assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
    drop(client);
    drop(trickler.join().unwrap());
}

#[test]
fn client_keeps_a_partial_line_across_a_timeout() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let line = encode(&ServerMsg::Heartbeat).unwrap();
    peer.write_all(&line[..5]).unwrap();
    assert!(matches!(
        client.recv(Some(Duration::from_millis(30))),
        Err(ProtoError::Timeout)
    ));
    peer.write_all(&line[5..]).unwrap();
    assert_eq!(
        client.recv(Some(Duration::from_secs(2))).unwrap(),
        ServerMsg::Heartbeat
    );
}

#[test]
fn client_rejects_an_endless_line() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let writer = std::thread::spawn(move || {
        let chunk = vec![b'a'; 8192];
        for _ in 0..16 {
            if peer.write_all(&chunk).is_err() {
                break;
            }
        }
        peer
    });
    let err = client.recv(Some(Duration::from_secs(2))).unwrap_err();
    assert!(matches!(err, ProtoError::LineTooLong));
    assert!(matches!(client.recv(None), Err(ProtoError::Io(_))));
    assert!(matches!(
        client.send(&ClientMsg::Status),
        Err(ProtoError::Io(_))
    ));
    drop(client);
    drop(writer.join().unwrap());
}

#[test]
fn reasons_display_their_wire_wording() {
    assert_eq!(
        HoldReason::PressureCritical.to_string(),
        "memory pressure critical"
    );
    assert_eq!(
        HoldReason::MemoryNearlyFull.to_string(),
        "memory nearly full"
    );
    assert_eq!(crate::LimitReason::SwapGrowing.to_string(), "swap growing");
    assert_eq!(crate::LimitReason::IdleDecay.to_string(), "idle decay");
}
