//! Integration tests for the Bolt TCP listener — Phase 11 task `00070`.
//!
//! The listener binds to an OS-assigned port, accepts one connection,
//! reads the 20-byte handshake, replies with the negotiated version
//! (or `00 00 00 00`), and stays ready to read framed messages on the
//! socket. The codec / framing tests cover the bytes themselves; these
//! tests cover the wiring.

#![cfg(all(
    not(target_arch = "wasm32"),
    feature = "http",
    feature = "redb-backend"
))]

use drevo::bolt::handshake::{BoltVersion, MAGIC_PREAMBLE};
use drevo::bolt::listener::{accept_and_run_session, accept_handshake};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

async fn bind_loopback() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.expect("bind")
}

#[tokio::test]
async fn handshake_negotiates_bolt_4_4_when_offered() {
    let listener = bind_loopback().await;
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        accept_handshake(socket).await
    });

    let mut client = TcpStream::connect(addr).await.expect("connect");
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&MAGIC_PREAMBLE);
    payload.extend_from_slice(&[0x00, 0x00, 0x04, 0x04]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    client.write_all(&payload).await.unwrap();

    let mut reply = [0u8; 4];
    timeout(Duration::from_secs(2), client.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, [0x00, 0x00, 0x04, 0x04]);

    let result = server.await.unwrap().unwrap();
    assert_eq!(result.negotiated, Some(BoltVersion { major: 4, minor: 4 }));
}

#[tokio::test]
async fn handshake_replies_with_zeros_when_no_version_matches() {
    let listener = bind_loopback().await;
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        accept_handshake(socket).await
    });

    let mut client = TcpStream::connect(addr).await.expect("connect");
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&MAGIC_PREAMBLE);
    // Offer four versions drevo does NOT support.
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x05]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x06]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x07]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x08]);
    client.write_all(&payload).await.unwrap();

    let mut reply = [0u8; 4];
    timeout(Duration::from_secs(2), client.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, [0x00, 0x00, 0x00, 0x00]);

    let result = server.await.unwrap().unwrap();
    assert_eq!(result.negotiated, None);
}

#[tokio::test]
async fn accept_and_run_session_handles_hello_run_pull_goodbye_end_to_end() {
    use drevo::bolt::packstream::{decode, encode, Value};
    use drevo::bolt::session::{ClientMessage, GOODBYE, HELLO, PULL, RUN};
    use drevo::db::Drevo;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let listener = bind_loopback().await;
    let addr = listener.local_addr().unwrap();
    let drevo = Arc::new(Drevo::open_in_memory().expect("open"));
    let drevo_for_server = Arc::clone(&drevo);

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        accept_and_run_session(socket, &drevo_for_server).await
    });

    let mut client = TcpStream::connect(addr).await.expect("connect");
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&MAGIC_PREAMBLE);
    payload.extend_from_slice(&[0x00, 0x00, 0x04, 0x04]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    client.write_all(&payload).await.unwrap();

    let mut version_reply = [0u8; 4];
    timeout(
        Duration::from_secs(2),
        client.read_exact(&mut version_reply),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(version_reply, [0x00, 0x00, 0x04, 0x04]);

    fn frame(msg: &ClientMessage) -> Vec<u8> {
        let value = match msg {
            ClientMessage::Hello { extra } => Value::Structure {
                tag: HELLO,
                fields: vec![Value::Dictionary(extra.clone())],
            },
            ClientMessage::Run {
                query,
                parameters,
                extra,
            } => Value::Structure {
                tag: RUN,
                fields: vec![
                    Value::String(query.clone()),
                    Value::Dictionary(parameters.clone()),
                    Value::Dictionary(extra.clone()),
                ],
            },
            ClientMessage::Pull { extra } => Value::Structure {
                tag: PULL,
                fields: vec![Value::Dictionary(extra.clone())],
            },
            ClientMessage::Goodbye => Value::Structure {
                tag: GOODBYE,
                fields: vec![],
            },
            _ => panic!("unused"),
        };
        let mut buf = Vec::new();
        encode(&value, &mut buf).unwrap();
        let len = buf.len() as u16;
        let mut framed = Vec::with_capacity(buf.len() + 4);
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(&buf);
        framed.extend_from_slice(&[0x00, 0x00]);
        framed
    }

    let empty: BTreeMap<String, Value> = BTreeMap::new();
    client
        .write_all(&frame(&ClientMessage::Hello {
            extra: empty.clone(),
        }))
        .await
        .unwrap();
    client
        .write_all(&frame(&ClientMessage::Run {
            query: "RETURN 1 AS n".to_string(),
            parameters: empty.clone(),
            extra: empty.clone(),
        }))
        .await
        .unwrap();
    let mut pull_extra: BTreeMap<String, Value> = BTreeMap::new();
    pull_extra.insert("n".to_string(), Value::Integer(-1));
    client
        .write_all(&frame(&ClientMessage::Pull { extra: pull_extra }))
        .await
        .unwrap();
    client
        .write_all(&frame(&ClientMessage::Goodbye))
        .await
        .unwrap();

    async fn read_one(client: &mut TcpStream) -> Value {
        let mut payload = Vec::new();
        loop {
            let mut len = [0u8; 2];
            client.read_exact(&mut len).await.unwrap();
            let l = u16::from_be_bytes(len) as usize;
            if l == 0 {
                break;
            }
            let start = payload.len();
            payload.resize(start + l, 0);
            client.read_exact(&mut payload[start..]).await.unwrap();
        }
        let (v, rest) = decode(&payload).unwrap();
        assert!(rest.is_empty());
        v
    }

    // HELLO ack
    let v = timeout(Duration::from_secs(2), read_one(&mut client))
        .await
        .unwrap();
    assert!(matches!(v, Value::Structure { tag, .. } if tag == 0x70));
    // RUN ack
    let v = timeout(Duration::from_secs(2), read_one(&mut client))
        .await
        .unwrap();
    assert!(matches!(v, Value::Structure { tag, .. } if tag == 0x70));
    // PULL → first RECORD
    let v = timeout(Duration::from_secs(2), read_one(&mut client))
        .await
        .unwrap();
    assert!(matches!(v, Value::Structure { tag, .. } if tag == 0x71));
    // PULL → terminal SUCCESS
    let v = timeout(Duration::from_secs(2), read_one(&mut client))
        .await
        .unwrap();
    assert!(matches!(v, Value::Structure { tag, .. } if tag == 0x70));

    // GOODBYE — server returns no reply and closes; allow either Ok or
    // a peer-closed read attempt below.
    drop(client);
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _ = drevo;
}

#[tokio::test]
async fn accept_and_run_session_returns_when_handshake_rejects_all_versions() {
    use drevo::db::Drevo;
    use std::sync::Arc;

    let listener = bind_loopback().await;
    let addr = listener.local_addr().unwrap();
    let drevo = Arc::new(Drevo::open_in_memory().expect("open"));
    let drevo_for_server = Arc::clone(&drevo);

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        accept_and_run_session(socket, &drevo_for_server).await
    });

    let mut client = TcpStream::connect(addr).await.expect("connect");
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&MAGIC_PREAMBLE);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x09]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    client.write_all(&payload).await.unwrap();

    let mut reply = [0u8; 4];
    timeout(Duration::from_secs(2), client.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, [0x00, 0x00, 0x00, 0x00]);

    let r = timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert!(r.is_ok());
    let _ = drevo;
}

#[tokio::test]
async fn handshake_errors_on_bad_magic_preamble() {
    let listener = bind_loopback().await;
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        accept_handshake(socket).await
    });

    let mut client = TcpStream::connect(addr).await.expect("connect");
    let mut payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    payload.extend_from_slice(&[0; 16]);
    client.write_all(&payload).await.unwrap();
    drop(client);

    let result = timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err(), "expected handshake to fail on bad magic");
}
