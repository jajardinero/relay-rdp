//! Our backend talking to IronRDP's server side of CLIPRDR in-process, with real PDUs: the
//! handshake, then files copied each way. Only the Wayland clipboard is left out.

use std::time::{Duration, Instant};

use ironrdp_cliprdr::pdu::{ClipboardFileAttributes, FileContentsFlags};
use ironrdp_cliprdr::{CliprdrClient, CliprdrServer};
use ironrdp_svc::{SvcMessage, SvcProcessor as _};
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;

use super::*;

/// What the server side was told.
#[derive(Debug, Default)]
struct ServerLog {
    client_formats: Vec<ClipboardFormat>,
    client_files: Option<(Vec<FileDescriptor>, Option<u32>)>,
    contents_requests: Vec<FileContentsRequest>,
    contents_responses: Vec<(u32, bool, Vec<u8>)>,
}

#[derive(Debug)]
struct ServerBackend(Arc<Mutex<ServerLog>>);

impl_as_any!(ServerBackend);

impl CliprdrBackend for ServerBackend {
    fn temporary_directory(&self) -> &str {
        "C:\\Temp"
    }

    /// What Windows offers.
    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {}

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        self.0.lock().client_formats = formats.to_vec();
    }

    fn on_format_data_request(&mut self, _: FormatDataRequest) {}

    fn on_format_data_response(&mut self, _: FormatDataResponse<'_>) {}

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        self.0.lock().contents_requests.push(request);
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        self.0.lock().contents_responses.push((
            response.stream_id(),
            response.is_error(),
            response.data().to_vec(),
        ));
    }

    fn on_lock(&mut self, _: LockDataId) {}

    fn on_unlock(&mut self, _: LockDataId) {}

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        self.0.lock().client_files = Some((files.to_vec(), clip_data_id));
    }
}

struct Harness {
    client: CliprdrClient,
    server: CliprdrServer,
    log: Arc<Mutex<ServerLog>>,
    session: UnboundedReceiver<SessionCmd>,
    worker_rx: mpsc::Receiver<WorkerMsg>,
    /// The worker, fed only messages that don't touch the Wayland clipboard.
    worker: Worker,
    remote: Arc<RemoteFiles>,
    /// Contents of the files the server copied, by list position.
    server_files: Vec<Vec<u8>>,
    /// Messages for the worker that would touch the Wayland clipboard.
    held: Vec<WorkerMsg>,
}

impl Harness {
    fn connect() -> Self {
        let (session_tx, session) = tokio::sync::mpsc::unbounded_channel();
        let (worker_tx, worker_rx) = mpsc::channel();
        let remote = Arc::new(RemoteFiles::new(session_tx.clone()));
        let backend = Backend {
            session: session_tx.clone(),
            worker: worker_tx,
            pending: None,
            conn: remote.connect(),
            remote: remote.clone(),
            files: false,
        };
        let log = Arc::default();
        let mut harness = Self {
            client: CliprdrClient::new(Box::new(backend)),
            server: CliprdrServer::new(Box::new(ServerBackend(Arc::clone(&log)))),
            log,
            session,
            worker_rx,
            worker: Worker::new(session_tx, remote.clone()),
            remote,
            server_files: Vec::new(),
            held: Vec::new(),
        };
        let start = harness.server.start().unwrap();
        harness.pump(start, Vec::new());
        harness
    }

    /// Carries PDUs both ways, along with what our side asks of the session and the server's
    /// file reads, until everything is answered.
    fn pump(&mut self, mut to_client: Vec<SvcMessage>, mut to_server: Vec<SvcMessage>) {
        loop {
            while let Ok(msg) = self.worker_rx.try_recv() {
                match msg {
                    msg @ (WorkerMsg::Files(_)
                    | WorkerMsg::FileContents(_)
                    | WorkerMsg::Lock(_)
                    | WorkerMsg::Unlock(_)) => self.worker.handle(msg),
                    other => self.held.push(other),
                }
            }
            while let Ok(SessionCmd::Clipboard(message)) = self.session.try_recv() {
                to_server.extend(apply(&mut self.client, message));
            }
            let requests = std::mem::take(&mut self.log.lock().contents_requests);
            for request in requests {
                let data = &self.server_files[request.index as usize];
                let response = if request.flags.contains(FileContentsFlags::SIZE) {
                    FileContentsResponse::new_size_response(request.stream_id, data.len() as u64)
                } else {
                    let start = request.position as usize;
                    // Like Windows, answer large reads in parts.
                    let end = data
                        .len()
                        .min(start + (request.requested_size as usize).min(50_000));
                    FileContentsResponse::new_data_response(
                        request.stream_id,
                        data[start..end].to_vec(),
                    )
                };
                to_client.extend(Vec::from(
                    self.server.submit_file_contents(response).unwrap(),
                ));
            }
            if to_client.is_empty() && to_server.is_empty() {
                return;
            }
            let replies_to_server = deliver(&mut self.client, std::mem::take(&mut to_client));
            let replies_to_client = deliver(&mut self.server, std::mem::take(&mut to_server));
            to_server = replies_to_server;
            to_client = replies_to_client;
        }
    }

    /// Reads a file in our filesystem; this thread carries the PDUs meanwhile.
    fn read(&mut self, path: PathBuf) -> Vec<u8> {
        let reader = std::thread::spawn(move || std::fs::read(path));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reader.is_finished() {
            assert!(Instant::now() < deadline, "read timed out");
            self.pump(Vec::new(), Vec::new());
            std::thread::sleep(Duration::from_millis(1));
        }
        reader.join().unwrap().unwrap()
    }

    fn held_listing(&mut self) -> Option<Listing> {
        let i = (self.held.iter()).position(|m| matches!(m, WorkerMsg::RemoteFiles(_)))?;
        match self.held.remove(i) {
            WorkerMsg::RemoteFiles(listing) => Some(listing),
            _ => unreachable!(),
        }
    }
}

/// What the session loop does with a request from our side (see `active::clipboard_message`).
fn apply(client: &mut CliprdrClient, message: ClipboardMessage) -> Vec<SvcMessage> {
    use ClipboardMessage as M;
    let messages = match message {
        M::SendInitiateCopy(formats) => client.initiate_copy(&formats),
        M::SendFormatData(response) => client.submit_format_data(response),
        M::SendInitiatePaste(format) => client.initiate_paste(format),
        M::SendFileContentsRequest(request) => client.request_file_contents(request),
        M::SendFileContentsResponse(response) => client.submit_file_contents(response),
        M::SendInitiateFileCopy(files) => client.initiate_file_copy(files),
        M::Error(e) => panic!("clipboard error: {e}"),
    };
    messages.expect("the channel accepts it").into()
}

fn deliver(to: &mut impl ironrdp_svc::SvcProcessor, messages: Vec<SvcMessage>) -> Vec<SvcMessage> {
    (messages.iter())
        .flat_map(|m| to.process(&m.encode_unframed_pdu().unwrap()).unwrap())
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("relay-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn handshake_completes_with_an_empty_local_clipboard() {
    let harness = Harness::connect();
    // The worker learned the server takes files, then that the channel is ready (where it
    // offers the local clipboard).
    assert!(harness.worker.files_enabled);
    assert!(matches!(harness.held[..], [WorkerMsg::Connected]));
}

#[test]
fn remote_files_are_read_through_the_mount() {
    let mut harness = Harness::connect();
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    harness.server_files = vec![Vec::new(), big.clone(), b"hi".to_vec()];
    let files = vec![
        FileDescriptor::new("Reports").with_attributes(ClipboardFileAttributes::DIRECTORY),
        FileDescriptor::new("q3.xlsx")
            .with_relative_path("Reports")
            .with_file_size(big.len() as u64),
        FileDescriptor::new("note.txt").with_file_size(2),
    ];
    let copy = harness.server.initiate_file_copy(files).unwrap();
    harness.pump(copy.into(), Vec::new());

    let listing = harness
        .held_listing()
        .expect("the file list reached the worker");
    let paths = match harness.remote.publish(&listing) {
        Ok(paths) => paths.unwrap(),
        Err(e) => {
            eprintln!("skipping: can't mount FUSE here ({e})");
            return;
        }
    };
    let names: Vec<_> = paths.iter().map(|p| p.file_name().unwrap()).collect();
    assert_eq!(names, ["Reports", "note.txt"]);

    assert_eq!(harness.read(paths[0].join("q3.xlsx")), big);
    assert_eq!(harness.read(paths[1].clone()), b"hi");
}

#[test]
fn local_files_are_served_to_the_server() {
    let mut harness = Harness::connect();
    let dir = scratch("protocol-local");
    std::fs::create_dir_all(dir.join("pics")).unwrap();
    std::fs::write(dir.join("pics/a.png"), b"\x89PNG....").unwrap();
    let notes: Vec<u8> = (0..100_000u32).map(|i| (i % 7) as u8).collect();
    std::fs::write(dir.join("notes.txt"), &notes).unwrap();

    // What the worker does when it finds files on the local clipboard.
    let files = LocalFiles::collect(&[dir.join("pics"), dir.join("notes.txt")]);
    let offer = ClipboardMessage::SendInitiateFileCopy(files.descriptors.clone());
    harness.worker.files = Some(Arc::new(files));
    let to_server = apply(&mut harness.client, offer);
    harness.pump(Vec::new(), to_server);

    // The server pastes: it asks for the list, then the contents.
    let list_format = (harness.log.lock().client_formats.iter())
        .find(|f| {
            f.name()
                .is_some_and(|n| n.value() == "FileGroupDescriptorW")
        })
        .map(ClipboardFormat::id)
        .expect("files offered");
    let paste = harness.server.initiate_paste(list_format).unwrap();
    harness.pump(paste.into(), Vec::new());
    let (listed, lock) = harness
        .log
        .lock()
        .client_files
        .clone()
        .expect("list received");
    let listed_names: Vec<_> = (listed.iter())
        .map(|f| (f.relative_path.as_deref(), f.name.as_str()))
        .collect();
    assert_eq!(
        listed_names,
        [(None, "pics"), (Some("pics"), "a.png"), (None, "notes.txt")]
    );
    assert!(lock.is_some(), "the server locked our list");

    // The local clipboard changes; the server still reads the snapshot it locked.
    harness.worker.files = None;
    let mut position = 0;
    let mut received = Vec::new();
    while position < notes.len() as u64 {
        let request = FileContentsRequest {
            stream_id: 40 + position as u32,
            index: 2,
            flags: FileContentsFlags::RANGE,
            position,
            // Ranges must stay within the file.
            requested_size: 65_536.min(notes.len() as u32 - position as u32),
            data_id: lock,
        };
        let messages = harness.server.request_file_contents(request).unwrap();
        harness.pump(messages.into(), Vec::new());
        let (_, error, data) = harness.log.lock().contents_responses.pop().unwrap();
        assert!(!error);
        position += data.len() as u64;
        received.extend(data);
    }
    assert_eq!(received, notes);
    let _ = std::fs::remove_dir_all(dir);
}
