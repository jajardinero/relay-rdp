//! The session lifecycle: connect, run the active stage, and reconnect when the network drops.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironrdp_connector::connection_activation::ConnectionActivationState;
use ironrdp_core::WriteBuf;
use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{
    ActiveStage, ActiveStageBuilder, ActiveStageOutput, GracefulDisconnectReason, fast_path,
};
use ironrdp_tokio::{FramedWrite as _, single_sequence_step_read, split_tokio_framed};
use smallvec::SmallVec;
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

use super::clipboard::ClipboardWorker;
use super::connect::{self, Channels, Connection};
use super::framebuffer::Rect;
use super::gfx::{GfxFailure, GfxShared};
use super::{
    ConnectParams, DesktopRequest, Ended, EventSink, Failure, PointerUpdate, Rendering, SessionCmd,
    SessionEvent, SharedFrame, Stage, Stats, netinfo, slowpath,
};
use crate::store::profiles::GraphicsMode;

/// Keep trying to reconnect for this long after the connection drops.
const RECONNECT_WINDOW: Duration = Duration::from_secs(300);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// While reconnecting, how often to look for a new network (a VPN tunnel or Wi-Fi coming up).
const NETWORK_POLL: Duration = Duration::from_millis(500);
/// How long to wait for the server to acknowledge a disconnect before dropping the socket.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// Window resizes are coalesced; only the size the user settles on is sent.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(250);
/// Display Control opens a moment after the session starts; keep retrying a resize until then.
const RESIZE_RETRY: Duration = Duration::from_millis(500);
const RESIZE_MAX_RETRIES: u32 = 20;

enum Exit {
    Closed,
    ServerEnded(String),
    /// The connection dropped; worth reconnecting.
    Lost(Failure),
    Failed(Failure),
    /// The graphics pipeline hit something it can't decode; reconnect with legacy graphics.
    GraphicsFallback(GfxFailure),
}

pub(super) async fn run_session(
    params: ConnectParams,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    self_tx: mpsc::UnboundedSender<SessionCmd>,
    events: EventSink,
    frame: Arc<SharedFrame>,
) {
    let clipboard = params
        .devices
        .clipboard
        .then(|| ClipboardWorker::spawn(self_tx.clone()));
    let mut desktop = params.desktop;
    let mut reconnecting_since: Option<Instant> = None;
    let mut attempt = 0u32;
    // Shown again when the user restarts a reconnect attempt.
    let mut lost_reason = String::new();
    let mut graphics = params.experience.graphics;
    let mut graphics_failures = 0u32;
    let mut hardware = params.hardware_video_decoding;
    let mut hardware_reason = None;

    'session: loop {
        let show_progress = reconnecting_since.is_none();
        let gfx = (graphics != GraphicsMode::Compatible).then(|| {
            GfxShared::new(
                Arc::clone(&frame),
                events.clone(),
                graphics,
                hardware,
                params.gpu.clone(),
                hardware_reason.clone(),
            )
        });
        let channels = Channels {
            gfx: gfx.as_ref(),
            clipboard: clipboard.as_ref().map(|worker| (worker, self_tx.clone())),
        };
        let connecting = connect::connect(&params, desktop, &events, show_progress, channels);
        tokio::pin!(connecting);
        let network =
            network_changed(reconnecting_since.and_then(|_| netinfo::routable_addresses()));
        tokio::pin!(network);

        let outcome = loop {
            tokio::select! {
                result = &mut connecting => break Some(result),
                // An attempt that started before the new network was up may be stuck on the old
                // route; start over on the new one.
                () = &mut network => {
                    tracing::debug!("network changed; retrying now");
                    restart_attempt(&events, &mut attempt, &lost_reason);
                    continue 'session;
                }
                cmd = cmd_rx.recv() => match cmd {
                    None | Some(SessionCmd::Disconnect) => break None,
                    // A reconnect attempt can hang on a half-working network; start over.
                    Some(SessionCmd::RetryNow) if reconnecting_since.is_some() => {
                        restart_attempt(&events, &mut attempt, &lost_reason);
                        continue 'session;
                    }
                    Some(SessionCmd::Resize(request)) => desktop = request,
                    Some(_) => {}
                },
            }
        };

        let exit = match outcome {
            None => Exit::Closed,
            Some(Ok(connection)) => {
                attempt = 0;
                reconnecting_since = None;
                ActiveSession::new(
                    connection,
                    &params,
                    desktop,
                    &events,
                    &frame,
                    gfx.clone(),
                    clipboard.as_ref(),
                )
                .run(&mut cmd_rx, &mut desktop)
                .await
            }
            Some(Err(failure)) if reconnecting_since.is_some() && failure.is_network() => {
                Exit::Lost(failure)
            }
            Some(Err(failure)) => Exit::Failed(failure),
        };

        let failure = match exit {
            Exit::GraphicsFallback(GfxFailure {
                reason,
                hardware: hardware_failure,
            }) => {
                // Codec errors (ClearCodec, Progressive...) aren't the GPU's fault, so they don't
                // turn off hardware decoding.
                if hardware && hardware_failure {
                    hardware = false;
                    hardware_reason = Some(reason.clone());
                    let message = if graphics.uses_h264() {
                        tracing::warn!(%reason, "hardware decoding failed; reconnecting with OpenH264");
                        "Switching to software video decoding"
                    } else {
                        tracing::warn!(%reason, "GPU drawing failed; reconnecting with CPU drawing");
                        "Switching to software rendering"
                    };
                    events.send(SessionEvent::Reconnecting {
                        attempt: 1,
                        retry_in: Duration::ZERO,
                        reason: message.to_owned(),
                    });
                    continue;
                }
                // A fresh connection restarts the video stream with a keyframe, which fixes a
                // one-off decoding glitch. If it happens again, stop using the pipeline.
                graphics_failures += 1;
                let message = if graphics_failures >= 2 && graphics != GraphicsMode::Compatible {
                    tracing::warn!(%reason, "graphics pipeline failed again; switching to compatible graphics");
                    graphics = GraphicsMode::Compatible;
                    "Switching to compatible graphics"
                } else {
                    tracing::warn!(%reason, "graphics pipeline glitch; reconnecting");
                    "Refreshing the picture"
                };
                events.send(SessionEvent::Reconnecting {
                    attempt: 1,
                    retry_in: Duration::ZERO,
                    reason: message.to_owned(),
                });
                continue;
            }
            Exit::Closed => return events.send(SessionEvent::Ended(Ended::Closed)),
            Exit::ServerEnded(reason) => {
                return events.send(SessionEvent::Ended(Ended::ServerEnded(reason)));
            }
            Exit::Failed(failure) => {
                return events.send(SessionEvent::Ended(Ended::Failed(failure)));
            }
            Exit::Lost(failure) => failure,
        };

        let since = *reconnecting_since.get_or_insert_with(Instant::now);
        if !params.advanced.auto_reconnect || since.elapsed() > RECONNECT_WINDOW {
            return events.send(SessionEvent::Ended(Ended::Failed(failure)));
        }

        attempt += 1;
        let delay = backoff(attempt);
        tracing::info!(attempt, ?delay, reason = %failure.detail, "connection lost, reconnecting");
        events.send(SessionEvent::Reconnecting {
            attempt,
            retry_in: delay,
            reason: failure.title.clone(),
        });
        lost_reason = failure.title;

        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        let network = network_changed(netinfo::routable_addresses());
        tokio::pin!(network);
        loop {
            tokio::select! {
                () = &mut sleep => break,
                () = &mut network => {
                    tracing::debug!("network changed; retrying now");
                    break;
                }
                cmd = cmd_rx.recv() => match cmd {
                    None | Some(SessionCmd::Disconnect) => return events.send(SessionEvent::Ended(Ended::Closed)),
                    Some(SessionCmd::RetryNow) => break,
                    Some(SessionCmd::Resize(request)) => desktop = request,
                    Some(_) => {}
                },
            }
        }
    }
}

/// Starts a reconnect attempt over, keeping the reason the connection was lost on the card.
fn restart_attempt(events: &EventSink, attempt: &mut u32, reason: &str) {
    *attempt += 1;
    events.send(SessionEvent::Reconnecting {
        attempt: *attempt,
        retry_in: Duration::ZERO,
        reason: reason.to_owned(),
    });
}

/// Resolves once the machine's routable addresses differ from `before`: a network or VPN tunnel
/// came up or went away. Never resolves when the addresses can't be read.
async fn network_changed(before: Option<Vec<IpAddr>>) {
    let Some(before) = before else {
        return std::future::pending().await;
    };
    let mut poll = tokio::time::interval(NETWORK_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        poll.tick().await;
        if netinfo::routable_addresses().is_some_and(|now| now != before) {
            return;
        }
    }
}

/// 0.5 s, 1 s, 2 s, 4 s … capped at 10 s. The first retry is quick: most drops are blips.
fn backoff(attempt: u32) -> Duration {
    let millis = 500u64.saturating_mul(1 << attempt.saturating_sub(1).min(16));
    Duration::from_millis(millis).min(MAX_BACKOFF)
}

struct PendingResize {
    request: DesktopRequest,
    at: TokioInstant,
    retries: u32,
}

#[derive(Default)]
struct Counters {
    updates: u32,
    bytes_in: u64,
    bytes_out: u64,
}

struct ActiveSession<'a> {
    params: &'a ConnectParams,
    events: &'a EventSink,
    frame: &'a SharedFrame,
    connection: Option<Connection>,
    gfx: Option<Arc<GfxShared>>,
    clipboard: Option<&'a ClipboardWorker>,
    focused: bool,
    dynamic_resize: bool,
    initial_request: DesktopRequest,
}

impl<'a> ActiveSession<'a> {
    fn new(
        connection: Connection,
        params: &'a ConnectParams,
        desktop: DesktopRequest,
        events: &'a EventSink,
        frame: &'a SharedFrame,
        gfx: Option<Arc<GfxShared>>,
        clipboard: Option<&'a ClipboardWorker>,
    ) -> Self {
        Self {
            params,
            events,
            frame,
            connection: Some(connection),
            gfx,
            clipboard,
            focused: true,
            dynamic_resize: params.dynamic_resize,
            initial_request: desktop,
        }
    }

    async fn run(
        mut self,
        cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
        desktop: &mut DesktopRequest,
    ) -> Exit {
        let Connection {
            result,
            framed,
            socket_fd,
            local_addr,
            os,
        } = self.connection.take().expect("connection present");
        if let Some(os) = os {
            self.events.send(SessionEvent::HostOs(os));
        }
        let port = self.params.port;

        let (mut reader, mut writer) = split_tokio_framed(framed);
        let mut size = result.desktop_size;
        let mut image = DecodedImage::new(PixelFormat::RgbA32, size.width, size.height);
        self.frame
            .lock()
            .resize(u32::from(size.width), u32::from(size.height));
        self.events.send(SessionEvent::Connected {
            width: size.width,
            height: size.height,
        });

        let activation_factory = result.activation_factory;
        let io_channel_id = result.io_channel_id;
        let mut stage: ActiveStage = ActiveStageBuilder {
            static_channels: result.static_channels,
            user_channel_id: result.user_channel_id,
            io_channel_id: result.io_channel_id,
            message_channel_id: result.message_channel_id,
            share_id: result.share_id,
            compression_type: result.compression_type,
            enable_server_pointer: result.enable_server_pointer,
            pointer_software_rendering: result.pointer_software_rendering,
        }
        .build();

        let connected_at = Instant::now();
        let mut first_picture = true;
        let mut closing: Option<TokioInstant> = None;
        let mut pending_resize = self.initial_resize(size);
        let mut counters = Counters::default();
        let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
        stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut outgoing: Vec<u8> = Vec::with_capacity(4096);
        let mut damaged: SmallVec<[Rect; 8]> = SmallVec::new();
        let mut gfx_presented = 0u64;
        let mut last_rendering: Option<Rendering> = None;
        let mut tick_count = 0u64;

        loop {
            let resize_at = pending_resize.as_ref().map(|p| p.at);

            let outputs = tokio::select! {
                pdu = reader.read_pdu() => {
                    let (action, payload) = match pdu {
                        Ok(pdu) => pdu,
                        Err(_) if closing.is_some() => return Exit::Closed,
                        Err(e) => return Exit::Lost(Failure::from_io(&e, port)),
                    };
                    counters.bytes_in += payload.len() as u64;
                    let frames = (action == ironrdp_pdu::Action::X224)
                        .then(|| slowpath::split(&payload, io_channel_id))
                        .flatten();
                    match frames {
                        Some(frames) => frames.iter().try_fold(Vec::new(), |mut outputs, frame| {
                            outputs.extend(stage.process(&mut image, action, frame)?);
                            Ok(outputs)
                        }),
                        None => stage.process(&mut image, action, &payload),
                    }
                }
                cmd = cmd_rx.recv() => match cmd {
                    None | Some(SessionCmd::Disconnect) => {
                        if closing.is_some() {
                            return Exit::Closed;
                        }
                        closing = Some(TokioInstant::now() + CLOSE_GRACE);
                        stage.graceful_shutdown()
                    }
                    Some(SessionCmd::Input(input)) => stage.process_fastpath_input(&mut image, &input),
                    Some(SessionCmd::Resize(request)) => {
                        *desktop = request;
                        if self.dynamic_resize {
                            pending_resize = Some(PendingResize {
                                request,
                                at: TokioInstant::now() + RESIZE_DEBOUNCE,
                                retries: 0,
                            });
                        }
                        Ok(Vec::new())
                    }
                    Some(SessionCmd::RetryNow) => Ok(Vec::new()),
                    Some(SessionCmd::Focus(focused)) => {
                        self.focused = focused;
                        if focused && let Some(clipboard) = self.clipboard {
                            clipboard.check_local(true);
                        }
                        Ok(Vec::new())
                    }
                    Some(SessionCmd::Clipboard(message)) => clipboard_message(&mut stage, message, self.clipboard),
                },
                () = sleep_until(resize_at) => {
                    let mut pending = pending_resize.take().expect("armed");
                    let request = pending.request.normalized();
                    let (width, height) =
                        MonitorLayoutEntry::adjust_display_size(u32::from(request.width), u32::from(request.height));
                    let current = self
                        .gfx
                        .as_ref()
                        .and_then(|g| g.output_size())
                        .unwrap_or((u32::from(size.width), u32::from(size.height)));
                    if (width, height) == current {
                        Ok(Vec::new())
                    } else {
                        match stage.encode_resize(width, height, Some(request.scale_factor), None) {
                            Some(Ok(frame)) => {
                                tracing::debug!(width, height, "requested new desktop size");
                                Ok(vec![ActiveStageOutput::ResponseFrame(frame)])
                            }
                            Some(Err(e)) => Err(e),
                            None if pending.retries < RESIZE_MAX_RETRIES => {
                                // Display Control isn't open yet.
                                pending.retries += 1;
                                pending.at = TokioInstant::now() + RESIZE_RETRY;
                                pending_resize = Some(pending);
                                Ok(Vec::new())
                            }
                            None => {
                                tracing::info!("server doesn't support dynamic resize; scaling instead");
                                self.dynamic_resize = false;
                                Ok(Vec::new())
                            }
                        }
                    }
                }
                () = sleep_until(closing) => return Exit::Closed,
                _ = stats_tick.tick() => {
                    // The kernel keeps a socket whose address is gone until it times out; don't
                    // leave the picture frozen that long.
                    let ip = local_addr.ip();
                    if netinfo::address_assigned(ip) == Some(false) {
                        tracing::info!(%ip, "local address went away; reconnecting");
                        return Exit::Lost(Failure::network_changed(ip));
                    }
                    tick_count += 1;
                    // Text is cheap to check; local copies show up within two seconds.
                    if self.focused && tick_count % 2 == 0 && let Some(clipboard) = self.clipboard {
                        clipboard.check_local(false);
                    }
                    let mut outputs = Vec::new();
                    if let Some(cliprdr) = stage.get_svc_processor_mut::<ironrdp_cliprdr::CliprdrClient>()
                        && let Ok(messages) = cliprdr.drive_timeouts()
                    {
                        match stage.process_svc_processor_messages(messages) {
                            Ok(frame) if !frame.is_empty() => outputs.push(ActiveStageOutput::ResponseFrame(frame)),
                            Ok(_) => {}
                            Err(e) => tracing::debug!(error = %e, "clipboard housekeeping failed"),
                        }
                    }
                    self.events.send(SessionEvent::Stats(Stats {
                        rtt: netinfo::tcp_rtt(socket_fd),
                        updates_per_sec: counters.updates as f32,
                        bytes_in_per_sec: counters.bytes_in,
                        bytes_out_per_sec: counters.bytes_out,
                    }));
                    counters = Counters::default();
                    Ok(outputs)
                }
            };

            let outputs = match outputs {
                Ok(outputs) => outputs,
                Err(e) => {
                    if let Some(failure) = self.gfx.as_ref().and_then(|g| g.take_failure()) {
                        return Exit::GraphicsFallback(failure);
                    }
                    let failure = Failure::from_session(&e, port);
                    tracing::warn!(detail = %failure.detail, "session error");
                    return if failure.is_network() {
                        Exit::Lost(failure)
                    } else {
                        Exit::Failed(failure)
                    };
                }
            };

            for output in outputs {
                match output {
                    ActiveStageOutput::ResponseFrame(bytes) => outgoing.extend_from_slice(&bytes),
                    ActiveStageOutput::GraphicsUpdate(r) => {
                        damaged.push(Rect::new(
                            u32::from(r.left),
                            u32::from(r.top),
                            u32::from(r.right.saturating_sub(r.left)) + 1,
                            u32::from(r.bottom.saturating_sub(r.top)) + 1,
                        ));
                    }
                    ActiveStageOutput::PointerDefault => self
                        .events
                        .send(SessionEvent::Pointer(PointerUpdate::Default)),
                    ActiveStageOutput::PointerHidden => self
                        .events
                        .send(SessionEvent::Pointer(PointerUpdate::Hidden)),
                    ActiveStageOutput::PointerBitmap(pointer) => {
                        self.events
                            .send(SessionEvent::Pointer(PointerUpdate::Bitmap(pointer)));
                    }
                    ActiveStageOutput::PointerPosition { .. } => {}
                    ActiveStageOutput::DeactivateAll => {
                        // Send anything queued before the reactivation exchange starts.
                        if !outgoing.is_empty() {
                            if let Err(e) = writer.write_all(&outgoing).await {
                                return Exit::Lost(Failure::from_io(&e, port));
                            }
                            outgoing.clear();
                        }
                        match reactivate(
                            &activation_factory,
                            &mut reader,
                            &mut writer,
                            &mut stage,
                            port,
                        )
                        .await
                        {
                            Ok(new_size) => {
                                size = new_size;
                                image =
                                    DecodedImage::new(PixelFormat::RgbA32, size.width, size.height);
                                damaged.clear();
                                self.frame
                                    .lock()
                                    .resize(u32::from(size.width), u32::from(size.height));
                                self.events.send(SessionEvent::DesktopResized {
                                    width: size.width,
                                    height: size.height,
                                });
                            }
                            Err(exit) => return exit,
                        }
                    }
                    ActiveStageOutput::Terminate(reason) => {
                        return match reason {
                            _ if closing.is_some() => Exit::Closed,
                            GracefulDisconnectReason::UserInitiated => Exit::Closed,
                            other => Exit::ServerEnded(describe_disconnect(&other)),
                        };
                    }
                    ActiveStageOutput::MultitransportRequest(_)
                    | ActiveStageOutput::AutoDetect(_) => {}
                }
            }

            if !outgoing.is_empty() {
                counters.bytes_out += outgoing.len() as u64;
                if let Err(e) = writer.write_all(&outgoing).await {
                    return Exit::Lost(Failure::from_io(&e, port));
                }
                outgoing.clear();
            }

            // Pictures arrive either as legacy updates (decoded into `image`) or through the
            // graphics pipeline, which composites straight into the shared frame.
            let mut pictured = false;
            let mut rendering = None;
            if let Some(gfx) = &self.gfx {
                if let Some(failure) = gfx.take_failure() {
                    return Exit::GraphicsFallback(failure);
                }
                let presented = gfx.presented();
                pictured = presented != gfx_presented;
                gfx_presented = presented;
                if pictured {
                    rendering = Some(gfx.rendering());
                }
            }
            if !damaged.is_empty() {
                // Legacy updates: Compatible mode, or the server didn't take up the pipeline.
                rendering.get_or_insert_with(Rendering::legacy);
                let mut shared = self.frame.lock();
                for rect in damaged.drain(..) {
                    shared.blit_rgba(image.data(), image.stride(), rect);
                }
                drop(shared);
                pictured = true;
                if self.frame.request_wake() {
                    self.events.wake();
                }
            }
            if let Some(rendering) = rendering
                && last_rendering.as_ref() != Some(&rendering)
            {
                tracing::debug!(?rendering, "rendering");
                last_rendering = Some(rendering.clone());
                self.events.send(SessionEvent::Rendering(rendering));
            }
            if pictured {
                counters.updates += 1;
                if first_picture {
                    first_picture = false;
                    self.events.send(SessionEvent::StageCompleted(
                        Stage::Desktop,
                        connected_at.elapsed(),
                    ));
                }
            }
        }
    }

    /// If the window changed size while we were connecting, ask for the new size right away.
    fn initial_resize(&self, negotiated: ironrdp_connector::DesktopSize) -> Option<PendingResize> {
        let wanted = self.initial_request.normalized();
        (self.dynamic_resize
            && (wanted.width != negotiated.width || wanted.height != negotiated.height))
            .then(|| PendingResize {
                request: wanted,
                at: TokioInstant::now() + RESIZE_DEBOUNCE,
                retries: 0,
            })
    }
}

/// Turns a request from the clipboard backend into protocol messages.
fn clipboard_message(
    stage: &mut ActiveStage,
    message: ironrdp_cliprdr::backend::ClipboardMessage,
    clipboard: Option<&ClipboardWorker>,
) -> ironrdp_session::SessionResult<Vec<ActiveStageOutput>> {
    use ironrdp_cliprdr::backend::ClipboardMessage as M;

    // Something is waiting for the answer to a file request; tell it the request is lost.
    let file_request = match &message {
        M::SendFileContentsRequest(request) => Some(request.stream_id),
        _ => None,
    };
    let dropped = || {
        if let (Some(stream_id), Some(clipboard)) = (file_request, clipboard) {
            clipboard.file_request_failed(stream_id);
        }
    };

    let Some(cliprdr) = stage.get_svc_processor_mut::<ironrdp_cliprdr::CliprdrClient>() else {
        dropped();
        return Ok(Vec::new());
    };
    let messages = match message {
        M::SendInitiateCopy(formats) => cliprdr.initiate_copy(&formats),
        M::SendFormatData(response) => cliprdr.submit_format_data(response),
        M::SendInitiatePaste(format) => cliprdr.initiate_paste(format),
        M::SendFileContentsRequest(request) => cliprdr.request_file_contents(request),
        M::SendFileContentsResponse(response) => cliprdr.submit_file_contents(response),
        M::SendInitiateFileCopy(files) => cliprdr.initiate_file_copy(files),
        M::Error(e) => {
            tracing::warn!(error = %e, "clipboard error");
            return Ok(Vec::new());
        }
    };
    match messages {
        Ok(messages) => Ok(vec![ActiveStageOutput::ResponseFrame(
            stage.process_svc_processor_messages(messages)?,
        )]),
        // Clipboard trouble (e.g. the channel isn't ready yet) must never end the session.
        Err(e) => {
            tracing::debug!(error = %e, "clipboard request dropped");
            dropped();
            Ok(Vec::new())
        }
    }
}

/// Deactivation-Reactivation Sequence (MS-RDPBCGR 1.3.1.3): the server renegotiates
/// capabilities, typically after a resolution change.
async fn reactivate<R, W>(
    factory: &ironrdp_connector::connection_activation::ConnectionActivationFactory,
    reader: &mut ironrdp_tokio::Framed<R>,
    writer: &mut ironrdp_tokio::Framed<W>,
    stage: &mut ActiveStage,
    port: u16,
) -> Result<ironrdp_connector::DesktopSize, Exit>
where
    R: ironrdp_tokio::FramedRead,
    W: ironrdp_tokio::FramedWrite,
{
    tracing::debug!("running deactivation-reactivation sequence");
    let mut activation = factory.create();
    let mut buf = WriteBuf::new();
    loop {
        let written = single_sequence_step_read(reader, &mut activation, &mut buf)
            .await
            .map_err(|e| Exit::Lost(Failure::from_connector(&e, port)))?;
        if written.size().is_some() {
            writer
                .write_all(buf.filled())
                .await
                .map_err(|e| Exit::Lost(Failure::from_io(&e, port)))?;
        }

        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            enable_server_pointer,
            pointer_software_rendering,
        } = activation.connection_activation_state()
        {
            stage.set_fastpath_processor(
                fast_path::ProcessorBuilder {
                    io_channel_id: activation.io_channel_id(),
                    user_channel_id: activation.user_channel_id(),
                    share_id,
                    enable_server_pointer,
                    pointer_software_rendering,
                    bulk_decompressor: None,
                }
                .build(),
            );
            stage.set_share_id(share_id);
            stage.set_enable_server_pointer(enable_server_pointer);
            tracing::debug!(?desktop_size, "reactivation complete");
            return Ok(desktop_size);
        }
    }
}

async fn sleep_until(deadline: Option<TokioInstant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn describe_disconnect(reason: &GracefulDisconnectReason) -> String {
    match reason {
        GracefulDisconnectReason::UserInitiated => "You disconnected.".to_owned(),
        GracefulDisconnectReason::ServerInitiated => {
            "The remote computer ended the session.".to_owned()
        }
        GracefulDisconnectReason::Other(description) => {
            let mut text = description.trim().to_owned();
            if let Some(first) = text.get(0..1) {
                let upper = first.to_uppercase();
                text.replace_range(0..1, &upper);
            }
            if !text.ends_with('.') {
                text.push('.');
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_secs(1));
        assert_eq!(backoff(4), Duration::from_secs(4));
        assert_eq!(backoff(10), MAX_BACKOFF);
        assert_eq!(backoff(100), MAX_BACKOFF);
    }
}
