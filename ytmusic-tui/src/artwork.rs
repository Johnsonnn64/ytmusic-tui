//! Optional terminal artwork. Rendering supplies only the visible image slots;
//! an independent worker downloads/cache images and talks to ueberzugpp. A
//! latest-value channel prevents rapid scrolling from building a backlog.

use std::sync::mpsc;
use std::thread::JoinHandle;

use ratatui::layout::Rect;
use tokio::sync::watch;

pub(crate) mod cache;

#[cfg(unix)]
mod x11;

/// One visible thumbnail. Identifiers describe slots, so duplicate covers can
/// be shown in multiple rows while sharing a single download/cache entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub identifier: String,
    pub url: String,
    pub area: Rect,
}

/// A resize revision forces re-placement even if character coordinates happen
/// to stay the same (font zoom can change pixels without changing row count).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scene {
    pub revision: u64,
    pub images: Vec<Placement>,
}

enum Status {
    Ready,
    Failed(String),
}

/// Owned by the terminal loop. No network, disk, or image IPC happens in
/// update(); dropping it joins the worker after its image process is removed.
pub struct ArtworkManager {
    scene: Option<watch::Sender<Scene>>,
    status: mpsc::Receiver<Status>,
    worker: Option<JoinHandle<()>>,
    available: bool,
}

impl ArtworkManager {
    pub fn new(enabled: bool) -> Self {
        let (status_tx, status) = mpsc::channel();
        let mut manager = Self {
            scene: None,
            status,
            worker: None,
            available: false,
        };
        #[cfg(unix)]
        if enabled && std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty()) {
            let (scene_tx, scene_rx) = watch::channel(Scene::default());
            match std::thread::Builder::new()
                .name("artwork".into())
                .spawn(move || {
                    if let Err(error) = x11::run(scene_rx, &status_tx) {
                        let _ = status_tx.send(Status::Failed(error));
                    }
                }) {
                Ok(worker) => {
                    manager.scene = Some(scene_tx);
                    manager.worker = Some(worker);
                }
                Err(_) => manager.available = false,
            }
        }
        #[cfg(not(unix))]
        let _ = (enabled, status_tx);
        manager
    }

    /// Poll worker health once per UI tick. Failures are reported once; the
    /// caller restores compact rows and may show a non-fatal toast.
    pub fn poll(&mut self) -> Option<String> {
        let mut error = None;
        while let Ok(status) = self.status.try_recv() {
            match status {
                Status::Ready => self.available = true,
                Status::Failed(message) => {
                    self.available = false;
                    error = Some(message);
                }
            }
        }
        error
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn update(&self, scene: Scene) {
        if let Some(sender) = &self.scene {
            sender.send_if_modified(|current| {
                if *current == scene {
                    false
                } else {
                    *current = scene;
                    true
                }
            });
        }
    }
}

impl Drop for ArtworkManager {
    fn drop(&mut self) {
        // Closing the latest-value channel cancels HTTP and destroys the X11
        // layer before the terminal leaves its alternate screen.
        self.scene.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
