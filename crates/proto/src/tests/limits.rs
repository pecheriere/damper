//! Replies longer than `MAX_LINE`: `MAX_REPLY`, `encode_with_limit`,
//! `decode_with_limit` and `Client::recv_with_limit`.

use std::io::Write;
use std::time::Duration;

use super::socket_pair;
use crate::{
    Client, Event, EventKind, MAX_LINE, MAX_REPLY, ProtoError, ServerMsg, decode,
    decode_with_limit, encode, encode_with_limit,
};

/// An `Events` reply whose line is longer than `MAX_LINE`.
fn big_events() -> ServerMsg {
    let events = (0..1000)
        .map(|i| Event {
            ts_ms: i,
            kind: EventKind::Error {
                message: "x".repeat(100),
            },
        })
        .collect();
    ServerMsg::Events {
        events,
        truncated: false,
    }
}

#[test]
fn max_reply_is_sixteen_mebibytes() {
    assert_eq!(MAX_REPLY, 16 * 1024 * 1024);
}

#[test]
fn a_long_reply_encodes_and_decodes_within_its_own_limit() {
    let msg = big_events();
    assert!(matches!(encode(&msg), Err(ProtoError::LineTooLong)));
    let line = encode_with_limit(&msg, MAX_REPLY).unwrap();
    assert!(line.len() > MAX_LINE);
    assert!(matches!(
        decode::<ServerMsg>(&line),
        Err(ProtoError::LineTooLong)
    ));
    assert_eq!(
        decode_with_limit::<ServerMsg>(&line, MAX_REPLY).unwrap(),
        msg
    );
    assert!(matches!(
        encode_with_limit(&msg, line.len() - 1),
        Err(ProtoError::LineTooLong)
    ));
}

#[test]
fn recv_with_limit_takes_a_reply_longer_than_max_line() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let msg = big_events();
    let line = encode_with_limit(&msg, MAX_REPLY).unwrap();
    let writer = std::thread::spawn(move || {
        peer.write_all(&line).unwrap();
        peer
    });
    let got = client
        .recv_with_limit(Some(Duration::from_secs(5)), MAX_REPLY)
        .unwrap();
    assert_eq!(got, msg);
    drop(writer.join().unwrap());
}

#[test]
fn recv_keeps_max_line_for_the_shim() {
    let (_dir, listener, path) = socket_pair();
    let mut client = Client::connect(&path, Duration::from_millis(200)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let line = encode_with_limit(&big_events(), MAX_REPLY).unwrap();
    let writer = std::thread::spawn(move || {
        // The client stops reading at MAX_LINE and closes.
        let _ = peer.write_all(&line);
        peer
    });
    let err = client.recv(Some(Duration::from_secs(5))).unwrap_err();
    assert!(matches!(err, ProtoError::LineTooLong), "{err:?}");
    drop(client);
    drop(writer.join().unwrap());
}
