//! QUIC endpoints and TLS 1.3 with self-signed identities.
//!
//! Certificates aren't checked against any authority: both sides present one
//! (client auth is mandatory), TLS proves each holds its key, and the
//! certificate's fingerprint is the peer id ([`crate::identity::peer_id`]).
//! Who may talk to whom is decided above this layer (group proofs, and the
//! broker's pinned fingerprint).

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, ServerConfig, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::identity::{self, Identity};
use crate::wire::{PeerId, VERSION};

/// The server name clients ask for; certificates aren't checked against it.
pub const SERVER_NAME: &str = "discostu";
pub const BROKER_ALPN: &[u8] = b"discostu-broker/1";
pub const DEFAULT_BROKER_PORT: u16 = 47900;

/// Peer ALPN carries the protocol version: mismatched builds fail the
/// handshake instead of misreading each other.
pub fn peer_alpn() -> Vec<u8> {
    format!("discostu/{VERSION}").into_bytes()
}

/// Exported keying material label for group proofs.
pub const PROOF_LABEL: &[u8] = b"EXPORTER-discostu-group-proof";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts any certificate but checks the handshake signature, so the peer
/// provably holds the key behind the certificate it showed.
#[derive(Debug)]
struct AnyCert(WebPkiSupportedAlgorithms);

impl AnyCert {
    fn new() -> Arc<Self> {
        Arc::new(Self(provider().signature_verification_algorithms))
    }
}

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
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
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

impl ClientCertVerifier for AnyCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
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
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

pub fn server_config(id: &Identity, alpn: Vec<u8>, transport: TransportConfig) -> ServerConfig {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_client_cert_verifier(AnyCert::new())
        .with_single_cert(vec![id.cert.clone()], id.key())
        .expect("own certificate");
    tls.alpn_protocols = vec![alpn];
    let mut cfg = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls).expect("QUIC TLS")));
    cfg.transport_config(Arc::new(transport));
    cfg
}

pub fn client_config(id: &Identity, alpn: Vec<u8>, transport: TransportConfig) -> ClientConfig {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(AnyCert::new())
        .with_client_auth_cert(vec![id.cert.clone()], id.key())
        .expect("own certificate");
    tls.alpn_protocols = vec![alpn];
    let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).expect("QUIC TLS")));
    cfg.transport_config(Arc::new(transport));
    cfg
}

/// Peer links: voice datagrams plus control, screen video on a separate
/// connection. Keep-alives every second, dead after six silent seconds.
pub fn peer_transport() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(VarInt::from_u32(6_000).into()));
    // Faster handshake retries: hole punching needs a few attempts while
    // both NATs open up.
    t.initial_rtt(Duration::from_millis(100));
    // Screen video at LAN bitrates (up to 80 Mbit/s) over tens of ms of RTT.
    t.stream_receive_window(VarInt::from_u32(16 << 20));
    t.receive_window(VarInt::from_u32(32 << 20));
    // But little unsent data on our side: when the path can't keep up,
    // writes must block soon, so the sharer drops frames and resyncs on a
    // keyframe instead of queueing seconds of video.
    t.send_window(1 << 20);
    t.datagram_receive_buffer_size(Some(1 << 20));
    t.datagram_send_buffer_size(1 << 20);
    t
}

/// To and from the broker: Fly's UDP proxy leaves about 1300 bytes per
/// packet, so no path MTU probing past QUIC's 1200 minimum.
pub fn broker_transport() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(VarInt::from_u32(20_000).into()));
    t.mtu_discovery_config(None);
    t
}

/// The certificate the other side presented.
pub fn remote_cert(conn: &quinn::Connection) -> Option<CertificateDer<'static>> {
    let certs = conn.peer_identity()?.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().cloned()
}

pub fn remote_id(conn: &quinn::Connection) -> Option<PeerId> {
    remote_cert(conn).map(|c| identity::peer_id(&c))
}

/// Keying material both ends of `conn` share and nobody else can know.
pub fn session_secret(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, PROOF_LABEL, b"").ok()?;
    Some(out)
}
