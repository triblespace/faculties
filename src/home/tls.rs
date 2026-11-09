//! The bridge's TLS, pinned to its certificate.
//!
//! A Hue bridge presents a certificate signed by the Hue root CA for its
//! `bridgeid`, with no subjectAltName, so a WebPKI name check cannot pass for
//! the address it is reached at. Home pins the leaf instead: the SHA-256 of
//! the DER certificate the bridge presented when it was bound. Every later
//! handshake is refused unless the bridge presents that same certificate, and
//! the handshake signature is still verified against the certificate's key,
//! so a peer that merely replays the certificate cannot complete it.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, RequestBuilder};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use serde_json::Value;

/// The SHA-256 of one DER certificate.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(der: &[u8]) -> Self {
        use sha2::Digest;
        Self(sha2::Sha256::digest(der).into())
    }

    /// 64 hex digits, in either case, optionally separated by colons or
    /// spaces (OpenSSL prints `AB:CD:...`) and optionally prefixed `sha256:`
    /// or `SHA256 Fingerprint=`.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let text = [
            "SHA256 Fingerprint=",
            "sha256 Fingerprint=",
            "sha256:",
            "SHA256:",
        ]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .unwrap_or(text);
        let digits: String = text.chars().filter(|c| !matches!(c, ':' | ' ')).collect();
        let mut bytes = [0u8; 32];
        if digits.len() != 64 || hex::decode_to_slice(&digits, &mut bytes).is_err() {
            bail!("a certificate pin is a SHA-256 fingerprint: 64 hex digits, colons allowed");
        }
        Ok(Self(bytes))
    }
}

/// OpenSSL's spelling, so a pin can be compared with
/// `openssl x509 -noout -fingerprint -sha256` by eye.
impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// How a connection decides whom it is talking to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// Only the certificate with this fingerprint is accepted.
    Pin(Fingerprint),
    /// Any certificate is accepted and its fingerprint recorded: binding a
    /// bridge for the first time, probing one, or an explicit
    /// `--insecure-tls`.
    Observe,
}

#[derive(Debug)]
struct Verifier {
    pin: Option<Fingerprint>,
    presented: Arc<Mutex<Option<Fingerprint>>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = Fingerprint::of(end_entity);
        *self.presented.lock().expect("fingerprint slot") = Some(presented);
        match self.pin {
            Some(pin) if pin != presented => Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// An HTTPS client for one bridge that verifies (or, observing, records)
/// the certificate the bridge presents.
pub(crate) struct Transport {
    http: Client,
    pin: Option<Fingerprint>,
    presented: Arc<Mutex<Option<Fingerprint>>>,
}

impl Transport {
    pub(crate) fn new(trust: Trust) -> Result<Self> {
        let provider = rustls::crypto::ring::default_provider();
        let presented = Arc::new(Mutex::new(None));
        let pin = match trust {
            Trust::Pin(pin) => Some(pin),
            Trust::Observe => None,
        };
        let verifier = Verifier {
            pin,
            presented: presented.clone(),
            algorithms: provider.signature_verification_algorithms,
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .context("configure Hue bridge TLS versions")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        let http = Client::builder()
            .user_agent("faculties-home/0")
            .timeout(std::time::Duration::from_secs(10))
            // Plain HTTP would carry no certificate to check.
            .https_only(true)
            .use_preconfigured_tls(config)
            .build()
            .context("build Hue bridge HTTP client")?;
        Ok(Self {
            http,
            pin,
            presented,
        })
    }

    /// The fingerprint of the certificate the bridge last presented.
    pub(crate) fn presented(&self) -> Option<Fingerprint> {
        *self.presented.lock().expect("fingerprint slot")
    }

    pub(crate) fn get(&self, url: String) -> RequestBuilder {
        self.http.get(url)
    }

    pub(crate) fn put(&self, url: String) -> RequestBuilder {
        self.http.put(url)
    }

    /// Send one request and read its JSON answer. A transport error loses
    /// its URL (the URL carries the username); a pin mismatch becomes an
    /// error naming both fingerprints.
    pub(crate) fn json(&self, request: RequestBuilder) -> Result<Value> {
        request
            .send()
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.json())
            .map_err(|error| match (self.pin, self.presented()) {
                (Some(pin), Some(presented)) if pin != presented => anyhow!(
                    "the bridge presented a certificate with SHA-256 {presented}, \
                     but Home is pinned to {pin}; refusing to talk to it (if the \
                     bridge really changed its certificate, rebind it with \
                     `home bridge set ADDRESS SECRET --pin {presented}`)"
                ),
                _ => anyhow!(error.without_url()),
            })
    }
}

/// The address as an `https://` base URL. Plain HTTP is refused: it carries
/// no certificate, so neither a pin nor `--insecure-tls` means anything there.
pub(crate) fn base_url(address: &str) -> Result<String> {
    let address = address.trim().trim_end_matches('/');
    if address.is_empty() {
        bail!("bridge address is empty");
    }
    if address.starts_with("http://") {
        bail!("Home reaches a bridge over HTTPS only; drop the http:// scheme");
    }
    if address.starts_with("https://") {
        Ok(address.to_owned())
    } else {
        Ok(format!("https://{address}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_parse_openssl_and_bare_hex_and_print_openssl() {
        let pin = Fingerprint([0xab; 32]);
        let openssl = pin.to_string();
        assert_eq!(openssl.len(), 95);
        assert!(openssl.starts_with("AB:AB:"));
        assert_eq!(Fingerprint::parse(&openssl).unwrap(), pin);
        assert_eq!(Fingerprint::parse(&"ab".repeat(32)).unwrap(), pin);
        assert_eq!(
            Fingerprint::parse(&format!("sha256:{}", "AB".repeat(32))).unwrap(),
            pin
        );
        assert_eq!(
            Fingerprint::parse(&format!("SHA256 Fingerprint={openssl}")).unwrap(),
            pin
        );
        assert!(Fingerprint::parse(&"ab".repeat(31)).is_err());
        assert!(Fingerprint::parse(&"zz".repeat(32)).is_err());
        assert!(Fingerprint::parse("").is_err());
    }

    #[test]
    fn a_fingerprint_is_the_sha256_of_the_der_bytes() {
        // SHA-256 of the empty string.
        assert_eq!(
            Fingerprint::of(b""),
            Fingerprint::parse("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap()
        );
    }

    #[test]
    fn base_url_is_https_only() {
        assert_eq!(base_url("192.0.2.10").unwrap(), "https://192.0.2.10");
        assert_eq!(
            base_url("https://127.0.0.1:9/").unwrap(),
            "https://127.0.0.1:9"
        );
        assert!(base_url("http://192.0.2.10").is_err());
        assert!(base_url("  ").is_err());
    }
}
