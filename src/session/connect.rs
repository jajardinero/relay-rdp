//! TCP → RDP negotiation → TLS (with certificate pinning) → CredSSP/NLA → session.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd as _, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironrdp_connector::{ClientConnector, ConnectionResult, Credentials, DesktopSize, ServerName};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp_pdu::nego::NegoRequestData;
use ironrdp_pdu::rdp::capability_sets::{MajorPlatformType, client_codecs_capabilities};
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use x509_cert::der::Encode as _;

use super::clipboard::ClipboardWorker;
use super::drive::SharedDrives;
use super::gfx::GfxShared;
use super::hostinfo;
use super::tls::{self, Backend};
use super::{
    CertificateInfo, CertificatePrompt, ConnectParams, DesktopRequest, EventSink, Failure,
    SessionEvent, Stage,
};
use crate::store::known_hosts::{self, Trust};
use crate::store::profiles::{AudioMode, DetectedOs, ExperienceSettings, Quality};

const DNS_TIMEOUT: Duration = Duration::from_secs(8);
const TCP_TIMEOUT: Duration = Duration::from_secs(8);
/// Applies separately to the negotiation + TLS phase and to the sign-in phase. Generous because
/// the server may be slow to create a session, but bounded so a wedged server can't hang the
/// tab forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(45);

pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Sync + Unpin {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Send + Sync + Unpin {}

pub type UpgradedFramed = ironrdp_tokio::TokioFramed<Box<dyn AsyncReadWrite>>;

pub struct Connection {
    pub result: ConnectionResult,
    pub framed: UpgradedFramed,
    /// The TCP socket under TLS, for RTT sampling. Valid while `framed` is alive.
    pub socket_fd: RawFd,
    /// The server's operating system, if it revealed it while signing in.
    pub os: Option<DetectedOs>,
}

/// Reports stage progress to the UI. Silent during automatic reconnects, where the UI shows
/// its own overlay instead of the connect track.
struct Progress<'a> {
    events: &'a EventSink,
    visible: bool,
    started: Instant,
    stage: Stage,
}

impl<'a> Progress<'a> {
    fn new(events: &'a EventSink, visible: bool) -> Self {
        let p = Self {
            events,
            visible,
            started: Instant::now(),
            stage: Stage::Network,
        };
        p.emit(SessionEvent::StageStarted(Stage::Network));
        p
    }

    fn advance(&mut self, next: Stage) {
        self.emit(SessionEvent::StageCompleted(
            self.stage,
            self.started.elapsed(),
        ));
        self.stage = next;
        self.started = Instant::now();
        self.emit(SessionEvent::StageStarted(next));
    }

    fn emit(&self, event: SessionEvent) {
        if self.visible {
            self.events.send(event);
        }
    }
}

/// Optional channels for one connection attempt.
#[derive(Clone)]
pub struct Channels<'a> {
    pub gfx: Option<&'a Arc<GfxShared>>,
    pub clipboard: Option<(
        &'a ClipboardWorker,
        tokio::sync::mpsc::UnboundedSender<super::SessionCmd>,
    )>,
}

pub async fn connect(
    params: &ConnectParams,
    desktop: DesktopRequest,
    events: &EventSink,
    show_progress: bool,
    channels: Channels<'_>,
) -> Result<Connection, Failure> {
    let mut progress = Progress::new(events, show_progress);

    let stream = open_tcp(&params.host, params.port).await?;
    progress.advance(Stage::Security);

    let SecureConnection {
        upgraded,
        connector,
        mut framed,
        certificate,
        socket_fd,
        recorded,
    } = with_timeout(begin_secure(params, desktop, channels, stream), params.port).await?;

    // Not under a timeout: this may wait for the user to read and answer the trust prompt.
    verify_certificate(params, &certificate, events).await?;

    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&certificate)
        .ok_or_else(|| Failure::internal("the server certificate has no public key"))?
        .to_owned();

    progress.advance(Stage::SignIn);

    let sign_in = async {
        ironrdp_tokio::connect_finalize(
            upgraded,
            connector,
            &mut framed,
            &mut ReqwestNetworkClient::new(),
            ServerName::new(params.host.clone()),
            server_public_key,
            None,
        )
        .await
        .map_err(|e| Failure::from_connector(&e, params.port))
    };
    let result = with_timeout(sign_in, params.port).await?;
    // Taking the recording also stops it.
    let os = recorded
        .lock()
        .take()
        .and_then(|bytes| hostinfo::detect(&bytes));
    match os {
        Some(os) => tracing::info!(?os, label = %os.label(), "server identified"),
        None => tracing::debug!("the server didn't reveal its operating system"),
    }

    progress.advance(Stage::Desktop);

    Ok(Connection {
        result,
        framed,
        socket_fd,
        os,
    })
}

struct SecureConnection {
    upgraded: ironrdp_tokio::Upgraded,
    connector: ClientConnector,
    framed: UpgradedFramed,
    certificate: x509_cert::Certificate,
    socket_fd: RawFd,
    /// What the server sends until sign-in completes.
    recorded: hostinfo::Recorded,
}

/// A failed TLS handshake consumes its socket. A compatibility attempt starts with a fresh
/// TCP connection, RDP negotiation and connector; it cannot reuse a half-negotiated session.
async fn begin_secure(
    params: &ConnectParams,
    desktop: DesktopRequest,
    channels: Channels<'_>,
    stream: TcpStream,
) -> Result<SecureConnection, Failure> {
    let mut initial = Some(stream);
    for backend in [Backend::Rustls, Backend::OpenSsl] {
        let stream = match initial.take() {
            Some(stream) => stream,
            None => open_tcp(&params.host, params.port).await?,
        };
        let socket_fd = stream.as_raw_fd();
        let client_addr = stream
            .local_addr()
            .map_err(|e| Failure::from_io(&e, params.port))?;
        let mut connector = build_connector(params, desktop, client_addr, channels.clone());
        let mut framed = ironrdp_tokio::TokioFramed::new(stream);
        let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
            .await
            .map_err(|e| Failure::from_connector(&e, params.port))?;
        let (tcp, leftover) = framed.into_inner();
        match tls::upgrade(tcp, &params.host, backend).await {
            Ok((stream, certificate)) => {
                let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
                let recorded = hostinfo::Recorded::new(parking_lot::Mutex::new(Some(Vec::new())));
                let stream: Box<dyn AsyncReadWrite> =
                    Box::new(hostinfo::Recording::new(stream, Arc::clone(&recorded)));
                return Ok(SecureConnection {
                    upgraded,
                    connector,
                    framed: ironrdp_tokio::TokioFramed::new_with_leftover(stream, leftover),
                    certificate,
                    socket_fd,
                    recorded,
                });
            }
            Err(error) if backend == Backend::Rustls && tls::can_retry(&error) => {
                tracing::info!(%error, "retrying RDP negotiation with TLS compatibility backend");
            }
            Err(error) => return Err(Failure::tls(&error)),
        }
    }
    unreachable!("the compatibility backend always returns a connection or an error")
}

async fn with_timeout<T>(
    fut: impl Future<Output = Result<T, Failure>>,
    port: u16,
) -> Result<T, Failure> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| Err(Failure::timeout(port)))
}

async fn open_tcp(host: &str, port: u16) -> Result<TcpStream, Failure> {
    let addrs: Vec<SocketAddr> =
        match tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port))).await {
            Ok(Ok(addrs)) => addrs.collect(),
            Ok(Err(e)) => return Err(Failure::dns(host, &e)),
            Err(_) => {
                return Err(Failure::dns(
                    host,
                    &std::io::Error::from(std::io::ErrorKind::TimedOut),
                ));
            }
        };

    let mut last_failure = Failure::dns(host, &std::io::Error::from(std::io::ErrorKind::NotFound));
    for addr in addrs {
        match tokio::time::timeout(TCP_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                tune_socket(&stream);
                tracing::debug!(%addr, "TCP connected");
                return Ok(stream);
            }
            Ok(Err(e)) => {
                tracing::debug!(%addr, error = %e, "TCP connect failed");
                last_failure = Failure::from_io(&e, port);
            }
            Err(_) => {
                tracing::debug!(%addr, "TCP connect timed out");
                last_failure = Failure::timeout(port);
            }
        }
    }
    Err(last_failure)
}

/// Low latency for input, and keepalives so a dead link is noticed in ~30 s instead of the
/// kernel default of over two hours.
fn tune_socket(stream: &TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::warn!(error = %e, "failed to set TCP_NODELAY");
    }
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5))
        .with_retries(3);
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive) {
        tracing::warn!(error = %e, "failed to enable TCP keepalive");
    }
}

fn build_connector(
    params: &ConnectParams,
    desktop: DesktopRequest,
    client_addr: SocketAddr,
    channels: Channels<'_>,
) -> ClientConnector {
    let gfx = channels.gfx;
    let audio = params.devices.audio == AudioMode::Local;
    let desktop = desktop.normalized();

    let performance_flags = performance_flags(&params.experience);
    let connection_type = connection_type(params.experience.quality);
    tracing::debug!(
        quality = ?params.experience.quality,
        ?connection_type,
        ?performance_flags,
        "connection quality"
    );

    let load_balance_info = params.advanced.load_balance_info.trim();
    let request_data = (!load_balance_info.is_empty())
        .then(|| NegoRequestData::routing_token(format!("{load_balance_info}\r\n")));

    let codecs = client_codecs_capabilities(&[]).expect("default codec list is valid");

    let config = ironrdp_connector::Config {
        desktop_size: DesktopSize {
            width: desktop.width,
            height: desktop.height,
        },
        desktop_scale_factor: if desktop.scale_factor > 100 {
            desktop.scale_factor
        } else {
            0
        },
        enable_tls: true,
        enable_credssp: true,
        credentials: Credentials::UsernamePassword {
            username: params.username.clone(),
            password: params.password.to_string(),
        },
        domain: (!params.domain.is_empty()).then(|| params.domain.clone()),
        client_build: 22621,
        client_name: client_name(),
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: params.advanced.keyboard_layout,
        ime_file_name: String::new(),
        bitmap: Some(ironrdp_connector::BitmapConfig {
            lossy_compression: true,
            color_depth: 32,
            codecs,
        }),
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data,
        autologon: false,
        enable_audio_playback: audio,
        performance_flags,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        // Off on purpose: the fast-path processor rebuilt after a deactivation-reactivation (any
        // resize) loses the decompressor state, which would corrupt every later update. RemoteFX
        // and the graphics pipeline are compressed by their codecs anyway.
        compression_type: None,
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: gfx.is_some(),
        connection_type,
    };

    let mut drdynvc = ironrdp_dvc::DrdynvcClient::new()
        .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
    if let Some(gfx) = gfx {
        drdynvc = drdynvc.with_dynamic_channel(gfx.client());
    }

    let client_name = config.client_name.clone();
    let mut connector = ClientConnector::new(config, client_addr).with_static_channel(drdynvc);

    if let Some((worker, session)) = channels.clipboard {
        connector =
            connector.with_static_channel(ironrdp_cliprdr::Cliprdr::new(worker.backend(session)));
    }

    if audio {
        connector = connector.with_static_channel(ironrdp_rdpsnd::client::Rdpsnd::new(Box::new(
            ironrdp_rdpsnd_native::cpal::RdpsndBackend::new(),
        )));
    }

    // Device redirection carries shared folders; Windows also expects it to be present
    // before it redirects audio.
    let (drives, announce) = SharedDrives::new(&params.devices.shared_folders);
    if audio || !announce.is_empty() {
        let mut rdpdr = ironrdp_rdpdr::Rdpdr::new(Box::new(drives), client_name);
        if !announce.is_empty() {
            rdpdr = rdpdr.with_drives(Some(announce));
        }
        connector = connector.with_static_channel(rdpdr);
    }

    connector
}

/// Visual effects the server should render, from the profile's toggles and quality.
fn performance_flags(experience: &ExperienceSettings) -> PerformanceFlags {
    let mut flags = PerformanceFlags::DISABLE_CURSOR_SHADOW;
    if experience.quality != Quality::LowBandwidth {
        flags |= PerformanceFlags::ENABLE_DESKTOP_COMPOSITION;
    }
    if !experience.wallpaper {
        flags |= PerformanceFlags::DISABLE_WALLPAPER;
    }
    if !experience.animations {
        flags |=
            PerformanceFlags::DISABLE_MENUANIMATIONS | PerformanceFlags::DISABLE_FULLWINDOWDRAG;
    }
    if experience.font_smoothing {
        flags |= PerformanceFlags::ENABLE_FONT_SMOOTHING;
    }
    flags
}

/// The link type we report to the server, which tunes video quality and frame rate to it.
/// Never auto-detect: that needs bandwidth probes IronRDP doesn't answer yet.
fn connection_type(quality: Quality) -> ConnectionType {
    match quality {
        Quality::Best => ConnectionType::Lan,
        Quality::Balanced => ConnectionType::BroadbandHigh,
        Quality::LowBandwidth => ConnectionType::BroadbandLow,
    }
}

/// The local hostname, as the server shows it (at most 15 characters).
fn client_name() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let name = name.split('.').next().unwrap_or("relay");
    let name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(15)
        .collect();
    if name.is_empty() {
        "relay".to_owned()
    } else {
        name
    }
}

async fn verify_certificate(
    params: &ConnectParams,
    certificate: &x509_cert::Certificate,
    events: &EventSink,
) -> Result<(), Failure> {
    let der = certificate
        .to_der()
        .map_err(|e| Failure::internal(format!("encode server certificate: {e}")))?;
    let fingerprint = known_hosts::fingerprint(&der);

    let trust = params
        .known_hosts
        .lock()
        .check(&params.host, params.port, &fingerprint);
    let previous_fingerprint = match trust {
        Trust::Trusted => return Ok(()),
        Trust::Unknown => None,
        Trust::Changed { previous } => Some(previous),
    };

    let tbs = &certificate.tbs_certificate;
    let info = CertificateInfo {
        host: params.host.clone(),
        port: params.port,
        fingerprint: fingerprint.clone(),
        subject: tbs.subject.to_string(),
        issuer: tbs.issuer.to_string(),
        not_after: format_time(&tbs.validity.not_after),
        previous_fingerprint,
    };

    let (reply, answer) = oneshot::channel();
    events.send(SessionEvent::Certificate(CertificatePrompt {
        info: info.clone(),
        reply,
    }));

    match answer.await {
        Ok(true) => {
            params
                .known_hosts
                .lock()
                .trust(&params.host, params.port, &fingerprint, &info.subject);
            Ok(())
        }
        _ => Err(Failure::untrusted_certificate()),
    }
}

fn format_time(time: &x509_cert::time::Time) -> String {
    let secs = i64::try_from(time.to_unix_duration().as_secs()).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp(secs, 0)
        .map_or_else(|| time.to_string(), |d| d.format("%B %-d, %Y").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::ssl::{Ssl, SslContextBuilder, SslMethod, SslVersion};
    use openssl::x509::{X509, X509NameBuilder};
    use std::pin::Pin;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    struct TlsServer {
        port: u16,
        certificate: Vec<u8>,
        task: tokio::task::JoinHandle<Option<String>>,
    }

    impl TlsServer {
        async fn start(cipher: &str, version: SslVersion, attempts: usize) -> Self {
            let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
            let mut name = X509NameBuilder::new().unwrap();
            name.append_entry_by_text("CN", "relay-tls-test").unwrap();
            let name = name.build();
            let mut certificate = X509::builder().unwrap();
            certificate.set_version(2).unwrap();
            certificate.set_subject_name(&name).unwrap();
            certificate.set_issuer_name(&name).unwrap();
            certificate.set_pubkey(&key).unwrap();
            certificate
                .set_not_before(&Asn1Time::days_from_now(0).unwrap())
                .unwrap();
            certificate
                .set_not_after(&Asn1Time::days_from_now(1).unwrap())
                .unwrap();
            certificate.sign(&key, MessageDigest::sha256()).unwrap();
            let certificate = certificate.build();
            let mut builder = SslContextBuilder::new(SslMethod::tls_server()).unwrap();
            builder.set_private_key(&key).unwrap();
            builder.set_certificate(&certificate).unwrap();
            builder.set_cipher_list(cipher).unwrap();
            builder.set_min_proto_version(Some(version)).unwrap();
            builder.set_max_proto_version(Some(version)).unwrap();
            if version == SslVersion::TLS1_1 {
                builder.set_security_level(0); // Test server only: prove the client rejects TLS 1.1.
            }
            let context = builder.build();
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let task = tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(8), async move {
                    for _ in 0..attempts {
                        let (mut tcp, _) = listener.accept().await.unwrap();
                        let mut header = [0u8; 4];
                        tcp.read_exact(&mut header).await.unwrap();
                        assert_eq!(&header[..2], &[3, 0]);
                        let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
                        let mut request = vec![0; len - 4];
                        tcp.read_exact(&mut request).await.unwrap();
                        // Confirm NLA/HYBRID_EX. No credentials are exchanged by these tests.
                        tcp.write_all(&[
                            3, 0, 0, 19, 14, 0xd0, 0, 0, 0x12, 0x34, 0, 2, 0, 8, 0, 8, 0, 0, 0,
                        ])
                        .await
                        .unwrap();
                        let mut tls =
                            tokio_openssl::SslStream::new(Ssl::new(&context).unwrap(), tcp)
                                .unwrap();
                        if Pin::new(&mut tls).accept().await.is_err() {
                            continue;
                        }
                        assert_eq!(tls.ssl().version_str(), "TLSv1.2");
                        let cipher = tls.ssl().current_cipher().unwrap().name().to_owned();
                        let mut ping = [0u8; 4];
                        tls.read_exact(&mut ping).await.unwrap();
                        assert_eq!(&ping, b"ping");
                        tls.write_all(b"pong").await.unwrap();
                        return Some(cipher);
                    }
                    None
                })
                .await
                .expect("TLS test server timed out")
            });
            Self {
                port,
                certificate: certificate.to_der().unwrap(),
                task,
            }
        }
    }

    fn tls_params(host: &str, port: u16) -> ConnectParams {
        let _ = rustls::crypto::ring::default_provider().install_default();
        ConnectParams {
            host: host.to_owned(),
            port,
            username: String::new(),
            domain: String::new(),
            password: zeroize::Zeroizing::new(String::new()),
            desktop: DesktopRequest {
                width: 640,
                height: 480,
                scale_factor: 100,
            },
            dynamic_resize: false,
            experience: Default::default(),
            devices: crate::store::profiles::DeviceSettings {
                audio: AudioMode::Off,
                clipboard: false,
                shared_folders: Vec::new(),
            },
            advanced: Default::default(),
            known_hosts: Arc::new(parking_lot::Mutex::new(
                crate::store::known_hosts::KnownHosts::in_memory(),
            )),
            hardware_video_decoding: false,
            gpu: None,
        }
    }

    async fn test_secure_connection(params: &ConnectParams) -> Result<SecureConnection, Failure> {
        let tcp = open_tcp(&params.host, params.port).await?;
        with_timeout(
            begin_secure(
                params,
                params.desktop,
                Channels {
                    gfx: None,
                    clipboard: None,
                },
                tcp,
            ),
            params.port,
        )
        .await
    }

    async fn check_tls_server(server: TlsServer, expected_cipher: &str) {
        let params = tls_params("127.0.0.1", server.port);
        let secure = test_secure_connection(&params).await.unwrap();
        // Both transports must hand the exact server certificate to the common pinning gate.
        assert_eq!(secure.certificate.to_der().unwrap(), server.certificate);
        assert!(ironrdp_tls::extract_tls_server_public_key(&secure.certificate).is_some());
        let (mut tls, _) = secure.framed.into_inner();
        tls.write_all(b"ping").await.unwrap();
        tls.flush().await.unwrap();
        let mut pong = [0u8; 4];
        tls.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");
        assert_eq!(server.task.await.unwrap().as_deref(), Some(expected_cipher));
    }

    #[tokio::test]
    async fn rdp_tls_keeps_modern_backend() {
        check_tls_server(
            TlsServer::start("ECDHE-RSA-AES256-GCM-SHA384", SslVersion::TLS1_2, 1).await,
            "ECDHE-RSA-AES256-GCM-SHA384",
        )
        .await;
    }

    #[tokio::test]
    async fn rdp_tls_retries_cbc_server_with_fresh_negotiation() {
        check_tls_server(
            TlsServer::start("ECDHE-RSA-AES256-SHA384", SslVersion::TLS1_2, 2).await,
            "ECDHE-RSA-AES256-SHA384",
        )
        .await;
    }

    #[tokio::test]
    async fn rdp_tls_rejects_tls11_server() {
        let server = TlsServer::start("ECDHE-RSA-AES256-SHA", SslVersion::TLS1_1, 2).await;
        let params = tls_params("127.0.0.1", server.port);
        let failure = match test_secure_connection(&params).await {
            Ok(_) => panic!("accepted TLS 1.1"),
            Err(failure) => failure,
        };
        assert_eq!(failure.kind, super::super::errors::FailureKind::Security);
        assert!(server.task.await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "unauthenticated RDP/TLS probe; set RELAY_TLS_TEST_HOST explicitly"]
    async fn rdp_tls_remote_handshake() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("relay=info")
            .with_test_writer()
            .try_init();
        let host = std::env::var("RELAY_TLS_TEST_HOST").unwrap();
        let params = tls_params(&host, 3389);
        let secure = test_secure_connection(&params).await.unwrap();
        assert!(ironrdp_tls::extract_tls_server_public_key(&secure.certificate).is_some());
        eprintln!("RDP/TLS succeeded for {host}; no login credentials were sent");
    }

    #[test]
    fn quality_maps_to_connection_type() {
        assert_eq!(connection_type(Quality::Best), ConnectionType::Lan);
        assert_eq!(
            connection_type(Quality::Balanced),
            ConnectionType::BroadbandHigh
        );
        assert_eq!(
            connection_type(Quality::LowBandwidth),
            ConnectionType::BroadbandLow
        );
    }

    #[test]
    fn performance_flags_follow_toggles_and_quality() {
        let mut experience = ExperienceSettings::default();
        Quality::Best.apply_visuals(&mut experience);
        let best = performance_flags(&experience);
        assert!(best.contains(
            PerformanceFlags::ENABLE_DESKTOP_COMPOSITION | PerformanceFlags::ENABLE_FONT_SMOOTHING
        ));
        assert!(!best.intersects(
            PerformanceFlags::DISABLE_WALLPAPER | PerformanceFlags::DISABLE_MENUANIMATIONS
        ));

        experience.quality = Quality::LowBandwidth;
        Quality::LowBandwidth.apply_visuals(&mut experience);
        let low = performance_flags(&experience);
        assert!(!low.intersects(
            PerformanceFlags::ENABLE_DESKTOP_COMPOSITION | PerformanceFlags::ENABLE_FONT_SMOOTHING
        ));
        assert!(low.contains(
            PerformanceFlags::DISABLE_WALLPAPER
                | PerformanceFlags::DISABLE_MENUANIMATIONS
                | PerformanceFlags::DISABLE_FULLWINDOWDRAG
        ));

        // Toggles changed after picking a preset still win.
        experience.font_smoothing = true;
        assert!(performance_flags(&experience).contains(PerformanceFlags::ENABLE_FONT_SMOOTHING));
    }
}
