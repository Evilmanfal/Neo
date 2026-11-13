//! The real punch runner on real sockets (loopback, so no NAT in the way):
//! probes are answered and reported, the birthday path hands back the
//! winning extra socket, and QUIC then comes up over exactly that socket.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use neo_core::identity::Identity;
use neo_core::net::punch::{self, Found, Plan};
use neo_core::net::socket::NeoSocket;

fn bind() -> Arc<NeoSocket> {
    NeoSocket::bind(&quinn::TokioRuntime, 0).unwrap()
}

/// In the app the main socket always has a QUIC endpoint reading it (that's
/// where probes get intercepted); give test sockets one too.
fn endpoint(s: &Arc<NeoSocket>) -> quinn::Endpoint {
    quinn::Endpoint::new_with_abstract_socket(quinn::EndpointConfig::default(), None, s.clone(), Arc::new(quinn::TokioRuntime))
        .unwrap()
}

fn local(s: &NeoSocket) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], s.port()))
}

#[tokio::test(flavor = "multi_thread")]
async fn probes_on_the_main_socket_find_each_other() {
    let (a, b) = (bind(), bind());
    let _endpoints = (endpoint(&a), endpoint(&b));
    let token = [3u8; 8];
    let a_plan = Plan { base: vec![local(&b)], ..Default::default() };
    let b_plan = Plan { base: vec![local(&a)], ..Default::default() };
    let (ra, rb) = tokio::join!(
        punch::run(&a, token, &a_plan, Duration::from_secs(5)),
        punch::run(&b, token, &b_plan, Duration::from_secs(5)),
    );
    assert!(matches!(ra, Some(Found::Main(addr)) if addr.port() == b.port()), "{ra:?}");
    assert!(matches!(rb, Some(Found::Main(addr)) if addr.port() == a.port()), "{rb:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_tokens_are_ignored_and_sessions_end_on_time() {
    let (a, b) = (bind(), bind());
    let _endpoints = (endpoint(&a), endpoint(&b));
    let a_plan = Plan { base: vec![local(&b)], ..Default::default() };
    let empty = Plan::default();
    let started = std::time::Instant::now();
    let (ra, rb) = tokio::join!(
        punch::run(&a, [1; 8], &a_plan, Duration::from_secs(2)),
        punch::run(&b, [2; 8], &empty, Duration::from_secs(2)),
    );
    assert!(ra.is_none() && rb.is_none());
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn birthday_socket_wins_and_carries_quic() {
    let dir = std::env::temp_dir().join(format!("neo-punch-test-{}", std::process::id()));
    let (server_id, client_id) = (Identity::load_or_create(&dir.join("s")).unwrap(), Identity::load_or_create(&dir.join("c")).unwrap());

    // B: an ordinary Neo endpoint on its main socket.
    let b = bind();
    let b_endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(neo_core::tls::server_config(&server_id).unwrap()),
        b.clone(),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    let accept = tokio::spawn(async move {
        let conn = b_endpoint.accept().await.unwrap().await.unwrap();
        let mut s = conn.accept_uni().await.unwrap();
        s.read_to_end(64).await.unwrap()
    });

    // A: only extra (birthday) sockets aim at B; B just listens.
    let a = bind();
    let token = [9u8; 8];
    let a_plan = Plan { extras: 16, extra_targets: vec![local(&b)], ..Default::default() };
    let listen = Plan::default();
    let (ra, _rb) = tokio::join!(
        punch::run(&a, token, &a_plan, Duration::from_secs(8)),
        punch::run(&b, token, &listen, Duration::from_secs(8)),
    );
    let Some(Found::Extra(sock, addr)) = ra else { panic!("expected a birthday socket, got {ra:?}") };
    assert_eq!(addr.port(), b.port());

    // QUIC over the winning socket, to B's main socket.
    let client = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, sock, Arc::new(quinn::TokioRuntime)).unwrap();
    let cfg = neo_core::tls::client_config(&client_id, server_id.id).unwrap();
    let conn = client.connect_with(cfg, local(&b), neo_core::tls::SERVER_NAME).unwrap().await.unwrap();
    let mut s = conn.open_uni().await.unwrap();
    s.write_all(b"through the punched hole").await.unwrap();
    s.finish().unwrap();
    assert_eq!(accept.await.unwrap(), b"through the punched hole");
    let _ = std::fs::remove_dir_all(&dir);
}
