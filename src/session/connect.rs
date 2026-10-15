//! TCP → RDP negotiation → TLS (with certificate pinning) → CredSSP/NLA → session.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd as _, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironrdp_connector::{ClientConnector, ConnectionResult, Credentials, DesktopSize, ServerName};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_pdu::gcc::KeyboardType;
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
use super::{
    CertificateInfo, CertificatePrompt, ConnectParams, DesktopRequest, EventSink, Failure,
    SessionEvent, Stage,
};
use crate::store::known_hosts::{self, Trust};
use crate::store::profiles::AudioMode;

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
    let socket_fd = stream.as_raw_fd();
    let client_addr = stream
        .local_addr()
        .map_err(|e| Failure::from_io(&e, params.port))?;

    progress.advance(Stage::Security);

    let mut connector = build_connector(params, desktop, client_addr, channels);
    let secure = async {
        let mut framed = ironrdp_tokio::TokioFramed::new(stream);
        let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
            .await
            .map_err(|e| Failure::from_connector(&e, params.port))?;

        let (tcp, leftover) = framed.into_inner();
        let (tls_stream, certificate) = ironrdp_tls::upgrade(tcp, &params.host)
            .await
            .map_err(|e| Failure::from_io(&e, params.port))?;
        Ok::<_, Failure>((should_upgrade, tls_stream, certificate, leftover))
    };
    let (should_upgrade, tls_stream, certificate, leftover) =
        with_timeout(secure, params.port).await?;

    // Not under a timeout: this may wait for the user to read and answer the trust prompt.
    verify_certificate(params, &certificate, events).await?;

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let erased: Box<dyn AsyncReadWrite> = Box::new(tls_stream);
    let mut framed = ironrdp_tokio::TokioFramed::new_with_leftover(erased, leftover);

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

    progress.advance(Stage::Desktop);

    Ok(Connection {
        result,
        framed,
        socket_fd,
    })
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

    let mut performance_flags =
        PerformanceFlags::DISABLE_CURSOR_SHADOW | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION;
    if !params.experience.wallpaper {
        performance_flags |= PerformanceFlags::DISABLE_WALLPAPER;
    }
    if !params.experience.animations {
        performance_flags |=
            PerformanceFlags::DISABLE_MENUANIMATIONS | PerformanceFlags::DISABLE_FULLWINDOWDRAG;
    }
    if params.experience.font_smoothing {
        performance_flags |= PerformanceFlags::ENABLE_FONT_SMOOTHING;
    }

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
