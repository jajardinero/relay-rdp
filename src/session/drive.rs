//! Shared folders (RDPDR drive redirection).
//!
//! IronRDP's Unix backend serves one root directory and joins server-supplied paths onto it
//! verbatim. This wrapper adds two things: several folders (one backend per drive, routed by
//! device id) and path sanitising, so a server can never reach outside a shared folder with
//! `..` components.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::RdpdrBackend;
use ironrdp_rdpdr::pdu::efs::{
    DeviceControlRequest, FileInformationClass, ServerDeviceAnnounceResponse, ServerDriveIoRequest,
};
use ironrdp_rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_rdpdr_native::backend::NixRdpdrBackend;
use ironrdp_svc::SvcMessage;

use crate::store::profiles::SharedFolder;

/// First device id; ids are arbitrary but must be unique per connection.
const FIRST_DEVICE_ID: u32 = 1;

pub struct SharedDrives {
    drives: BTreeMap<u32, NixRdpdrBackend>,
}

impl std::fmt::Debug for SharedDrives {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedDrives")
            .field("drives", &self.drives.keys())
            .finish()
    }
}

impl_as_any!(SharedDrives);

impl SharedDrives {
    /// Builds the backend and the `(device id, drive name)` list to announce. Folders that don't
    /// exist are skipped (and logged) rather than failing the connection.
    pub fn new(folders: &[SharedFolder]) -> (Self, Vec<(u32, String)>) {
        let mut drives = BTreeMap::new();
        let mut announce = Vec::new();
        let mut next_id = FIRST_DEVICE_ID;
        for folder in folders {
            let Ok(root) = std::fs::canonicalize(&folder.path) else {
                tracing::warn!(path = %folder.path.display(), "shared folder doesn't exist; skipping");
                continue;
            };
            if !root.is_dir() {
                tracing::warn!(path = %root.display(), "shared folder isn't a directory; skipping");
                continue;
            }
            let name = drive_name(&folder.name, &root);
            drives.insert(next_id, NixRdpdrBackend::new(root_string(root)));
            announce.push((next_id, name));
            next_id += 1;
        }
        (Self { drives }, announce)
    }
}

/// The backend concatenates root and request path, so the root must not end in a separator.
fn root_string(root: PathBuf) -> String {
    let mut s = root.to_string_lossy().into_owned();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if s == "/" { String::new() } else { s }
}

/// Windows shows the drive as "<name> on <computer>"; keep the name short and printable.
fn drive_name(name: &str, root: &std::path::Path) -> String {
    let name = name.trim();
    let base = if name.is_empty() {
        root.file_name()
            .map_or_else(|| "Shared".to_owned(), |n| n.to_string_lossy().into_owned())
    } else {
        name.to_owned()
    };
    base.chars().filter(|c| !c.is_control()).take(32).collect()
}

/// Rebuilds a Windows path from its normal components only: `.`, `..` and empty components are
/// dropped, so the result always stays under the drive root.
fn sanitize(path: &str) -> String {
    let parts: Vec<&str> = path
        .split(['\\', '/'])
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    if parts.is_empty() {
        return if path.is_empty() {
            String::new()
        } else {
            "\\".to_owned()
        };
    }
    let mut out = String::new();
    for part in parts {
        out.push('\\');
        out.push_str(part);
    }
    out
}

fn device_id(req: &ServerDriveIoRequest) -> u32 {
    use ServerDriveIoRequest as R;
    match req {
        R::ServerCreateDriveRequest(r) => r.device_io_request.device_id,
        R::ServerDriveQueryInformationRequest(r) => r.device_io_request.device_id,
        R::DeviceCloseRequest(r) => r.device_io_request.device_id,
        R::ServerDriveQueryDirectoryRequest(r) => r.device_io_request.device_id,
        R::ServerDriveNotifyChangeDirectoryRequest(r) => r.device_io_request.device_id,
        R::ServerDriveQueryVolumeInformationRequest(r) => r.device_io_request.device_id,
        R::DeviceControlRequest(r) => r.header.device_id,
        R::DeviceReadRequest(r) => r.device_io_request.device_id,
        R::DeviceWriteRequest(r) => r.device_io_request.device_id,
        R::ServerDriveSetInformationRequest(r) => r.device_io_request.device_id,
        R::ServerDriveLockControlRequest(r) => r.device_io_request.device_id,
    }
}

fn sanitize_request(req: &mut ServerDriveIoRequest) {
    match req {
        ServerDriveIoRequest::ServerCreateDriveRequest(r) => r.path = sanitize(&r.path),
        ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(r) => r.path = sanitize(&r.path),
        ServerDriveIoRequest::ServerDriveSetInformationRequest(r) => {
            if let FileInformationClass::Rename(rename) = &mut r.set_buffer {
                rename.file_name = sanitize(&rename.file_name);
            }
        }
        _ => {}
    }
}

impl RdpdrBackend for SharedDrives {
    fn handle_server_device_announce_response(
        &mut self,
        pdu: ServerDeviceAnnounceResponse,
    ) -> PduResult<()> {
        tracing::debug!(device_id = pdu.device_id, status = ?pdu.result_code, "shared folder announced");
        Ok(())
    }

    fn handle_scard_call(
        &mut self,
        _req: DeviceControlRequest<ScardIoCtlCode>,
        _call: ScardCall,
    ) -> PduResult<()> {
        Ok(())
    }

    fn handle_drive_io_request(
        &mut self,
        mut req: ServerDriveIoRequest,
    ) -> PduResult<Vec<SvcMessage>> {
        let id = device_id(&req);
        let Some(drive) = self.drives.get_mut(&id) else {
            tracing::warn!(device_id = id, "request for an unknown shared folder");
            return Ok(Vec::new());
        };
        sanitize_request(&mut req);
        drive.handle_drive_io_request(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_components_cannot_escape() {
        assert_eq!(sanitize("\\..\\..\\etc\\passwd"), "\\etc\\passwd");
        assert_eq!(sanitize("\\docs\\..\\..\\x"), "\\docs\\x");
        assert_eq!(sanitize("../../x"), "\\x");
        assert_eq!(sanitize("\\a\\.\\b\\"), "\\a\\b");
    }

    #[test]
    fn normal_paths_are_kept() {
        assert_eq!(sanitize("\\report.docx"), "\\report.docx");
        assert_eq!(sanitize("\\folder\\*"), "\\folder\\*");
        assert_eq!(sanitize("\\"), "\\");
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn roots_have_no_trailing_separator() {
        assert_eq!(
            root_string(PathBuf::from("/home/me/Shared/")),
            "/home/me/Shared"
        );
        assert_eq!(root_string(PathBuf::from("/")), "");
    }

    #[test]
    fn missing_folders_are_skipped() {
        let tmp = std::env::temp_dir();
        let folders = vec![
            SharedFolder {
                name: "Temp".into(),
                path: tmp,
            },
            SharedFolder {
                name: "Gone".into(),
                path: "/definitely/not/here".into(),
            },
        ];
        let (drives, announce) = SharedDrives::new(&folders);
        assert_eq!(announce, vec![(1, "Temp".to_owned())]);
        assert_eq!(drives.drives.len(), 1);
    }
}
