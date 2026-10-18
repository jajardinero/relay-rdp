//! Desktop integration test: local RDP servers, no production credentials or
//! saved host settings. Run explicitly on a Wayland desktop with a VA-API GPU.
use super::*;
use crate::store::profiles::{AudioMode, DeviceSettings, ExperienceSettings, GraphicsMode};
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

struct Server {
    child: Child,
    port: u16,
}
impl Server {
    fn start() -> Self {
        Self::launch(&["--gfx"])
    }

    /// `--gfx` streams H.264 4:2:0; `--gfx444` streams 4:4:4 to clients that accept it.
    /// `--annex-b` frames H.264 with start codes, as Windows does.
    fn launch(args: &[&str]) -> Self {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/test_server");
        let mut child = Command::new(binary)
            .args(args)
            .args(["--port", &port.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("build the test_server example first");
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert!(line.contains("test server on"), "server failed: {line}");
        // Keep draining server logs so a busy encoder cannot fill its pipe.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        });
        std::thread::sleep(Duration::from_millis(100));
        Self { child, port }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("relay-test-server-{}", self.child.id())),
        );
    }
}

/// Logging, TLS, and a proxy to the process's one event loop (winit allows only one per
/// process, so the tests share it; it is never run).
fn setup() -> winit::event_loop::EventLoopProxy<UserEvent> {
    use std::sync::OnceLock;
    use winit::platform::wayland::EventLoopBuilderExtWayland as _;
    static PROXY: OnceLock<std::sync::Mutex<winit::event_loop::EventLoopProxy<UserEvent>>> =
        OnceLock::new();
    let _ = tracing_subscriber::fmt()
        .with_env_filter("relay=debug")
        .with_test_writer()
        .try_init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let proxy = PROXY.get_or_init(|| {
        let event_loop = winit::event_loop::EventLoop::<UserEvent>::with_user_event()
            .with_wayland()
            .with_any_thread(true)
            .build()
            .unwrap();
        let proxy = event_loop.create_proxy();
        std::mem::forget(event_loop);
        std::sync::Mutex::new(proxy)
    });
    proxy.lock().unwrap().clone()
}

#[test]
#[ignore = "requires a Wayland desktop, VA-API/Vulkan GPU, and cargo build --example test_server"]
fn wayland_rdp_sessions_resize_and_toggle() {
    let proxy = setup();
    let gpu = crate::gpu::tests::context().unwrap();
    // Session 0 decodes length-prefixed H.264 on the GPU; 1 (GPU) and 2 (CPU) get Annex B.
    let servers = [
        Server::start(),
        Server::launch(&["--gfx", "--annex-b"]),
        Server::launch(&["--gfx", "--annex-b"]),
    ];
    let make_params = |port, hardware| ConnectParams {
        host: "127.0.0.1".to_owned(),
        port,
        username: "test".to_owned(),
        domain: String::new(),
        password: Zeroizing::new("test".to_owned()),
        desktop: DesktopRequest {
            width: 640,
            height: 480,
            scale_factor: 100,
        },
        dynamic_resize: true,
        experience: Default::default(),
        advanced: Default::default(),
        devices: DeviceSettings {
            clipboard: false,
            audio: AudioMode::Off,
            shared_folders: Vec::new(),
        },
        known_hosts: Arc::new(Mutex::new(KnownHosts::in_memory())),
        hardware_video_decoding: hardware,
        gpu: Some(gpu.clone()),
    };
    let mut sessions: Vec<_> = servers
        .iter()
        .enumerate()
        .map(|(i, s)| SessionHandle::spawn(make_params(s.port, i != 2), proxy.clone()).unwrap())
        .collect();
    let mut live = [false; 3];
    let mut hardware_seen = [false; 3];
    let wait = |sessions: &[SessionHandle],
                live: &mut [bool; 3],
                hardware_seen: &mut [bool; 3],
                sizes: &[(u32, u32); 3]| {
        let start = Instant::now();
        loop {
            for (i, session) in sessions.iter().enumerate() {
                while let Some(event) = session.try_event() {
                    match event {
                        SessionEvent::Certificate(prompt) => {
                            let _ = prompt.reply.send(true);
                        }
                        SessionEvent::StageCompleted(Stage::Desktop, _) => live[i] = true,
                        SessionEvent::Rendering(rendering) => {
                            eprintln!("session {i}: {rendering:?}");
                            if rendering.engine == Engine::Gpu && rendering.codec == Codec::H264 {
                                hardware_seen[i] = true;
                            }
                        }
                        SessionEvent::Ended(reason) => panic!("session {i} ended: {reason:?}"),
                        SessionEvent::Reconnecting { reason, .. } => {
                            panic!("session {i} unexpectedly reconnecting: {reason}")
                        }
                        _ => {}
                    }
                }
            }
            if sessions.iter().enumerate().all(|(i, s)| {
                let frame = s.frame.lock();
                live[i]
                    && (frame.width(), frame.height()) == sizes[i]
                    && frame.gpu.is_some() == (i != 2)
            }) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting for RDP pictures"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait(&sessions, &mut live, &mut hardware_seen, &[(640, 480); 3]);
    assert!(hardware_seen[0] && hardware_seen[1] && !hardware_seen[2]);
    let sizes = [(800, 600), (640, 512), (800, 600)];
    for (session, &(width, height)) in sessions.iter().zip(&sizes) {
        session.send(SessionCmd::Resize(DesktopRequest {
            width: width as u16,
            height: height as u16,
            scale_factor: 100,
        }));
    }
    wait(&sessions, &mut live, &mut hardware_seen, &sizes);
    // An active GPU-path failure must obtain a fresh stream before selecting
    // OpenH264, without consuming the existing legacy-graphics retry budget.
    crate::gpu::tests::simulate_failure(&gpu);
    let mut reconnects = [0; 3];
    let mut fallback_seen = [false; 3];
    let mut recovered_picture = [false, false, true];
    let recovery = Instant::now();
    loop {
        for (i, session) in sessions.iter().enumerate() {
            while let Some(event) = session.try_event() {
                match event {
                    SessionEvent::Certificate(prompt) => {
                        let _ = prompt.reply.send(true);
                    }
                    SessionEvent::Reconnecting { reason, .. } => {
                        eprintln!("session {i}: {reason}");
                        reconnects[i] += 1;
                        recovered_picture[i] = false;
                        assert_eq!(reconnects[i], 1, "repeated hardware fallback");
                    }
                    SessionEvent::Rendering(rendering) if rendering.engine == Engine::Cpu => {
                        fallback_seen[i] = rendering.fallback.as_deref()
                            == Some("injected hardware decoder failure");
                    }
                    SessionEvent::StageCompleted(Stage::Desktop, _) if reconnects[i] > 0 => {
                        recovered_picture[i] = true
                    }
                    SessionEvent::Ended(reason) => panic!("fallback session {i} ended: {reason:?}"),
                    _ => {}
                }
            }
        }
        if fallback_seen[0]
            && fallback_seen[1]
            && recovered_picture.iter().all(|&pictured| pictured)
            && sessions.iter().enumerate().all(|(i, s)| {
                let frame = s.frame.lock();
                frame.gpu.is_none() && (frame.width(), frame.height()) == sizes[i]
            })
        {
            break;
        }
        assert!(
            recovery.elapsed() < Duration::from_secs(15),
            "hardware fallback did not recover"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(reconnects, [1, 1, 0]);
    for session in &sessions {
        session.send(SessionCmd::Disconnect);
    }
    let stop = Instant::now();
    for session in &mut sessions {
        loop {
            if let Some(SessionEvent::Ended(reason)) = session.try_event() {
                assert!(
                    matches!(reason, Ended::Closed),
                    "disconnect ended unexpectedly: {reason:?}"
                );
                break;
            }
            assert!(
                stop.elapsed() < Duration::from_secs(10),
                "disconnect timeout for {:?}",
                session.id
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // A fresh connection snapshots the changed preference independently of old
    // sessions. Use a fresh server to avoid the example's single-user state.
    let server = Server::start();
    let software = SessionHandle::spawn(make_params(server.port, false), proxy.clone()).unwrap();
    let start = Instant::now();
    loop {
        if let Some(event) = software.try_event() {
            match event {
                SessionEvent::Certificate(prompt) => {
                    let _ = prompt.reply.send(true);
                }
                SessionEvent::StageCompleted(Stage::Desktop, _) => break,
                SessionEvent::Ended(reason) => panic!("software reconnect ended: {reason:?}"),
                _ => {}
            }
        }
        assert!(start.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(software.frame.lock().gpu.is_none());
    software.send(SessionCmd::Disconnect);

    // Initialization failure must fall back before negotiating the stream,
    // without reconnecting or requiring the user to change the preference.
    let unavailable_server = Server::start();
    let mut unavailable_gpu = crate::gpu::tests::context().unwrap();
    Arc::get_mut(&mut unavailable_gpu).unwrap().render_node =
        Some("/dev/dri/relay-unavailable-device".to_owned());
    let mut params = make_params(unavailable_server.port, true);
    params.gpu = Some(unavailable_gpu);
    let unavailable = SessionHandle::spawn(params, proxy.clone()).unwrap();
    let mut fallback_reason = false;
    let mut pictured = false;
    let start = Instant::now();
    // The first picture may be a classic update sent before the graphics pipeline is up, so
    // wait for the pipeline to report the reason too.
    while !(pictured && fallback_reason) {
        if let Some(event) = unavailable.try_event() {
            match event {
                SessionEvent::Certificate(prompt) => {
                    let _ = prompt.reply.send(true);
                }
                SessionEvent::Rendering(rendering) => {
                    fallback_reason |=
                        rendering.engine == Engine::Cpu && rendering.fallback.is_some();
                }
                SessionEvent::StageCompleted(Stage::Desktop, _) => pictured = true,
                SessionEvent::Reconnecting { reason, .. } => {
                    panic!("initialization fallback reconnected: {reason}")
                }
                SessionEvent::Ended(reason) => panic!("initialization fallback ended: {reason:?}"),
                _ => {}
            }
        }
        assert!(start.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(unavailable.frame.lock().gpu.is_none());
    unavailable.send(SessionCmd::Disconnect);
}

#[test]
#[ignore = "requires a Wayland desktop, VA-API/Vulkan GPU, and cargo build --example test_server"]
fn wayland_rdp_full_color() {
    let proxy = setup();
    let gpu = crate::gpu::tests::context().unwrap();
    // Full color with hardware decoding, Full color in software, and Auto (which must keep
    // getting 4:2:0 from the same kind of server).
    let cases = [
        (GraphicsMode::FullColor, true, Engine::Gpu, Codec::H264Full),
        (GraphicsMode::FullColor, false, Engine::Cpu, Codec::H264Full),
        (GraphicsMode::Auto, true, Engine::Gpu, Codec::H264),
    ];
    // The software case gets Annex B H.264, as Windows sends it.
    let servers: Vec<_> = cases
        .iter()
        .map(|&(_, hardware, _, _)| match hardware {
            true => Server::launch(&["--gfx444"]),
            false => Server::launch(&["--gfx444", "--annex-b"]),
        })
        .collect();
    let sessions: Vec<_> = servers
        .iter()
        .zip(&cases)
        .map(|(server, &(graphics, hardware, _, _))| {
            let params = ConnectParams {
                host: "127.0.0.1".to_owned(),
                port: server.port,
                username: "test".to_owned(),
                domain: String::new(),
                password: Zeroizing::new("test".to_owned()),
                desktop: DesktopRequest {
                    width: 640,
                    height: 480,
                    scale_factor: 100,
                },
                dynamic_resize: true,
                experience: ExperienceSettings {
                    graphics,
                    ..Default::default()
                },
                advanced: Default::default(),
                devices: DeviceSettings {
                    clipboard: false,
                    audio: AudioMode::Off,
                    shared_folders: Vec::new(),
                },
                known_hosts: Arc::new(Mutex::new(KnownHosts::in_memory())),
                hardware_video_decoding: hardware,
                gpu: Some(gpu.clone()),
            };
            SessionHandle::spawn(params, proxy.clone()).unwrap()
        })
        .collect();
    let mut seen = [false; 3];
    let start = Instant::now();
    while !seen.iter().all(|&s| s) {
        for (i, session) in sessions.iter().enumerate() {
            while let Some(event) = session.try_event() {
                match event {
                    SessionEvent::Certificate(prompt) => {
                        let _ = prompt.reply.send(true);
                    }
                    SessionEvent::Rendering(rendering) => {
                        eprintln!("session {i}: {rendering:?}");
                        let (_, _, engine, codec) = cases[i];
                        assert_ne!(
                            rendering.codec,
                            if codec == Codec::H264 {
                                Codec::H264Full
                            } else {
                                Codec::H264
                            },
                            "session {i} received the wrong H.264 format"
                        );
                        seen[i] |= rendering.engine == engine && rendering.codec == codec;
                    }
                    SessionEvent::Ended(reason) => panic!("session {i} ended: {reason:?}"),
                    SessionEvent::Reconnecting { reason, .. } => {
                        panic!("session {i} unexpectedly reconnecting: {reason}")
                    }
                    _ => {}
                }
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "timed out waiting for H.264 pictures: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Keep streaming for a while to catch failures in predicted pictures.
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(2) {
        for (i, session) in sessions.iter().enumerate() {
            while let Some(event) = session.try_event() {
                match event {
                    SessionEvent::Ended(reason) => panic!("session {i} ended: {reason:?}"),
                    SessionEvent::Reconnecting { reason, .. } => {
                        panic!("session {i} unexpectedly reconnecting: {reason}")
                    }
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for (i, session) in sessions.iter().enumerate() {
        let frame = session.frame.lock();
        assert_eq!((frame.width(), frame.height()), (640, 480));
        assert_eq!(frame.gpu.is_some(), cases[i].1, "session {i}");
    }
    for session in &sessions {
        session.send(SessionCmd::Disconnect);
    }
}
