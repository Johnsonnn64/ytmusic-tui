//! Song notifications run outside the terminal and playback threads. A latest
//! value channel cancels outdated cover downloads when songs are skipped.

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::watch;

use crate::app::{AppEvent, NowPlaying};
use crate::artwork::cache::{cache_directory, cache_path, download, trim_cache};

#[derive(Default)]
struct Selection {
    current: Option<NowPlaying>,
    announced: bool,
}

enum Update {
    None,
    Cancel,
    Show(NowPlaying),
}

impl Selection {
    fn on_event(&mut self, event: &AppEvent) -> Update {
        match event {
            AppEvent::NowPlaying(now) => {
                let changed =
                    self.current.as_ref().map(|track| &track.video_id) != Some(&now.video_id);
                self.current = (!now.video_id.is_empty()).then(|| now.clone());
                if changed {
                    self.announced = false;
                    return Update::Cancel;
                }
            }
            AppEvent::PlayerStarted => {
                self.announced = false;
                return Update::Cancel;
            }
            AppEvent::PlayerLoaded if !self.announced => {
                if let Some(now) = &self.current {
                    self.announced = true;
                    return Update::Show(now.clone());
                }
            }
            AppEvent::PlaybackStopped | AppEvent::TrackEnded | AppEvent::TrackError(_) => {
                self.current = None;
                self.announced = false;
                return Update::Cancel;
            }
            _ => {}
        }
        Update::None
    }
}

/// Best-effort desktop notifications. Missing notify-send or a notification
/// daemon never interrupts playback. No I/O happens in on_event().
pub struct Notifications {
    selection: Selection,
    sender: Option<watch::Sender<Option<NowPlaying>>>,
    worker: Option<JoinHandle<()>>,
}

impl Notifications {
    pub fn new(enabled: bool) -> Self {
        let mut notifications = Self {
            selection: Selection::default(),
            sender: None,
            worker: None,
        };
        if enabled {
            let (sender, receiver) = watch::channel(None);
            if let Ok(worker) = std::thread::Builder::new()
                .name("notifications".into())
                .spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        runtime.block_on(work(receiver));
                    }
                })
            {
                notifications.sender = Some(sender);
                notifications.worker = Some(worker);
            }
        }
        notifications
    }

    pub fn on_event(&mut self, event: &AppEvent) {
        let Some(sender) = &self.sender else {
            return;
        };
        match self.selection.on_event(event) {
            Update::None => {}
            Update::Cancel => {
                sender.send_replace(None);
            }
            Update::Show(now) => {
                sender.send_replace(Some(now));
            }
        }
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn work(mut receiver: watch::Receiver<Option<NowPlaying>>) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .connect_timeout(Duration::from_secs(2))
        .build()
        .ok();
    while receiver.changed().await.is_ok() {
        loop {
            let Some(now) = receiver.borrow_and_update().clone() else {
                break;
            };
            tokio::select! {
                biased;
                changed = receiver.changed() => {
                    if changed.is_err() { return; }
                }
                _ = show(client.as_ref(), &now) => break,
            }
        }
    }
}

async fn show(client: Option<&reqwest::Client>, now: &NowPlaying) {
    let mut icon = None;
    if !now.thumbnail_url.is_empty()
        && let (Some(client), Some(directory)) = (client, cache_directory())
        && std::fs::create_dir_all(&directory).is_ok()
    {
        let path = cache_path(&directory, &now.thumbnail_url);
        if path.is_file() || download(client, &now.thumbnail_url, &path).await.is_ok() {
            trim_cache(&directory, &HashSet::from([path.clone()]));
            icon = Some(path);
        }
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        notification_command(now, icon.as_deref()).status(),
    )
    .await;
}

fn notification_command(now: &NowPlaying, icon: Option<&Path>) -> Command {
    let mut command = Command::new("notify-send");
    command.args(["--app-name", "ytmusic-tui"]);
    if let Some(icon) = icon {
        command.arg("--icon").arg(icon);
    }
    // notify-send interprets body markup. Escape artist text and pass arguments
    // directly, so quotes, shell syntax, and leading hyphens stay literal.
    let artist = now
        .artist
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    command
        .arg("--")
        .arg(&now.title)
        .arg(artist)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::RepeatMode;

    fn song(id: &str) -> NowPlaying {
        NowPlaying {
            video_id: id.into(),
            title: "--title \"$(touch nope)\"".into(),
            artist: "A & <B>".into(),
            album: String::new(),
            thumbnail_url: String::new(),
            duration_seconds: 100.0,
            shuffle: false,
            repeat: RepeatMode::Off,
        }
    }

    #[test]
    fn announces_only_loaded_songs_and_ignores_mode_and_pause_changes() {
        let mut selection = Selection::default();
        assert!(matches!(
            selection.on_event(&AppEvent::NowPlaying(song("1"))),
            Update::Cancel
        ));
        assert!(matches!(
            selection.on_event(&AppEvent::PlayerStarted),
            Update::Cancel
        ));
        assert!(
            matches!(selection.on_event(&AppEvent::PlayerLoaded), Update::Show(now) if now.video_id == "1")
        );
        let mut shuffled = song("1");
        shuffled.shuffle = true;
        assert!(matches!(
            selection.on_event(&AppEvent::NowPlaying(shuffled)),
            Update::None
        ));
        assert!(matches!(
            selection.on_event(&AppEvent::PlayerPaused(false)),
            Update::None
        ));
        assert!(matches!(
            selection.on_event(&AppEvent::PlayerLoaded),
            Update::None
        ));
        // Replaying the same track is a fresh start, so repeat-one works too.
        selection.on_event(&AppEvent::PlayerStarted);
        assert!(matches!(
            selection.on_event(&AppEvent::PlayerLoaded),
            Update::Show(_)
        ));
    }

    #[test]
    fn failed_stopped_and_replaced_tracks_cancel_pending_notifications() {
        let mut selection = Selection::default();
        for event in [
            AppEvent::TrackError("failed".into()),
            AppEvent::TrackEnded,
            AppEvent::PlaybackStopped,
        ] {
            selection.on_event(&AppEvent::NowPlaying(song("1")));
            assert!(matches!(selection.on_event(&event), Update::Cancel));
            assert!(matches!(
                selection.on_event(&AppEvent::PlayerLoaded),
                Update::None
            ));
        }
        selection.on_event(&AppEvent::NowPlaying(song("1")));
        selection.on_event(&AppEvent::NowPlaying(song("2")));
        assert!(
            matches!(selection.on_event(&AppEvent::PlayerLoaded), Update::Show(now) if now.video_id == "2")
        );
    }

    #[test]
    fn command_preserves_title_artist_and_optional_cover_as_separate_arguments() {
        let now = song("1");
        for icon in [None, Some(Path::new("/cache/album cover.img"))] {
            let command = notification_command(&now, icon);
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let mut expected = vec!["--app-name", "ytmusic-tui"];
            if icon.is_some() {
                expected.extend(["--icon", "/cache/album cover.img"]);
            }
            expected.extend(["--", &now.title, "A &amp; &lt;B&gt;"]);
            assert_eq!(args, expected);
        }
    }
}
