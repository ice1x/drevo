//! Integration tests for the Bolt TCP listener — Phase 11 task `00070`.
//!
//! The listener binds to an OS-assigned port, accepts one connection,
//! reads the 20-byte handshake, replies with the negotiated version
//! (or `00 00 00 00`), and stays ready to read framed messages on the
//! socket. The codec / framing tests cover the bytes themselves; these
//! tests cover the wiring.

#![cfg(all(not(target_arch = "wasm32"), feature = "http"))]

use drevo::bolt::handshake::{BoltVersion, MAGIC_PREAMBLE};
use drevo::bolt::listener::accept_handshake;
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
