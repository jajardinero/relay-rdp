//! RDP TLS transports. Rustls is preferred; OpenSSL adds TLS 1.2 CBC compatibility, and TLS 1.0
//! for hosts that allow legacy TLS.
//! Certificate trust is checked by connect::verify_certificate before any CredSSP traffic.

use std::io;
use std::pin::Pin;

use openssl::ssl::{
    SslConnector, SslMethod, SslOptions, SslSessionCacheMode, SslVerifyMode, SslVersion,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use x509_cert::Certificate;
use x509_cert::der::Decode as _;

use super::connect::AsyncReadWrite;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Backend {
    Rustls,
    OpenSsl,
}

/// Only retry failures that can mean no common TLS version/cipher/group. Certificate parsing,
/// certificate trust, malformed messages and authentication failures never select a new backend.
pub(super) fn can_retry(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
    ) {
        return true;
    }
    error
        .get_ref()
        .and_then(|error| error.downcast_ref::<rustls::Error>())
        .is_some_and(|error| {
            matches!(
                error,
                rustls::Error::PeerIncompatible(_)
                    | rustls::Error::AlertReceived(
                        rustls::AlertDescription::HandshakeFailure
                            | rustls::AlertDescription::InsufficientSecurity
                            | rustls::AlertDescription::ProtocolVersion
                    )
            )
        })
}

/// `legacy`: the host allows TLS 1.0 and AES-CBC-SHA suites (OpenSSL backend only).
pub(super) async fn upgrade<S>(
    stream: S,
    host: &str,
    backend: Backend,
    legacy: bool,
) -> io::Result<(Box<dyn AsyncReadWrite>, Certificate)>
where
    S: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
{
    match backend {
        Backend::Rustls => {
            let (stream, certificate) = ironrdp_tls::upgrade(stream, host).await?;
            let negotiated = ironrdp_tls::negotiated(&stream);
            tracing::info!(?backend, version = ?negotiated.version, cipher = ?negotiated.cipher_suite, "RDP TLS established");
            Ok((Box::new(stream), certificate))
        }
        Backend::OpenSsl => {
            let mut builder = SslConnector::builder(SslMethod::tls_client())?;
            // Add only forward-secret AES-CBC/SHA-2 suites to rustls's modern suite set.
            // TLS 1.3 suites use OpenSSL's separate, modern defaults.
            const MODERN: &str =
                "ECDHE+AESGCM:ECDHE+CHACHA20:ECDHE-RSA-AES256-SHA384:ECDHE-RSA-AES128-SHA256";
            if legacy {
                // Old Windows offers only TLS 1.0, often with RSA key exchange and AES-CBC-SHA.
                // OpenSSL refuses both above security level 0. Preferred suites come first.
                builder.set_min_proto_version(Some(SslVersion::TLS1))?;
                builder.set_cipher_list(&format!(
                    "{MODERN}:ECDHE-RSA-AES256-SHA:ECDHE-RSA-AES128-SHA:AES256-SHA:AES128-SHA:@SECLEVEL=0"
                ))?;
            } else {
                builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
                builder.set_cipher_list(MODERN)?;
            }
            // RDP uses explicit certificate pinning, including changed-certificate approval.
            // Do not send credentials until that common trust check has completed.
            builder.set_verify(SslVerifyMode::NONE);
            // CredSSP does not support TLS session resumption.
            builder.set_session_cache_mode(SslSessionCacheMode::OFF);
            builder.set_options(SslOptions::NO_TICKET);
            // For CBC suites before TLS 1.1, OpenSSL sends an empty record before each one
            // with data (a BEAST countermeasure; the openssl crate keeps it on). Windows' RDP
            // server rejects them with an internal-error alert, so send whole records, as
            // FreeRDP does. TLS 1.1 and later aren't affected.
            builder.set_options(SslOptions::DONT_INSERT_EMPTY_FRAGMENTS);
            let connector = builder.build();
            let ssl = connector
                .configure()?
                .use_server_name_indication(false)
                .verify_hostname(false)
                .into_ssl(host)?;
            let mut stream = tokio_openssl::SslStream::new(ssl, stream)?;
            Pin::new(&mut stream)
                .connect()
                .await
                .map_err(io::Error::other)?;
            stream.flush().await?;
            let certificate = stream
                .ssl()
                .peer_certificate()
                .ok_or_else(|| io::Error::other("peer certificate is missing"))?;
            let certificate =
                Certificate::from_der(&certificate.to_der()?).map_err(io::Error::other)?;
            let version = stream.ssl().version_str();
            let cipher = stream.ssl().current_cipher().map(|cipher| cipher.name());
            let old = [SslVersion::SSL3, SslVersion::TLS1, SslVersion::TLS1_1];
            if stream.ssl().version2().is_some_and(|v| old.contains(&v)) {
                tracing::warn!(
                    ?backend,
                    version,
                    ?cipher,
                    "RDP TLS established with legacy TLS"
                );
            } else {
                tracing::info!(?backend, version, ?cipher, "RDP TLS established");
            }
            Ok((Box::new(stream), certificate))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_only_for_tls_compatibility_failures() {
        for kind in [io::ErrorKind::ConnectionReset, io::ErrorKind::UnexpectedEof] {
            assert!(can_retry(&io::Error::from(kind)));
        }
        assert!(can_retry(&io::Error::other(rustls::Error::AlertReceived(
            rustls::AlertDescription::HandshakeFailure,
        ))));
        for error in [
            io::Error::from(io::ErrorKind::TimedOut),
            io::Error::other("invalid certificate encoding"),
            io::Error::other(rustls::Error::NoCertificatesPresented),
            io::Error::other(rustls::Error::AlertReceived(
                rustls::AlertDescription::BadCertificate,
            )),
        ] {
            assert!(!can_retry(&error), "unexpected retry for {error}");
        }
    }
}
