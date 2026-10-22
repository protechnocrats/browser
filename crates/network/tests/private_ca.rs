//! M37's gap, against a live TLS server (ADR 018): a site whose certificate
//! a private CA issued is trusted once the CA is added as an extra root —
//! in addition to the bundled ones, never instead — and refused without it.
//!
//! The fixtures in `fixtures/private-ca/` are a CA certificate and a leaf for
//! `localhost` and `127.0.0.1` it signed, valid until 2126. The CA's private
//! key was deleted after signing: the fixtures can issue nothing else.

use network::{NetworkConfig, NetworkContext, NetworkError, RequestContext, TlsTrust};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

const CA: &[u8] = include_bytes!("fixtures/private-ca/ca.pem");
const LEAF: &[u8] = include_bytes!("fixtures/private-ca/leaf.pem");
const LEAF_KEY: &[u8] = include_bytes!("fixtures/private-ca/leaf.key");

/// An HTTPS server presenting the private CA's leaf; answers `secret`.
fn serve_tls() -> u16 {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(LEAF).map(Result::unwrap).collect();
    let key = PrivateKeyDer::from_pem_slice(LEAF_KEY).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = Arc::clone(&config);
            std::thread::spawn(move || {
                let Ok(connection) = rustls::ServerConnection::new(config) else { return };
                let mut tls = rustls::StreamOwned::new(connection, stream);
                let mut reader = BufReader::new(&mut tls);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                drop(reader);
                let _ = write!(tls, "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret");
                let _ = tls.flush();
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    port
}

fn context(extra_roots: Vec<Vec<u8>>) -> NetworkContext {
    let config = NetworkConfig { tls: TlsTrust { extra_roots_pem: extra_roots }, ..NetworkConfig::default() };
    let (network, report) = NetworkContext::with_config(&config).unwrap();
    assert_eq!(report.trust.rejected, 0);
    network
}

#[test]
fn a_private_ca_added_as_an_extra_root_is_trusted_for_its_sites() {
    let port = serve_tls();
    for host in ["localhost", "127.0.0.1"] {
        let response = context(vec![CA.to_vec()]).fetch_with(&format!("https://{host}:{port}/"), &RequestContext::typed_navigation()).unwrap();
        assert_eq!((response.status, response.body.as_slice()), (200, &b"secret"[..]), "{host}");
    }
}

#[test]
fn without_the_private_ca_the_site_is_refused() {
    let port = serve_tls();
    let result = context(Vec::new()).fetch_with(&format!("https://localhost:{port}/"), &RequestContext::typed_navigation());
    let Err(NetworkError::Transport(why)) = result else { panic!("expected a TLS failure, got {result:?}") };
    assert!(why.to_ascii_lowercase().contains("certificate"), "refused for its certificate: {why}");
}

/// Extra roots are added to the bundled ones, never instead: a store with
/// the private CA still holds every bundled root.
#[test]
fn an_extra_root_is_added_to_the_bundled_ones() {
    let (_, report) = TlsTrust { extra_roots_pem: vec![CA.to_vec()] }.client_config();
    assert_eq!((report.added, report.rejected), (1, 0));
}
