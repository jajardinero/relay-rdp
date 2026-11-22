//! Native file and folder choosers (through the desktop portal), run off the UI thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use crate::app::UiWaker;

pub enum Picked {
    RdpFile(PathBuf),
    SharedFolder(PathBuf),
}

pub struct Picker {
    tx: mpsc::Sender<Picked>,
    rx: mpsc::Receiver<Picked>,
    waker: UiWaker,
    /// One chooser at a time.
    open: Arc<AtomicBool>,
}

impl Picker {
    pub fn new(waker: UiWaker) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            waker,
            open: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn rdp_file(&self) {
        self.spawn(|| {
            rfd::FileDialog::new()
                .set_title("Import a Remote Desktop file")
                .add_filter("Remote Desktop files", &["rdp"])
                .pick_file()
                .map(Picked::RdpFile)
        });
    }

    pub fn shared_folder(&self) {
        self.spawn(|| {
            rfd::FileDialog::new()
                .set_title("Choose a folder to share")
                .pick_folder()
                .map(Picked::SharedFolder)
        });
    }

    pub fn try_recv(&self) -> Option<Picked> {
        self.rx.try_recv().ok()
    }

    fn spawn(&self, pick: impl FnOnce() -> Option<Picked> + Send + 'static) {
        if self.open.swap(true, Ordering::AcqRel) {
            return;
        }
        let (tx, waker, open) = (self.tx.clone(), self.waker.clone(), Arc::clone(&self.open));
        let spawned = std::thread::Builder::new()
            .name("file-chooser".into())
            .spawn(move || {
                if let Some(picked) = pick() {
                    let _ = tx.send(picked);
                    waker.wake();
                }
                open.store(false, Ordering::Release);
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "couldn't open the file chooser");
            self.open.store(false, Ordering::Release);
        }
    }
}
