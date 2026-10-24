//! X11 backend. This module runs entirely outside the terminal thread.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use sha1::{Digest, Sha1};
use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinSet};

use super::{Placement, Scene, Status};

mod windows;
use windows::ImageWindows;

const MAX_DOWNLOADS: usize = 2;
const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHE_FILES: usize = 256;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const RETRY_AFTER: Duration = Duration::from_secs(60);

pub(super) fn run(
    scene: watch::Receiver<Scene>,
    status: &mpsc::Sender<Status>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Could not start artwork worker".to_owned())?;
    let result = runtime.block_on(work(scene, status));
    runtime.shutdown_timeout(Duration::from_millis(100));
    result
}

async fn work(
    mut scenes: watch::Receiver<Scene>,
    status: &mpsc::Sender<Status>,
) -> Result<(), String> {
    let mut layer = Layer::start(&scenes).await?;
    let cache = cache_directory().ok_or("No artwork cache directory available")?;
    fs::create_dir_all(&cache).map_err(|_| "Cannot create artwork cache")?;
    trim_cache(&cache, &HashSet::new());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(4))
        .build()
        .map_err(|_| "Could not start thumbnail downloads")?;
    let _ = status.send(Status::Ready);

    let mut scene = scenes.borrow_and_update().clone();
    let mut displayed = HashMap::new();
    let mut revision = scene.revision;
    let mut jobs = JoinSet::new();
    let mut inflight: HashMap<String, AbortHandle> = HashMap::new();
    let mut failures: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if scene.revision != revision {
            // ueberzugpp captures font/cell metrics at startup. Replacing its
            // layer after resize also handles font zoom; merely resending cell
            // coordinates would keep using stale pixel sizes. Coalesce resize
            // bursts before starting the replacement process.
            layer
                .send(&changes(&displayed, &HashMap::new(), false))
                .map_err(|_| "Artwork renderer disconnected")?;
            displayed.clear();
            match tokio::time::timeout(Duration::from_millis(120), scenes.changed()).await {
                Ok(Ok(())) => {
                    scene = scenes.borrow_and_update().clone();
                    continue;
                }
                Ok(Err(_)) => break,
                Err(_) => {}
            }
            layer.stop();
            layer = Layer::start(&scenes).await?;
            revision = scene.revision;
        }
        let wanted: HashSet<_> = scene.images.iter().map(|image| image.url.clone()).collect();
        inflight.retain(|url, task| {
            if wanted.contains(url) {
                true
            } else {
                task.abort();
                false
            }
        });
        // Failed downloads retry after a cooldown, with memory bounded to the
        // current viewport. Switching away cancels work rather than queuing it.
        failures.retain(|url, at| wanted.contains(url) && at.elapsed() < RETRY_AFTER);
        let mut resolved = HashMap::new();
        for image in &scene.images {
            let path = cache_path(&cache, &image.url);
            if path.is_file() {
                resolved.insert(
                    image.identifier.clone(),
                    Displayed {
                        image: image.clone(),
                        path,
                    },
                );
            } else if inflight.len() < MAX_DOWNLOADS
                && !inflight.contains_key(&image.url)
                && !failures.contains_key(&image.url)
            {
                let url = image.url.clone();
                let client = client.clone();
                let task_url = url.clone();
                let task = jobs.spawn(async move {
                    let result = download(&client, &task_url, &path).await;
                    (task_url, result)
                });
                inflight.insert(url, task);
            }
        }

        let commands = changes(&displayed, &resolved, revision != scene.revision);
        if !commands.is_empty() {
            layer
                .send(&commands)
                .map_err(|_| "Artwork renderer disconnected")?;
            displayed = resolved;
            revision = scene.revision;
            if let Some(windows) = &mut layer.windows {
                windows.refresh(&displayed);
            }
        }

        tokio::select! {
            biased;
            changed = scenes.changed() => {
                if changed.is_err() {
                    break;
                }
                scene = scenes.borrow_and_update().clone();
            }
            result = jobs.join_next_with_id(), if !jobs.is_empty() => {
                match result {
                    Some(Ok((id, (url, result)))) => {
                        // A rapid scroll away/back can replace a task for the
                        // same URL before its old completion is collected.
                        if inflight.get(&url).is_some_and(|task| task.id() == id) {
                            inflight.remove(&url);
                            if result.is_err() {
                                failures.insert(url, Instant::now());
                            }
                        }
                        if result.is_ok() {
                            let protected = scene.images.iter()
                                .map(|image| cache_path(&cache, &image.url))
                                .collect();
                            trim_cache(&cache, &protected);
                        }
                    }
                    Some(Err(error)) => inflight.retain(|_, task| task.id() != error.id()),
                    None => {}
                }
            }
            _ = tick.tick() => {
                if layer.child.try_wait().map_err(|_| "Cannot check artwork renderer")?.is_some() {
                    return Err("Artwork renderer exited".into());
                }
                // The renderer creates its windows asynchronously after IPC.
                // Prepare each new window once; ready XIDs need no updates.
                if let Some(windows) = &mut layer.windows {
                    windows.refresh(&displayed);
                }
            }
        }
    }
    jobs.abort_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn image(identifier: &str, y: u16) -> Displayed {
        Displayed {
            image: Placement {
                identifier: identifier.into(),
                url: "https://example.test/album.jpg".into(),
                area: Rect::new(6, y, 6, 3),
            },
            path: PathBuf::from("/tmp/album cover.jpg"),
        }
    }

    #[test]
    fn progress_redraws_send_no_ipc_and_scrolling_only_updates_changed_slots() {
        let first = HashMap::from([
            ("queue-0".into(), image("queue-0", 2)),
            ("queue-1".into(), image("queue-1", 5)),
        ]);
        assert!(changes(&first, &first, false).is_empty());
        let scrolled = HashMap::from([
            ("queue-1".into(), image("queue-1", 2)),
            ("queue-2".into(), image("queue-2", 5)),
        ]);
        let commands = changes(&first, &scrolled, false);
        assert_eq!(commands.len(), 3);
        assert!(
            commands
                .iter()
                .any(|command| command["action"] == "remove" && command["identifier"] == "queue-0")
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["action"] == "add")
                .count(),
            2
        );
        for command in commands.iter().filter(|command| command["action"] == "add") {
            assert_eq!(command["max_width"], 6);
            assert_eq!(command["max_height"], 3);
        }
    }

    #[test]
    fn leaving_view_removes_every_image_and_resize_replaces_existing_images() {
        let images = HashMap::from([
            ("queue-0".into(), image("queue-0", 2)),
            ("queue-1".into(), image("queue-1", 5)),
        ]);
        assert!(
            changes(&images, &HashMap::new(), false)
                .iter()
                .all(|command| command["action"] == "remove")
        );
        let resized = changes(&images, &images, true);
        assert_eq!(resized.len(), 2);
        assert!(resized.iter().all(|command| command["action"] == "add"));
    }

    #[test]
    fn repeated_album_covers_have_separate_slots_but_share_cache_files() {
        let a = image("queue-0", 2);
        let b = image("queue-1", 5);
        assert_eq!(
            cache_path(Path::new("/cache"), &a.image.url),
            cache_path(Path::new("/cache"), &b.image.url)
        );
        let slots = HashMap::from([("queue-0".into(), a), ("queue-1".into(), b)]);
        assert_eq!(changes(&HashMap::new(), &slots, false).len(), 2);
    }

    #[test]
    fn image_cache_enforces_file_and_byte_limits_without_evicting_visible_images() {
        let directory = tempfile::tempdir().unwrap();
        let keep = directory.path().join("keep.img");
        fs::write(&keep, b"visible").unwrap();
        let protected = HashSet::from([keep.clone()]);
        for index in 0..MAX_CACHE_FILES + 3 {
            fs::write(directory.path().join(format!("{index}.img")), b"image").unwrap();
        }
        trim_cache(directory.path(), &protected);
        assert!(keep.exists());
        assert_eq!(
            fs::read_dir(directory.path()).unwrap().count(),
            MAX_CACHE_FILES
        );
        let big = directory.path().join("oversize.img");
        fs::File::create(&big)
            .unwrap()
            .set_len(MAX_CACHE_BYTES + 1)
            .unwrap();
        trim_cache(directory.path(), &protected);
        assert!(!big.exists());
        assert!(keep.exists());
    }

    #[test]
    fn thumbnail_validation_rejects_error_pages_and_gifs() {
        assert!(is_thumbnail(b"\xff\xd8\xffjpeg"));
        assert!(is_thumbnail(b"\x89PNG\r\n\x1a\npng"));
        assert!(is_thumbnail(b"RIFF\x04\0\0\0WEBP"));
        assert!(!is_thumbnail(b"<html>not found</html>"));
        assert!(!is_thumbnail(b"GIF89a"));
        assert!(!is_thumbnail(b""));
    }

    #[tokio::test]
    async fn invalid_thumbnail_urls_never_create_cache_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.img");
        assert!(
            download(&reqwest::Client::new(), "file:///etc/passwd", &path)
                .await
                .is_err()
        );
        assert!(!path.exists());
    }
}

/// Child stdin stays open to keep ueberzugpp in the foreground; commands go
/// through its Unix socket. The private directory also isolates its config so
/// a user's daemon/escape-code settings cannot change this owned lifecycle.
struct Layer {
    child: Child,
    socket: PathBuf,
    _directory: tempfile::TempDir,
    windows: Option<ImageWindows>,
}

impl Layer {
    async fn start(scenes: &watch::Receiver<Scene>) -> Result<Self, String> {
        let directory = tempfile::Builder::new()
            .prefix("ytm-art-")
            .tempdir()
            .map_err(|_| "Cannot create artwork socket directory")?;
        let mut child = None;
        for executable in ["ueberzugpp", "ueberzug"] {
            match Command::new(executable)
                .args([
                    "layer",
                    "--silent",
                    "--output",
                    "x11",
                    "--no-cache",
                    "--origin-center",
                ])
                .env("UEBERZUGPP_TMPDIR", directory.path())
                .env("XDG_CONFIG_HOME", directory.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(process) => {
                    child = Some(process);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err("Could not launch ueberzugpp".into()),
            }
        }
        let child = child.ok_or("Install ueberzugpp to show artwork")?;
        let socket = directory
            .path()
            .join(format!("ueberzugpp-{}.socket", child.id()));
        let windows = ImageWindows::connect(child.id());
        let mut layer = Self {
            child,
            socket,
            _directory: directory,
            windows,
        };
        for _ in 0..100 {
            if scenes.has_changed().is_err() {
                return Err("Artwork startup cancelled".into());
            }
            if layer
                .child
                .try_wait()
                .map_err(|_| "Cannot start artwork renderer")?
                .is_some()
            {
                return Err("ueberzugpp could not attach to this X11 terminal".into());
            }
            if layer.socket.exists() {
                return Ok(layer);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err("Timed out starting ueberzugpp".into())
    }

    fn send(&self, commands: &[serde_json::Value]) -> io::Result<()> {
        if commands.is_empty() {
            return Ok(());
        }
        let mut socket = UnixStream::connect(&self.socket)?;
        socket.set_write_timeout(Some(Duration::from_millis(100)))?;
        for command in commands {
            serde_json::to_writer(&mut socket, command)?;
            socket.write_all(b"\n")?;
        }
        // ueberzugpp processes a connection's commands after reaching EOF.
        socket.shutdown(Shutdown::Write)
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        self.stop();
        // Destroying the X11 connection removes all image child windows.
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Displayed {
    image: Placement,
    path: PathBuf,
}

/// Compute IPC only for changed slots, including removals when leaving Queue
/// or opening an overlay. Repeated player progress redraws send no commands.
fn changes(
    previous: &HashMap<String, Displayed>,
    next: &HashMap<String, Displayed>,
    resized: bool,
) -> Vec<serde_json::Value> {
    let mut commands = Vec::new();
    for identifier in previous.keys() {
        if !next.contains_key(identifier) {
            commands.push(serde_json::json!({"action": "remove", "identifier": identifier}));
        }
    }
    for (identifier, item) in next {
        if resized || previous.get(identifier) != Some(item) {
            commands.push(serde_json::json!({
                "action": "add", "identifier": identifier, "path": item.path,
                // Images keep their aspect ratio and may be shorter than the
                // three-line slot. Anchor their center on the middle row.
                "x": item.image.area.x + item.image.area.width / 2,
                "y": item.image.area.y + item.image.area.height / 2,
                "max_width": item.image.area.width, "max_height": item.image.area.height,
            }));
        }
    }
    commands
}

fn cache_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|root| root.join("ytmusic-tui/artwork"))
}

fn cache_path(directory: &Path, url: &str) -> PathBuf {
    let hash: String = Sha1::digest(url.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    directory.join(format!("{hash}.img"))
}

async fn download(client: &reqwest::Client, url: &str, path: &Path) -> Result<(), ()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| ())?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err(());
    }
    let mut response = client
        .get(parsed)
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_IMAGE_BYTES as u64)
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    // Thumbnails are JPEG/PNG/WebP. Reject error pages and animated GIFs before
    // passing data to the external renderer.
    if !is_thumbnail(&bytes) {
        return Err(());
    }
    let mut file = tempfile::NamedTempFile::new_in(path.parent().ok_or(())?).map_err(|_| ())?;
    file.write_all(&bytes).map_err(|_| ())?;
    file.persist(path).map_err(|_| ())?;
    Ok(())
}

fn is_thumbnail(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

/// Keep our own cache bounded. ueberzugpp's separate resized-image disk cache
/// is disabled; currently visible files are protected during eviction.
fn trim_cache(directory: &Path, protected: &HashSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "img") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                (
                    path,
                    metadata.len(),
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                )
            })
        })
        .collect();
    files.sort_by_key(|(_, _, modified)| *modified);
    let mut count = files.len();
    let mut bytes: u64 = files.iter().map(|(_, size, _)| size).sum();
    for (path, size, _) in files {
        if count <= MAX_CACHE_FILES && bytes <= MAX_CACHE_BYTES {
            break;
        }
        if !protected.contains(&path) && fs::remove_file(path).is_ok() {
            count -= 1;
            bytes = bytes.saturating_sub(size);
        }
    }
}
