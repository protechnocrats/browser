//! TLS trust anchors (Milestone 37, ADR 018): the bundled Mozilla roots,
//! plus whatever CA certificates the user or their organisation adds —
//! never instead of them.
//!
//! The host reads the files; this module takes their bytes. A file that
//! holds nothing usable is reported, never fatal, and never empties the
//! trust store.

use std::sync::Arc;

/// Largest PEM input accepted per file (a system bundle is ~200 KiB).
pub const MAX_PEM_BYTES: usize = 4 * 1024 * 1024;

/// Extra trust anchors, as PEM bytes (one entry per file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsTrust {
    pub extra_roots_pem: Vec<Vec<u8>>,
}

/// What building the trust store did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustReport {
    /// Certificates added on top of the bundled roots.
    pub added: usize,
    /// Certificates (or whole files) that could not be used.
    pub rejected: usize,
}

impl TlsTrust {
    /// Whether anything is added to the bundled roots.
    pub fn is_default(&self) -> bool {
        self.extra_roots_pem.is_empty()
    }

    /// A client configuration like `ureq`'s own (`ring`, TLS 1.2 and 1.3,
    /// the bundled Mozilla roots), with the extra roots added.
    pub fn client_config(&self) -> (Arc<rustls::ClientConfig>, TrustReport) {
        use rustls::pki_types::pem::PemObject;
        let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let mut report = TrustReport::default();
        for pem in &self.extra_roots_pem {
            if pem.len() > MAX_PEM_BYTES {
                report.rejected += 1;
                continue;
            }
            let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_slice_iter(pem).filter_map(Result::ok).collect();
            if certs.is_empty() {
                report.rejected += 1;
                continue;
            }
            let (added, ignored) = roots.add_parsable_certificates(certs);
            report.added += added;
            report.rejected += ignored;
        }
        let config = rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .expect("the ring provider supports TLS 1.2 and 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
        (Arc::new(config), report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_extra_is_the_bundled_roots() {
        let (_, report) = TlsTrust::default().client_config();
        assert_eq!(report, TrustReport::default());
    }

    #[test]
    fn unusable_files_are_reported_and_never_empty_the_store() {
        let garbage = TlsTrust {
            extra_roots_pem: vec![
                b"not a certificate".to_vec(),
                b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n".to_vec(),
                b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".to_vec(),
                vec![0u8; MAX_PEM_BYTES + 1],
                Vec::new(),
            ],
        };
        let (config, report) = garbage.client_config();
        assert_eq!(report.added, 0);
        assert_eq!(report.rejected, 5);
        // The configuration still trusts the bundled roots.
        drop(config);
    }
}
