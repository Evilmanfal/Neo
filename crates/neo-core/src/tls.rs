//! QUIC/TLS 1.3 configuration with identity pinning instead of a CA.
//!
//! Both sides present their self-signed identity certificate (mutual TLS).
//! The connecting side checks that the receiver's certificate hashes to the
//! `DeviceId` it meant to reach; the receiving side learns the sender's
//! `DeviceId` from the client certificate. TLS still verifies the handshake
//! signatures, so possession of the private key is proven on both ends and
//! every byte is end-to-end encrypted, including through a future relay.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::identity::{DeviceId, Identity};

pub const ALPN: &[u8] = b"neo/6";
/// What Neo 0.1 speaks. Still negotiated, only so either side can say
/// "update Neo" instead of failing the handshake with a cryptic error.
pub const LEGACY_ALPN: &[u8] = b"neo/5";
pub const SERVER_NAME: &str = "neo";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Congestion control. BBR by default (holds its rate on lossy and long
/// paths where loss-based Cubic backs off); `NEO_CC=cubic` switches back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cc {
    Bbr,
    Cubic,
}

impl Cc {
    pub fn from_env() -> Cc {
        match std::env::var("NEO_CC").map(|v| v.to_ascii_lowercase()) {
            Ok(v) if v == "cubic" => Cc::Cubic,
            _ => Cc::Bbr,
        }
    }
}

/// Flow-control windows sized for LAN speeds while capping per-transfer memory.
pub fn transport() -> Arc<TransportConfig> {
    transport_for(Cc::from_env())
}

pub fn transport_for(cc: Cc) -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    match cc {
        Cc::Bbr => t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default())),
        Cc::Cubic => t.congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default())),
    };
    t.stream_receive_window(VarInt::from_u32(8 << 20));
    t.receive_window(VarInt::from_u32(24 << 20));
    t.send_window(24 << 20);
    t.max_concurrent_uni_streams(VarInt::from_u32(64));
    t.max_concurrent_bidi_streams(VarInt::from_u32(8));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(Duration::from_secs(6).try_into().unwrap()));
    Arc::new(t)
}

pub fn server_config(identity: &Identity) -> Result<quinn::ServerConfig> {
    server_config_alpn(identity, &[ALPN, LEGACY_ALPN])
}

/// Test hook: a server speaking only these protocols (e.g. like Neo 0.1).
#[doc(hidden)]
pub fn server_config_alpn(identity: &Identity, alpn: &[&[u8]]) -> Result<quinn::ServerConfig> {
    let provider = provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyIdentity(provider)))
        .with_single_cert(vec![identity.cert.clone()], identity.key())?;
    tls.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(transport());
    Ok(cfg)
}

/// Client config that only accepts a server whose certificate hashes to `expected`.
pub fn client_config(identity: &Identity, expected: DeviceId) -> Result<quinn::ClientConfig> {
    client_config_alpn(identity, expected, &[ALPN, LEGACY_ALPN])
}

/// Test hook: a client offering only these protocols (e.g. like Neo 0.1).
#[doc(hidden)]
pub fn client_config_alpn(identity: &Identity, expected: DeviceId, alpn: &[&[u8]]) -> Result<quinn::ClientConfig> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { expected, provider }))
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key())?;
    tls.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(transport());
    Ok(cfg)
}

/// True if the other side only speaks an older Neo protocol.
pub fn is_legacy(conn: &quinn::Connection) -> bool {
    conn.handshake_data()
        .and_then(|h| h.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|h| h.protocol)
        .is_some_and(|p| p != ALPN)
}

/// The DeviceId of whoever is on the other end of an established connection.
pub fn peer_id(conn: &quinn::Connection) -> Option<DeviceId> {
    let certs = conn.peer_identity()?.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().map(|c| DeviceId::of_cert(c))
}

#[derive(Debug)]
struct Pinned {
    expected: DeviceId,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if DeviceId::of_cert(end_entity) == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("peer identity does not match".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Accepts any client identity (the sender is identified, then the user decides).
#[derive(Debug)]
struct AnyIdentity(Arc<CryptoProvider>);

impl ClientCertVerifier for AnyIdentity {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
