//! Clipboard sharing (CLIPRDR): text and images, both directions.
//!
//! The CLIPRDR backend is called from inside the protocol stack and must not block, so all
//! local clipboard access happens on a worker thread (Wayland data-control through `arboard`).
//! Replies to the server go back through the session's own command channel.
//!
//! - Remote → local: when the server announces new clipboard contents we fetch them right away
//!   (text, PNG or DIB) and place them on the local clipboard.
//! - Local → remote: when the window gains focus (and every couple of seconds while it has it)
//!   we look at the local clipboard and announce it to the server if it changed. The data is
//!   sent when the remote side actually pastes.

use std::hash::{Hash as _, Hasher as _};
use std::sync::mpsc;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse,
};
use ironrdp_core::impl_as_any;
use tokio::sync::mpsc::UnboundedSender;

use super::SessionCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Png,
    Dib,
}

enum WorkerMsg {
    /// Look at the local clipboard; announce it if it changed. Images are only checked when
    /// asked, as reading one means decoding it.
    CheckLocal { images: bool },
    /// The server asked for the local clipboard in this format.
    Provide(ClipboardFormatId),
    /// The server sent its clipboard contents.
    SetLocal(Kind, Vec<u8>),
}

/// One per session; outlives reconnects.
pub struct ClipboardWorker {
    tx: mpsc::Sender<WorkerMsg>,
}

impl ClipboardWorker {
    pub fn spawn(session: UnboundedSender<SessionCmd>) -> Self {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || Worker::new(session).run(rx));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "clipboard sharing disabled");
        }
        Self { tx }
    }

    pub fn backend(&self, session: UnboundedSender<SessionCmd>) -> Box<dyn CliprdrBackend> {
        Box::new(Backend {
            session,
            worker: self.tx.clone(),
            pending: None,
        })
    }

    pub fn check_local(&self, images: bool) {
        let _ = self.tx.send(WorkerMsg::CheckLocal { images });
    }
}

#[derive(Debug)]
struct Backend {
    session: UnboundedSender<SessionCmd>,
    worker: mpsc::Sender<WorkerMsg>,
    /// What kind of data the paste we requested will return.
    pending: Option<Kind>,
}

impl_as_any!(Backend);

impl Backend {
    fn to_session(&self, message: ClipboardMessage) {
        let _ = self.session.send(SessionCmd::Clipboard(message));
    }
}

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        let _ = self.worker.send(WorkerMsg::CheckLocal { images: true });
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        let has = |id: ClipboardFormatId| formats.iter().any(|f| f.id() == id);
        let png = formats
            .iter()
            .find(|f| {
                f.name()
                    .is_some_and(|n| n.value().eq_ignore_ascii_case("PNG"))
            })
            .map(ClipboardFormat::id);

        let request = if has(ClipboardFormatId::CF_UNICODETEXT) {
            Some((Kind::Text, ClipboardFormatId::CF_UNICODETEXT))
        } else if let Some(png) = png {
            Some((Kind::Png, png))
        } else if has(ClipboardFormatId::CF_DIB) {
            Some((Kind::Dib, ClipboardFormatId::CF_DIB))
        } else {
            None
        };
        if let Some((kind, id)) = request {
            self.pending = Some(kind);
            self.to_session(ClipboardMessage::SendInitiatePaste(id));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let _ = self.worker.send(WorkerMsg::Provide(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let Some(kind) = self.pending.take() else {
            return;
        };
        if response.is_error() {
            tracing::debug!("the server couldn't provide its clipboard");
            return;
        }
        let _ = self
            .worker
            .send(WorkerMsg::SetLocal(kind, response.data().to_vec()));
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

enum Local {
    Text(String),
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
}

struct Worker {
    session: UnboundedSender<SessionCmd>,
    clipboard: Option<arboard::Clipboard>,
    /// What we last announced (or received), to avoid echoing the same content back and forth.
    last_hash: Option<u64>,
    current: Option<Local>,
}

impl Worker {
    fn new(session: UnboundedSender<SessionCmd>) -> Self {
        Self {
            session,
            clipboard: None,
            last_hash: None,
            current: None,
        }
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => tracing::warn!(error = %e, "can't open the local clipboard"),
            }
        }
        self.clipboard.as_mut()
    }

    fn run(mut self, rx: mpsc::Receiver<WorkerMsg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                WorkerMsg::CheckLocal { images } => self.check_local(images),
                WorkerMsg::Provide(format) => self.provide(format),
                WorkerMsg::SetLocal(kind, data) => self.set_local(kind, &data),
            }
        }
    }

    fn check_local(&mut self, images: bool) {
        let Some(clipboard) = self.clipboard() else {
            return;
        };
        let local = match clipboard.get_text() {
            Ok(text) if !text.is_empty() => Some(Local::Text(text)),
            _ if images => clipboard.get_image().ok().map(|img| Local::Image {
                width: img.width,
                height: img.height,
                rgba: img.bytes.into_owned(),
            }),
            _ => None,
        };
        let Some(local) = local else { return };

        let hash = hash_local(&local);
        if self.last_hash == Some(hash) {
            return;
        }
        self.last_hash = Some(hash);

        let format = match &local {
            Local::Text(_) => ClipboardFormatId::CF_UNICODETEXT,
            Local::Image { .. } => ClipboardFormatId::CF_DIB,
        };
        self.current = Some(local);
        let _ = self
            .session
            .send(SessionCmd::Clipboard(ClipboardMessage::SendInitiateCopy(
                vec![ClipboardFormat::new(format)],
            )));
    }

    fn provide(&mut self, format: ClipboardFormatId) {
        let data = match (&self.current, format) {
            (Some(Local::Text(text)), ClipboardFormatId::CF_UNICODETEXT) => Some(encode_text(text)),
            (
                Some(Local::Image {
                    width,
                    height,
                    rgba,
                }),
                ClipboardFormatId::CF_DIB,
            ) => encode_dib(*width, *height, rgba),
            _ => None,
        };
        let response = match data {
            Some(data) => OwnedFormatDataResponse::new_data(data),
            None => OwnedFormatDataResponse::new_error(),
        };
        let _ = self
            .session
            .send(SessionCmd::Clipboard(ClipboardMessage::SendFormatData(
                response,
            )));
    }

    fn set_local(&mut self, kind: Kind, data: &[u8]) {
        let local = match kind {
            Kind::Text => decode_text(data).map(Local::Text),
            Kind::Png => decode_png(data),
            Kind::Dib => decode_dib(data),
        };
        let Some(local) = local else {
            tracing::debug!(?kind, "couldn't decode the remote clipboard");
            return;
        };
        // Remember it so the next local check doesn't send it straight back.
        self.last_hash = Some(hash_local(&local));
        let Some(clipboard) = self.clipboard() else {
            return;
        };
        let result = match &local {
            Local::Text(text) => clipboard.set_text(text.clone()),
            Local::Image {
                width,
                height,
                rgba,
            } => clipboard.set_image(arboard::ImageData {
                width: *width,
                height: *height,
                bytes: rgba.as_slice().into(),
            }),
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "couldn't set the local clipboard");
            self.clipboard = None;
        }
        self.current = Some(local);
    }
}

fn hash_local(local: &Local) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match local {
        Local::Text(text) => text.hash(&mut hasher),
        Local::Image {
            width,
            height,
            rgba,
        } => {
            (width, height).hash(&mut hasher);
            rgba.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// UTF-16LE, CRLF line endings, NUL-terminated, as Windows expects for CF_UNICODETEXT.
fn encode_text(text: &str) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut out: Vec<u8> = normalized
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    out.extend_from_slice(&[0, 0]);
    out
}

fn decode_text(data: &[u8]) -> Option<String> {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    let text = String::from_utf16_lossy(&units).replace("\r\n", "\n");
    (!text.is_empty()).then_some(text)
}

fn decode_png(data: &[u8]) -> Option<Local> {
    let image = image::load_from_memory_with_format(data, image::ImageFormat::Png).ok()?;
    let rgba = image.to_rgba8();
    Some(Local::Image {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

const BITMAPINFOHEADER_SIZE: usize = 40;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

/// A CF_DIB: BITMAPINFOHEADER then bottom-up BGRA rows (32 bpp, uncompressed).
fn encode_dib(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let w = i32::try_from(width).ok()?;
    let h = i32::try_from(height).ok()?;
    let image_size = u32::try_from(width * height * 4).ok()?;
    let mut out = Vec::with_capacity(BITMAPINFOHEADER_SIZE + image_size as usize);
    out.extend_from_slice(&(BITMAPINFOHEADER_SIZE as u32).to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes()); // positive height: bottom-up
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&BI_RGB.to_le_bytes());
    out.extend_from_slice(&image_size.to_le_bytes());
    out.extend_from_slice(&[0; 16]); // resolution, palette
    for row in (0..height).rev() {
        for px in rgba[row * width * 4..(row + 1) * width * 4].chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Some(out)
}

fn decode_dib(data: &[u8]) -> Option<Local> {
    let u32_at = |o: usize| {
        data.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
    };
    let i32_at = |o: usize| {
        data.get(o..o + 4)
            .map(|b| i32::from_le_bytes(b.try_into().expect("4 bytes")))
    };
    let header_size = u32_at(0)? as usize;
    let width = usize::try_from(i32_at(4)?).ok()?;
    let raw_height = i32_at(8)?;
    let bit_count = u16::from_le_bytes(data.get(14..16)?.try_into().ok()?);
    let compression = u32_at(16)?;
    let colors_used = u32_at(32)? as usize;

    if width == 0 || raw_height == 0 || width > 16384 || raw_height.unsigned_abs() > 16384 {
        return None;
    }
    let height = raw_height.unsigned_abs() as usize;
    let bottom_up = raw_height > 0;
    let bpp = match (bit_count, compression) {
        (32, BI_RGB | BI_BITFIELDS) => 4,
        (24, BI_RGB) => 3,
        _ => return None,
    };
    // Pixel data follows the header, three colour masks (BI_BITFIELDS with a small header) and
    // any palette entries.
    let masks = if compression == BI_BITFIELDS && header_size == BITMAPINFOHEADER_SIZE {
        12
    } else {
        0
    };
    let offset = header_size + masks + colors_used * 4;
    let stride = (width * bpp).div_ceil(4) * 4;
    let pixels = data.get(offset..offset + stride * height)?;

    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let row = if bottom_up { height - 1 - y } else { y };
        let line = &pixels[row * stride..row * stride + width * bpp];
        for px in line.chunks_exact(bpp) {
            // 32-bit DIBs from Windows usually carry no alpha; treat them as opaque.
            rgba.extend_from_slice(&[px[2], px[1], px[0], 0xFF]);
        }
    }
    Some(Local::Image {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_with_crlf() {
        let wire = encode_text("line one\nline two");
        assert_eq!(&wire[wire.len() - 2..], &[0, 0]);
        assert!(
            String::from_utf16_lossy(
                &wire
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect::<Vec<_>>()
            )
            .contains("\r\n")
        );
        assert_eq!(decode_text(&wire).unwrap(), "line one\nline two");
    }

    #[test]
    fn non_ascii_text() {
        let wire = encode_text("Ñandú — 東京 🚀");
        assert_eq!(decode_text(&wire).unwrap(), "Ñandú — 東京 🚀");
    }

    #[test]
    fn dib_round_trips() {
        let rgba: Vec<u8> = (0..3 * 2).flat_map(|i| [i as u8 * 10, 1, 2, 255]).collect();
        let dib = encode_dib(3, 2, &rgba).unwrap();
        match decode_dib(&dib).unwrap() {
            Local::Image {
                width,
                height,
                rgba: back,
            } => {
                assert_eq!((width, height), (3, 2));
                assert_eq!(back, rgba);
            }
            Local::Text(_) => panic!("expected an image"),
        }
    }

    #[test]
    fn malformed_dibs_are_rejected() {
        assert!(decode_dib(&[]).is_none());
        let mut dib = encode_dib(2, 2, &[0; 16]).unwrap();
        dib.truncate(50);
        assert!(decode_dib(&dib).is_none());
    }
}
