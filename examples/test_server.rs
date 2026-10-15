//! A local RDP server for exercising Relay without a Windows machine.
//!
//! It requires NLA (CredSSP) with user `test` / password `test`, presents a fresh self-signed
//! certificate each run (so the trust prompt appears), animates a scene so you can judge
//! smoothness, draws the pointer and clicks it receives, flashes a bar for every key, and
//! follows client-requested resolution changes.
//!
//! With `--gfx`, the picture is H.264-encoded (OpenH264, from the system library) and streamed
//! over the graphics pipeline, as Windows does, instead of RemoteFX updates.
//!
//!     cargo run --example test_server -- [--port 3390] [--user test] [--pass test] [--gfx]

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    BitmapUpdate, Credentials, DesktopSize, DisplayUpdate, EgfxServerMessage, GfxDvcBridge,
    GfxServerFactory, GfxServerHandle, KeyboardEvent, MouseEvent, PixelFormat, RdpServer,
    RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler, ServerEvent,
    ServerEventSender, TlsIdentityCtx,
};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType, VuiConfig};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

const FRAME: Duration = Duration::from_millis(16);
const BOX: u16 = 96;

struct Scene {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
    started: Instant,
    box_pos: (u16, u16),
    pointer: (u16, u16),
    key_flash: Option<(Instant, [u8; 3])>,
    pending_resize: Option<DesktopSize>,
    dirty: Vec<(u16, u16, u16, u16)>,
}

impl Scene {
    fn new(width: u16, height: u16) -> Self {
        let mut scene = Self {
            width,
            height,
            pixels: Vec::new(),
            started: Instant::now(),
            box_pos: (0, 0),
            pointer: (0, 0),
            key_flash: None,
            pending_resize: None,
            dirty: Vec::new(),
        };
        scene.resize(width, height);
        scene
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.pixels = vec![0; usize::from(width) * usize::from(height) * 4];
        for y in 0..height {
            for x in 0..width {
                let c = background(x, y, width, height);
                self.put(x, y, c);
            }
        }
        self.dirty.clear();
        self.dirty.push((0, 0, width, height));
    }

    fn put(&mut self, x: u16, y: u16, [r, g, b]: [u8; 3]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (usize::from(y) * usize::from(self.width) + usize::from(x)) * 4;
        self.pixels[i..i + 4].copy_from_slice(&[b, g, r, 0xFF]);
    }

    fn fill(&mut self, x: u16, y: u16, w: u16, h: u16, color: Option<[u8; 3]>) {
        let x1 = x.saturating_add(w).min(self.width);
        let y1 = y.saturating_add(h).min(self.height);
        for yy in y..y1 {
            for xx in x..x1 {
                let c = color.unwrap_or_else(|| background(xx, yy, self.width, self.height));
                self.put(xx, yy, c);
            }
        }
        if x < x1 && y < y1 {
            self.dirty.push((x, y, x1 - x, y1 - y));
        }
    }

    /// Advances the animation: a box bouncing along a Lissajous path.
    fn tick(&mut self) {
        let t = self.started.elapsed().as_secs_f32();
        let (old_x, old_y) = self.box_pos;
        self.fill(old_x, old_y, BOX, BOX, None);

        let span_x = f32::from(self.width.saturating_sub(BOX));
        let span_y = f32::from(self.height.saturating_sub(BOX + 40));
        let x = ((t * 0.9).sin() * 0.5 + 0.5) * span_x;
        let y = ((t * 1.3).cos() * 0.5 + 0.5) * span_y;
        self.box_pos = (x as u16, y as u16);
        let hue = (t * 40.0) % 360.0;
        self.fill(self.box_pos.0, self.box_pos.1, BOX, BOX, Some(hsv(hue)));

        // Key flash bar along the bottom.
        let bar_y = self.height.saturating_sub(24);
        match self.key_flash {
            Some((at, color)) if at.elapsed() < Duration::from_millis(250) => {
                self.fill(0, bar_y, self.width, 24, Some(color));
            }
            Some(_) => {
                self.key_flash = None;
                self.fill(0, bar_y, self.width, 24, None);
            }
            None => {}
        }
    }

    fn take_dirty(&mut self) -> Option<(u16, u16, u16, u16)> {
        let first = *self.dirty.first()?;
        let bounds = self
            .dirty
            .drain(..)
            .fold(first, |(x, y, w, h), (x2, y2, w2, h2)| {
                let left = x.min(x2);
                let top = y.min(y2);
                let right = (x + w).max(x2 + w2);
                let bottom = (y + h).max(y2 + h2);
                (left, top, right - left, bottom - top)
            });
        Some(bounds)
    }

    fn region(&self, (x, y, w, h): (u16, u16, u16, u16)) -> BitmapUpdate {
        let stride = usize::from(w) * 4;
        let mut data = Vec::with_capacity(stride * usize::from(h));
        for row in y..y + h {
            let start = (usize::from(row) * usize::from(self.width) + usize::from(x)) * 4;
            data.extend_from_slice(&self.pixels[start..start + stride]);
        }
        BitmapUpdate {
            x,
            y,
            width: NonZeroU16::new(w).expect("non-empty"),
            height: NonZeroU16::new(h).expect("non-empty"),
            format: PixelFormat::BgrA32,
            data: data.into(),
            stride: NonZeroUsize::new(stride).expect("non-empty"),
        }
    }
}

fn background(x: u16, y: u16, width: u16, height: u16) -> [u8; 3] {
    // A calm diagonal gradient with a 64 px grid, so scaling artefacts are easy to spot.
    let fx = f32::from(x) / f32::from(width.max(1));
    let fy = f32::from(y) / f32::from(height.max(1));
    let base = [
        (20.0 + 30.0 * fx) as u8,
        (28.0 + 20.0 * fy) as u8,
        (48.0 + 40.0 * (1.0 - fx)) as u8,
    ];
    if x % 64 == 0 || y % 64 == 0 {
        [base[0] + 18, base[1] + 18, base[2] + 18]
    } else {
        base
    }
}

fn hsv(h: f32) -> [u8; 3] {
    let c = 0.85;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + 0.1) * 255.0) as u8,
        ((g + 0.1) * 255.0) as u8,
        ((b + 0.1) * 255.0) as u8,
    ]
}

#[derive(Clone)]
struct Handler {
    scene: Arc<Mutex<Scene>>,
    wake: Arc<Notify>,
    /// While the graphics pipeline streams the picture, legacy updates pause.
    gfx_streaming: Arc<AtomicBool>,
}

impl RdpServerInputHandler for Handler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        println!("key   {event:?}");
        if let KeyboardEvent::Pressed { code, .. } = event {
            let color = hsv(f32::from(code) * 37.0 % 360.0);
            self.scene.lock().key_flash = Some((Instant::now(), color));
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let mut scene = self.scene.lock();
        match event {
            MouseEvent::Move { x, y } => scene.pointer = (x, y),
            MouseEvent::LeftPressed | MouseEvent::RightPressed | MouseEvent::MiddlePressed => {
                let (x, y) = scene.pointer;
                let color = if matches!(event, MouseEvent::LeftPressed) {
                    [240, 240, 240]
                } else {
                    [255, 120, 80]
                };
                scene.fill(x.saturating_sub(4), y.saturating_sub(4), 9, 9, Some(color));
                println!("mouse {event:?} at {x},{y}");
            }
            other => println!("mouse {other:?}"),
        }
    }
}

struct Updates {
    handler: Handler,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        loop {
            // The lock guard must not live across an await, so decide first, then wait.
            let update = {
                let mut scene = self.handler.scene.lock();
                if let Some(size) = scene.pending_resize.take() {
                    println!("resize to {}x{}", size.width, size.height);
                    scene.resize(size.width, size.height);
                    Some(DisplayUpdate::Resize(size))
                } else if self.handler.gfx_streaming.load(Ordering::Acquire) {
                    None
                } else {
                    scene.tick();
                    scene
                        .take_dirty()
                        .map(|region| DisplayUpdate::Bitmap(scene.region(region)))
                }
            };
            if update.is_some() {
                return Ok(update);
            }
            tokio::select! {
                () = tokio::time::sleep(FRAME) => {}
                () = self.handler.wake.notified() => {}
            }
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for Handler {
    async fn size(&mut self) -> DesktopSize {
        let scene = self.scene.lock();
        DesktopSize {
            width: scene.width,
            height: scene.height,
        }
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        println!(
            "client asked for {}x{}",
            client_size.width, client_size.height
        );
        let mut scene = self.scene.lock();
        scene.resize(client_size.width, client_size.height);
        client_size
    }

    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        let mut scene = self.scene.lock();
        let (w, h) = (scene.width, scene.height);
        scene.dirty.push((0, 0, w, h));
        Ok(Box::new(Updates {
            handler: self.clone(),
        }))
    }

    fn request_layout(&mut self, layout: ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout) {
        if let Some(monitor) = layout.monitors().first() {
            let (width, height) = monitor.dimensions();
            let size = DesktopSize {
                width: u16::try_from(width).unwrap_or(1920),
                height: u16::try_from(height).unwrap_or(1080),
            };
            self.scene.lock().pending_resize = Some(size);
            self.wake.notify_one();
        }
    }
}

// ── Graphics pipeline (H.264) ─────────────────────────────────────────────────

struct GfxLog;

impl GraphicsPipelineHandler for GfxLog {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        println!("gfx: client offers {pdu:?}");
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        println!("gfx: ready with {negotiated:?}");
    }
}

#[derive(Clone, Default)]
struct GfxFactory {
    handle: Arc<std::sync::Mutex<Option<GfxServerHandle>>>,
    sender: Arc<std::sync::Mutex<Option<UnboundedSender<ServerEvent>>>>,
}

impl ServerEventSender for GfxFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

impl GfxServerFactory for GfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GfxLog)
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server = Arc::new(std::sync::Mutex::new(GraphicsPipelineServer::new(
            Box::new(GfxLog),
        )));
        *self.handle.lock().unwrap() = Some(Arc::clone(&server));
        Some((GfxDvcBridge::new(Arc::clone(&server)), server))
    }
}

fn make_encoder() -> anyhow::Result<Encoder> {
    // SAFETY: the system library is OpenH264 2.6, the ABI the bindings target.
    let api =
        unsafe { openh264::OpenH264API::from_blob_path_unchecked("/usr/lib/libopenh264.so.8")? };
    let config = EncoderConfig::new()
        .vui(VuiConfig::bt601())
        .usage_type(UsageType::ScreenContentRealTime)
        .bitrate(BitRate::from_bps(12_000_000))
        .max_frame_rate(FrameRate::from_hz(60.0));
    Ok(Encoder::with_api_config(api, config)?)
}

/// Annex B (start codes) to AVC (4-byte big-endian lengths), the framing RDP uses.
fn annex_b_to_avc(annex_b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annex_b.len() + 16);
    for nal in openh264::nal_units(annex_b) {
        let start = nal
            .iter()
            .position(|&b| b != 0)
            .map_or(nal.len(), |i| i + 1);
        let body = &nal[start.min(nal.len())..];
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
    }
    out
}

async fn stream_gfx(factory: GfxFactory, handler: Handler) -> anyhow::Result<()> {
    let mut encoder: Option<(Encoder, u16, u16)> = None;
    let mut surface: Option<(u16, u16, u16)> = None;
    let mut previous_server: Option<GfxServerHandle> = None;
    let started = Instant::now();
    loop {
        tokio::time::sleep(FRAME).await;
        let handle = factory.handle.lock().unwrap().clone();
        let sender = factory.sender.lock().unwrap().clone();
        let (Some(handle), Some(sender)) = (handle, sender) else {
            continue;
        };

        // A reconnect creates a new protocol instance even when the desktop
        // size is unchanged. Its surfaces and video reference frames are fresh.
        if previous_server
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(previous, &handle))
        {
            surface = None;
            encoder = None;
            previous_server = Some(Arc::clone(&handle));
        }
        let mut scene = handler.scene.lock();
        let mut server = handle.lock().unwrap();
        if !server.is_ready() || !server.supports_avc420() {
            handler.gfx_streaming.store(false, Ordering::Release);
            surface = None;
            continue;
        }
        let (w, h) = (scene.width & !1, scene.height & !1);

        if surface.is_none_or(|(_, sw, sh)| (sw, sh) != (w, h)) {
            if let Some((id, _, _)) = surface.take() {
                server.delete_surface(id);
            }
            server.resize(w, h);
            let Some(id) = server.create_surface(w, h) else {
                continue;
            };
            server.map_surface_to_output(id, 0, 0);
            surface = Some((id, w, h));
            // A new surface (new client or new size) must start with a keyframe carrying the
            // parameter sets, so start a fresh encoder.
            encoder = None;
            println!("gfx: streaming {w}x{h} as H.264");
            handler.gfx_streaming.store(true, Ordering::Release);
        }
        if encoder
            .as_ref()
            .is_none_or(|(_, ew, eh)| (*ew, *eh) != (w, h))
        {
            encoder = Some((make_encoder()?, w, h));
        }

        // Never encode a frame that can't be sent: dropping an encoded frame breaks the
        // reference chain for every frame after it.
        if server.should_backpressure() {
            continue;
        }
        scene.tick();
        scene.dirty.clear();
        let yuv = YUVBuffer::from_rgb8_source(BgraSliceU8::new(
            &scene.pixels[..usize::from(w) * usize::from(h) * 4],
            (usize::from(w), usize::from(h)),
        ));
        let (enc, _, _) = encoder.as_mut().expect("just created");
        let bitstream = enc.encode(&yuv)?.to_vec();
        let avc = annex_b_to_avc(&bitstream);
        let region = Avc420Region {
            left: 0,
            top: 0,
            right: w - 1,
            bottom: h - 1,
            quantization_parameter: 22,
            quality: 100,
        };
        let (id, _, _) = surface.expect("surface ready");
        let ts = started.elapsed().as_millis() as u32;
        if server.send_avc420_frame(id, &avc, &[region], ts).is_none() {
            enc.force_intra_frame();
            continue;
        }
        let Some(channel_id) = server.channel_id() else {
            continue;
        };
        let messages = ironrdp_dvc::encode_dvc_messages(
            channel_id,
            server.drain_output(),
            ironrdp_svc::ChannelFlags::empty(),
        )?;
        let _ = sender.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
            messages,
        }));
    }
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let port: u16 = arg("--port")
        .map_or(Ok(3390), |p| p.parse())
        .context("--port")?;
    let user = arg("--user").unwrap_or_else(|| "test".to_owned());
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pass = arg("--pass").unwrap_or_else(|| "test".to_owned());

    let dir = std::env::temp_dir().join(format!("relay-test-server-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, certified.cert.pem())?;
    std::fs::write(&key_path, certified.signing_key.serialize_pem())?;
    let identity = TlsIdentityCtx::init_from_paths(&cert_path, &key_path)?;
    let acceptor = identity.make_acceptor()?;

    let handler = Handler {
        scene: Arc::new(Mutex::new(Scene::new(1280, 800))),
        wake: Arc::new(Notify::new()),
        gfx_streaming: Arc::new(AtomicBool::new(false)),
    };

    let gfx = std::env::args()
        .any(|a| a == "--gfx")
        .then(GfxFactory::default);
    if let Some(factory) = &gfx {
        let (factory, handler) = (factory.clone(), handler.clone());
        tokio::spawn(async move {
            if let Err(e) = stream_gfx(factory, handler).await {
                eprintln!("gfx streaming stopped: {e:#}");
            }
        });
    }

    let mut server = RdpServer::builder()
        .with_addr(([127, 0, 0, 1], port))
        .with_hybrid(acceptor, identity.pub_key)
        .with_input_handler(handler.clone())
        .with_display_handler(handler)
        .with_honor_client_desktop_size(true)
        .with_gfx_factory(gfx.map(|f| Box::new(f) as Box<dyn GfxServerFactory>))
        .build();
    server.set_credentials(Some(Credentials {
        username: user.clone(),
        password: pass,
        domain: None,
    }));

    println!("test server on 127.0.0.1:{port} (user {user})");
    server.run().await
}
