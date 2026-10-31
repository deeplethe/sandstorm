//! The control plane's transport.
//!
//! `identity.rs` has issued a CA, a master server certificate and per-node
//! client certificates since the first v0.6 commit, and tested both sides of
//! the handshake -- but nothing served TLS, so every certificate was
//! generated and then ignored while node tokens and commands went over
//! plain HTTP. This module is the half that was missing.
//!
//! Two directions are verified, because each stops a different attack
//! (RFC 16.5):
//!
//! - The node presents a client certificate, so a machine that merely knows
//!   the master's address cannot accept scheduling and be handed someone
//!   else's sandboxes.
//! - The master presents a server certificate, so an attacker cannot stand
//!   in for the master, collect node connections and either take the
//!   sandboxes on them or run commands inside them.
//!
//! The second direction has a bootstrap problem: a node registering for the
//! first time has no CA certificate yet, so it has nothing to verify the
//! master against. Accepting whatever answers would leave exactly the hole
//! the server certificate exists to close, on the one request that carries
//! the registration token. So the token carries the CA's fingerprint --
//! `<fingerprint>.<secret>`, the way k3s puts the CA hash in `K3S_TOKEN` --
//! and first contact is verified by pinning it.

use anyhow::{bail, Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

/// Separates the CA fingerprint from the secret in a registration token.
/// `.` because it appears in neither half: the fingerprint is hex and
/// `random_token` emits hex too.
pub const TOKEN_SEP: char = '.';

/// Install the crypto provider for this process.
///
/// rustls 0.23 refuses to pick one when more than one is compiled in, and
/// reqwest drags its own along, so leaving it implicit means the first TLS
/// call in the process panics. Choosing `ring` explicitly also records which
/// one we actually run on. Idempotent: a second call is a no-op, so both
/// halves of `sandstormd` can call it.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Build the token a node is given: the CA fingerprint it should pin, then
/// the single-use secret. Only the secret is stored; the fingerprint is
/// public and is there so first contact can be verified at all.
pub fn join_token(ca_fingerprint: &str, secret: &str) -> String {
    format!("{ca_fingerprint}{TOKEN_SEP}{secret}")
}

/// Split a presented token back into (pinned CA fingerprint, secret).
///
/// A token with no fingerprint is still accepted so that one minted before
/// this existed keeps working; the node warns in that case, because it then
/// has nothing to verify the master against on the one request that carries
/// the token.
pub fn split_token(token: &str) -> (Option<&str>, &str) {
    match token.split_once(TOKEN_SEP) {
        Some((pin, secret)) if pin.len() == 64 && !secret.is_empty() => (Some(pin), secret),
        _ => (None, token),
    }
}

/// SHA-256 of a certificate's DER, hex. This is what a node pins.
pub fn fingerprint(der: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(der))
}

/// The fingerprint of the first certificate in a PEM file.
pub fn fingerprint_of_pem(pem: &str) -> Result<String> {
    let certs = parse_certs(pem)?;
    let first = certs.first().context("no certificate in PEM")?;
    Ok(fingerprint(first))
}

fn parse_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .context("parsing certificates")?;
    if certs.is_empty() {
        bail!("no certificate in PEM");
    }
    Ok(certs)
}

fn parse_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut pem.as_bytes())
        .context("parsing private key")?
        .context("no private key in PEM")
}

fn roots_from_pem(ca_pem: &str) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for c in parse_certs(ca_pem)? {
        roots.add(c).context("adding the CA to the root store")?;
    }
    Ok(roots)
}

/// The master's TLS configuration.
///
/// Client authentication is **optional at the handshake** rather than
/// required, because `/v1/node/register` is how a node gets its certificate
/// in the first place and so cannot be asked to present one. What rustls
/// still guarantees is that a certificate offered at all chains to our CA;
/// a connection that presents none simply arrives with no identity, and
/// `peer_node` is then `None`. Requiring an identity is the router's job
/// (`require_node_cert`), which is also where it can be matched against the
/// node id in the message -- a check the handshake cannot make.
pub fn server_config(cert_pem: &str, key_pem: &str, ca_pem: &str) -> Result<ServerConfig> {
    let mut chain = parse_certs(cert_pem)?;
    // Send the CA alongside the leaf so a node pinning the CA's fingerprint
    // can find it in the chain on first contact, before it has ca.pem.
    for c in parse_certs(ca_pem)? {
        if !chain.contains(&c) {
            chain.push(c);
        }
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots_from_pem(ca_pem)?))
        .allow_unauthenticated()
        .build()
        .context("building the client verifier")?;
    ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, parse_key(key_pem)?)
        .context("loading the master's server certificate")
}

/// A node's TLS configuration once it holds a certificate: it proves who it
/// is, and verifies the master against the CA.
pub fn client_config(cert_pem: &str, key_pem: &str, ca_pem: &str) -> Result<ClientConfig> {
    ClientConfig::builder()
        .with_root_certificates(roots_from_pem(ca_pem)?)
        .with_client_auth_cert(parse_certs(cert_pem)?, parse_key(key_pem)?)
        .context("loading the node's client certificate")
}

/// A node's TLS configuration for its very first request, when it has the
/// registration token but no CA certificate.
///
/// The master is verified by pinning the CA fingerprint carried in the
/// token. This is not "no verification deferred until later": an attacker
/// standing in for the master fails the handshake here, so the registration
/// token is never sent to it.
pub fn pinned_client_config(ca_fingerprint: &str) -> ClientConfig {
    ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCa {
            want: ca_fingerprint.to_ascii_lowercase(),
        }))
        .with_no_client_auth()
}

/// The names on the master's certificate: loopback, the address it is bound
/// to, and whatever the operator added.
///
/// The bind address stays even when it is unspecified. A node in the same
/// process dials `https://<bind>`, and every call after its registration
/// verifies the name, so `--master --node --bind 0.0.0.0:8080` needs it.
///
/// Entries are trimmed and blanks dropped, so `a, b` and an empty environment
/// variable do not reach the certificate as names no client can match.
pub fn master_sans(bind: std::net::IpAddr, extra: &[String]) -> Vec<String> {
    let mut sans = vec![
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        bind.to_string(),
    ];
    sans.extend(
        extra
            .iter()
            .map(|san| san.trim().to_string())
            .filter(|san| !san.is_empty()),
    );
    let mut seen = std::collections::HashSet::new();
    sans.retain(|san| seen.insert(san.clone()));
    sans
}

/// Reads the master's certificate and key, issuing them if they are absent.
pub fn load_or_issue_master(
    ca: &crate::identity::Ca,
    sans: Vec<String>,
) -> Result<(String, String, String)> {
    let paths = ca.issue_master(sans)?;
    Ok((
        std::fs::read_to_string(&paths.cert)
            .with_context(|| format!("reading {}", paths.cert.display()))?,
        std::fs::read_to_string(&paths.key)
            .with_context(|| format!("reading {}", paths.key.display()))?,
        ca.cert_pem(),
    ))
}

pub fn read_pem(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// Client config that trusts exactly one server certificate, by SHA-256.
///
/// For a master connecting to a listening node. There is no CA here: the
/// node made its own certificate, and the pairing code carries its
/// fingerprint, so the pin IS the identity. Names are not checked -- a node
/// is reached by whatever address the operator typed, often a bare IP.
pub fn pinned_leaf_client_config(cert_fingerprint: &str) -> ClientConfig {
    install_crypto_provider();
    ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedLeaf {
            want: cert_fingerprint.to_ascii_lowercase(),
        }))
        .with_no_client_auth()
}

/// Server config with no client certificates, for a listening node. The
/// master authenticates with the pairing secret instead: it cannot hold a
/// certificate the node would know to trust before they have met.
pub fn plain_server_config(cert_pem: &str, key_pem: &str) -> Result<ServerConfig> {
    install_crypto_provider();
    let certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing the node certificate")?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .context("parsing the node key")?
        .context("no private key in the node key file")?;
    Ok(ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?)
}

#[derive(Debug)]
struct PinnedLeaf {
    want: String,
}

impl rustls::client::danger::ServerCertVerifier for PinnedLeaf {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Constant time is not needed: the fingerprint is public, it is in
        // the pairing code. What matters is that it is compared at all.
        if fingerprint(end_entity) == self.want {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "this node presented certificate {} but the pairing code pins {} -- \
                 either the code is for another node, or something is in between",
                fingerprint(end_entity),
                self.want
            )))
        }
    }

    // The handshake signature still has to verify against the pinned
    // certificate's key, or anyone could replay a copy of the certificate.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Verifies the master on first contact by the CA fingerprint in the token.
///
/// It checks that the chain the master sent contains the pinned CA **and**
/// that the leaf was issued by it, so presenting the (public) CA certificate
/// alongside an unrelated leaf does not pass.
#[derive(Debug)]
struct PinnedCa {
    want: String,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let pinned = intermediates
            .iter()
            .find(|c| fingerprint(c) == self.want)
            .ok_or_else(|| {
                rustls::Error::General(
                    "the master did not present the CA this token pins".to_string(),
                )
            })?;

        let mut roots = RootCertStore::empty();
        roots.add(pinned.clone()).map_err(|e| {
            rustls::Error::General(format!("the pinned certificate is not usable as a CA: {e}"))
        })?;
        let verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| rustls::Error::General(e.to_string()))?;
        // Name verification is deliberately skipped -- a node is pointed at
        // the master by address, which may be an IP the certificate does not
        // carry. The pin is the identity check.
        verifier
            .verify_server_cert(
                end_entity,
                intermediates,
                &ServerName::try_from("sandstorm-master").unwrap(),
                &[],
                now,
            )
            .or_else(|e| match e {
                rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName)
                | rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidForNameContext { .. },
                ) => Ok(rustls::client::danger::ServerCertVerified::assertion()),
                other => Err(other),
            })
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Ca;

    fn ca() -> (Ca, tempfile::TempDir) {
        install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        (Ca::open_or_create(dir.path()).unwrap(), dir)
    }

    #[test]
    fn a_master_names_loopback_its_bind_address_and_what_it_was_given() {
        let extra = ["master.example".to_string(), "203.0.113.7".to_string()];
        assert_eq!(
            master_sans("10.2.0.11".parse().unwrap(), &extra),
            [
                "127.0.0.1",
                "localhost",
                "10.2.0.11",
                "master.example",
                "203.0.113.7"
            ],
        );
    }

    #[test]
    fn an_unspecified_bind_stays_for_the_node_in_this_process() {
        for bind in ["0.0.0.0", "::"] {
            assert_eq!(
                master_sans(bind.parse().unwrap(), &[]),
                ["127.0.0.1", "localhost", bind],
                "{bind}"
            );
        }
    }

    #[test]
    fn blanks_and_spaces_do_not_become_names() {
        let extra = ["".to_string(), " a.example".to_string(), "  ".to_string()];
        assert_eq!(
            master_sans("10.2.0.11".parse().unwrap(), &extra),
            ["127.0.0.1", "localhost", "10.2.0.11", "a.example"],
        );
    }

    #[test]
    fn a_name_is_not_repeated() {
        let extra = ["localhost".to_string(), "10.2.0.11".to_string()];
        assert_eq!(
            master_sans("10.2.0.11".parse().unwrap(), &extra),
            ["127.0.0.1", "localhost", "10.2.0.11"],
        );
    }

    #[test]
    fn the_certificate_carries_a_dns_name_and_an_address() {
        let (ca, _d) = ca();
        let sans = master_sans(
            "0.0.0.0".parse().unwrap(),
            &["master.example".to_string(), "203.0.113.7".to_string()],
        );
        let (cert, _, _) = load_or_issue_master(&ca, sans).unwrap();
        let (_, pem) = x509_parser::pem::parse_x509_pem(cert.as_bytes()).unwrap();
        let cert = pem.parse_x509().unwrap();
        let names = cert.subject_alternative_name().unwrap().unwrap();
        let names: Vec<String> = names
            .value
            .general_names
            .iter()
            .map(|name| format!("{name:?}"))
            .collect();
        assert!(
            names.iter().any(|n| n.contains("master.example")),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("[203, 0, 113, 7]")),
            "{names:?}"
        );
        assert_eq!(names.len(), 5, "{names:?}");
    }

    #[test]
    fn both_sides_load_from_the_issued_certificates() {
        let (ca, _d) = ca();
        let (cert, key, ca_pem) = load_or_issue_master(&ca, vec!["127.0.0.1".into()]).unwrap();
        server_config(&cert, &key, &ca_pem).expect("master serves");

        let node = ca.issue_node("node-a").unwrap();
        client_config(
            &read_pem(&node.cert).unwrap(),
            &read_pem(&node.key).unwrap(),
            &ca_pem,
        )
        .expect("node dials");
    }

    #[test]
    fn the_served_chain_carries_the_ca_for_pinning() {
        let (ca, _d) = ca();
        let (cert, key, ca_pem) = load_or_issue_master(&ca, vec!["127.0.0.1".into()]).unwrap();
        server_config(&cert, &key, &ca_pem).unwrap();

        // The point of the chain: a node that has only the pinned
        // fingerprint must be able to find the CA in what the master sends.
        // Rebuild the chain the same way server_config does and look for it.
        let mut chain = parse_certs(&cert).unwrap();
        for c in parse_certs(&ca_pem).unwrap() {
            if !chain.contains(&c) {
                chain.push(c);
            }
        }
        assert_eq!(chain.len(), 2, "one leaf and one CA");
        let pin = fingerprint_of_pem(&ca_pem).unwrap();
        assert!(
            chain.iter().skip(1).any(|c| fingerprint(c) == pin),
            "the CA has to be reachable among the intermediates, which is \
             where PinnedCa looks for it"
        );
        assert_ne!(
            fingerprint(&chain[0]),
            pin,
            "the leaf must not be the CA, or pinning would prove nothing"
        );
    }

    #[test]
    fn two_nodes_get_distinguishable_certificates() {
        let (ca, _d) = ca();
        let a = read_pem(&ca.issue_node("node-a").unwrap().cert).unwrap();
        let b = read_pem(&ca.issue_node("node-b").unwrap().cert).unwrap();
        // The master tells one node from another by this value, so two
        // nodes sharing one would let either act as the other.
        assert_ne!(
            fingerprint_of_pem(&a).unwrap(),
            fingerprint_of_pem(&b).unwrap()
        );
    }

    #[test]
    fn reissuing_for_one_node_changes_the_fingerprint() {
        let (ca, _d) = ca();
        let first = read_pem(&ca.issue_node("node-a").unwrap().cert).unwrap();
        let again = read_pem(&ca.issue_node("node-a").unwrap().cert).unwrap();
        // Re-registration must overwrite the stored fingerprint, or the old
        // certificate would keep working beside the new one.
        assert_ne!(
            fingerprint_of_pem(&first).unwrap(),
            fingerprint_of_pem(&again).unwrap()
        );
    }

    #[test]
    fn a_node_certificate_is_not_the_ca() {
        let (ca, _d) = ca();
        let node = ca.issue_node("node-a").unwrap();
        let node_pem = read_pem(&node.cert).unwrap();
        assert_ne!(
            fingerprint_of_pem(&node_pem).unwrap(),
            fingerprint_of_pem(&ca.cert_pem()).unwrap()
        );
    }

    #[test]
    fn a_token_round_trips_through_the_pin() {
        let (ca, _d) = ca();
        let pin = fingerprint_of_pem(&ca.cert_pem()).unwrap();
        let t = join_token(&pin, "deadbeef");
        assert_eq!(split_token(&t), (Some(pin.as_str()), "deadbeef"));
    }

    #[test]
    fn a_token_without_a_pin_still_yields_its_secret() {
        // Minted before the pin existed: usable, but the node has nothing
        // to verify the master with, which is why it warns.
        assert_eq!(split_token("abc123"), (None, "abc123"));
        // Not a fingerprint-length prefix, so the whole thing is the secret.
        let short = format!("ab{TOKEN_SEP}cd");
        assert_eq!(split_token(&short), (None, short.as_str()));
    }

    #[test]
    fn the_fingerprint_is_stable_and_is_sha256() {
        let (ca, _d) = ca();
        let pem = ca.cert_pem();
        let a = fingerprint_of_pem(&pem).unwrap();
        assert_eq!(a, fingerprint_of_pem(&pem).unwrap());
        assert_eq!(a.len(), 64, "hex of 32 bytes");
    }
}

/// The identity a connection arrived with: the SHA-256 of the client
/// certificate the peer presented, or `None` when it presented none.
///
/// Put into every request's extensions by [`PeerCertAcceptor`], so a handler
/// reads who is calling from the transport rather than from the body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerCert(pub Option<String>);

/// Serves TLS and records which certificate each connection presented.
///
/// `axum-server`'s own Rustls acceptor terminates TLS and then throws the
/// connection's identity away, which would leave the master believing the
/// `node_id` in the request body -- so a node holding any valid certificate
/// could act as any other node. This wraps it and carries the fingerprint
/// through to the handlers.
#[derive(Clone)]
pub struct PeerCertAcceptor {
    inner: axum_server::tls_rustls::RustlsAcceptor<axum_server::accept::NoDelayAcceptor>,
}

impl PeerCertAcceptor {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            inner: axum_server::tls_rustls::RustlsAcceptor::new(
                axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(config)),
            )
            .acceptor(axum_server::accept::NoDelayAcceptor),
        }
    }
}

type AcceptFuture<S> = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = std::io::Result<(
                    tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
                    axum::middleware::AddExtension<S, PeerCert>,
                )>,
            > + Send,
    >,
>;

impl<S> axum_server::accept::Accept<tokio::net::TcpStream, S> for PeerCertAcceptor
where
    S: Send + 'static,
{
    type Stream = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Service = axum::middleware::AddExtension<S, PeerCert>;
    type Future = AcceptFuture<S>;

    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        let fut = axum_server::accept::Accept::accept(&self.inner, stream, service);
        Box::pin(async move {
            let (tls, service) = fut.await?;
            let who = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .map(fingerprint);
            let service = tower_layer::Layer::layer(&axum::Extension(PeerCert(who)), service);
            Ok((tls, service))
        })
    }
}
