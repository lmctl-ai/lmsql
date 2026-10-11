//! TLS for PostgreSQL connections: rustls (ring provider), roots from `sslrootcert` (PEM) or the
//! bundled Mozilla roots (webpki-roots). `require`/`prefer` encrypt without verifying the
//! certificate (libpq semantics); `verify-ca` checks the chain; `verify-full` also the host name.

use super::dsn::TlsMode;
use crate::error::DbError;
use lmrust::server::ErrorCode;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
  CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use std::path::Path;
use std::sync::Arc;
use tokio_postgres_rustls::MakeRustlsConnect;

fn tls_error(message: &str) -> DbError {
  DbError::new(ErrorCode::ValidationError, "TLS_CONFIG", message)
}

fn provider() -> Arc<CryptoProvider> {
  Arc::new(ring::default_provider())
}

fn roots(root_cert: Option<&Path>) -> Result<RootCertStore, DbError> {
  let mut store = RootCertStore::empty();
  match root_cert {
    Some(path) => {
      let certs = CertificateDer::pem_file_iter(path)
        .map_err(|_| tls_error("sslrootcert cannot be read as PEM certificates"))?;
      for cert in certs {
        let cert = cert.map_err(|_| tls_error("sslrootcert contains an invalid certificate"))?;
        store
          .add(cert)
          .map_err(|_| tls_error("sslrootcert contains an unusable certificate"))?;
      }
      if store.is_empty() {
        return Err(tls_error("sslrootcert contains no certificate"));
      }
    }
    None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
  }
  Ok(store)
}

/// Encrypt only: any certificate is accepted, handshake signatures are still checked.
#[derive(Debug)]
struct EncryptOnly(Arc<CryptoProvider>);

impl ServerCertVerifier for EncryptOnly {
  fn verify_server_cert(
    &self,
    _end_entity: &CertificateDer<'_>,
    _intermediates: &[CertificateDer<'_>],
    _server_name: &ServerName<'_>,
    _ocsp: &[u8],
    _now: UnixTime,
  ) -> Result<ServerCertVerified, rustls::Error> {
    Ok(ServerCertVerified::assertion())
  }
  fn verify_tls12_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    verify_tls12_signature(
      message,
      cert,
      dss,
      &self.0.signature_verification_algorithms,
    )
  }
  fn verify_tls13_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    verify_tls13_signature(
      message,
      cert,
      dss,
      &self.0.signature_verification_algorithms,
    )
  }
  fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
    self.0.signature_verification_algorithms.supported_schemes()
  }
}

/// `verify-ca`: the chain must verify; a host-name mismatch is accepted.
#[derive(Debug)]
struct ChainOnly(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for ChainOnly {
  fn verify_server_cert(
    &self,
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    server_name: &ServerName<'_>,
    ocsp: &[u8],
    now: UnixTime,
  ) -> Result<ServerCertVerified, rustls::Error> {
    match self
      .0
      .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
    {
      Err(rustls::Error::InvalidCertificate(
        CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
      )) => Ok(ServerCertVerified::assertion()),
      other => other,
    }
  }
  fn verify_tls12_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    self.0.verify_tls12_signature(message, cert, dss)
  }
  fn verify_tls13_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    self.0.verify_tls13_signature(message, cert, dss)
  }
  fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
    self.0.supported_verify_schemes()
  }
}

/// The connector for `mode`; `None` for `disable`.
pub fn connector(
  mode: TlsMode,
  root_cert: Option<&Path>,
) -> Result<Option<MakeRustlsConnect>, DbError> {
  let provider = provider();
  let builder = ClientConfig::builder_with_provider(provider.clone())
    .with_safe_default_protocol_versions()
    .map_err(|_| tls_error("no usable TLS protocol version"))?;
  let config = match mode {
    TlsMode::Disable => return Ok(None),
    TlsMode::Prefer | TlsMode::Require => builder
      .dangerous()
      .with_custom_certificate_verifier(Arc::new(EncryptOnly(provider)))
      .with_no_client_auth(),
    TlsMode::VerifyCa => {
      let verifier =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots(root_cert)?), provider)
          .build()
          .map_err(|_| tls_error("cannot build the certificate verifier"))?;
      builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ChainOnly(verifier)))
        .with_no_client_auth()
    }
    TlsMode::VerifyFull => builder
      .with_root_certificates(roots(root_cert)?)
      .with_no_client_auth(),
  };
  Ok(Some(MakeRustlsConnect::new(config)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn connectors_build_for_every_mode() {
    assert!(connector(TlsMode::Disable, None).unwrap().is_none());
    for mode in [
      TlsMode::Prefer,
      TlsMode::Require,
      TlsMode::VerifyCa,
      TlsMode::VerifyFull,
    ] {
      assert!(connector(mode, None).unwrap().is_some(), "{mode:?}");
    }
  }

  #[test]
  fn a_bad_sslrootcert_is_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.pem");
    let err = connector(TlsMode::VerifyFull, Some(&missing))
      .err()
      .unwrap();
    assert_eq!(err.tool_code, "TLS_CONFIG");
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "not a certificate").unwrap();
    let err = connector(TlsMode::VerifyCa, Some(&empty)).err().unwrap();
    assert_eq!(err.tool_code, "TLS_CONFIG");
    assert!(!err.message.contains(dir.path().to_str().unwrap()));
  }
}
