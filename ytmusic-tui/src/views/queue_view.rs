//! Queue view: displays the current playback queue with position highlight.
//!
//! Port of `src/ytmusic_tui/views/queue.py`. Shows all tracks with the
//! currently playing track marked `▶`. Enter jumps to that queue position
//! (Python used `set_playlist` with the selected index as `start_index`, which
//! effectively resumes the queue from that track).
//!
//! # Design choice: QueueSnapshot command
//!
//! The `QueueManager` lives in the runtime thread. Rather than share it across
//! threads, the view is populated via a dedicated
//! [`crate::app::AppCommand::FetchQueue`] /
//! [`crate::app::AppEvent::QueueSnapshot`] round-trip that sends a snapshot of
//! the queue's current state to the UI. The UI folds the snapshot into
//! [`QueueView`] via [`QueueView::set_snapshot`].
//!
//! This keeps the queue single-owned in the runtime; the snapshot hop is
//! invisible to the user (the view is read-only — the runtime re-emits
//! `QueueSnapshot` after any queue-mutating command).
//!
//! # NowPlaying → queue refresh
//!
//! When [`crate::app::AppEvent::NowPlaying`] arrives while the queue view is
//! visible, the UI fold returns [`crate::app::AppCommand::FetchQueue`] to
//! re-snapshot the queue. This keeps the `▶` marker accurate as tracks
//! advance (the same FetchQueue path used when the view is first opened).

use std::cell::Cell;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, TableState};
use ytmusic_api::Track;

use super::filter_bar::matches_filter;
use super::{PageState, Theme, borderless_table, table_header, table_row};
use crate::artwork::Placement;
use crate::formatting::format_duration;

const ART_ROW_HEIGHT: u16 = 3;
const ART_WIDTH: u16 = 6;
const ART_COLUMN_WIDTH: u16 = ART_WIDTH + 2;
const ART_MIN_WIDTH: u16 = 70;

/// A point-in-time snapshot of the queue (sent from the runtime thread).
///
/// The runtime emits this in response to
/// [`crate::app::AppCommand::FetchQueue`] and after any queue-mutating command.
/// Keeping it `Clone + PartialEq` makes it cheap to cache and compare.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueSnapshot {
    /// All tracks currently in the queue.
    pub tracks: Vec<Track>,
    /// Index of the currently playing track (`None` when the queue is idle).
    pub current_index: Option<usize>,
}

/// What an Enter keypress on the queue view resolves to.
///
/// Mirrors Python's `on_data_table_row_selected`: play from the selected
/// index, queueing the rest — same as the playlist/album/history views.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueAction {
    /// Jump to the selected queue position and resume playing. The runtime
    /// handles this by calling `play_playlist(tracks, start_index)`.
    JumpTo {
        tracks: Vec<Track>,
        start_index: usize,
    },
}

/// The queue view.
///
/// Holds a [`PageState`]-wrapped [`QueueSnapshot`] so the UI can display
/// "Loading…" before the snapshot arrives, and a row cursor for the user's
/// selection (distinct from the queue's own current-track marker).
#[derive(Debug, Clone)]
pub struct QueueView {
    state: PageState<QueueSnapshot>,
    cursor: usize,
    /// The active in-page filter query (`None` = no filter), set by the main
    /// loop from the filter bar. Navigation, the `▶` marker, and "jump to" all
    /// operate over the filtered subset when set.
    filter: Option<String>,
    artwork_enabled: bool,
    /// Shared by text and image layout; counts filtered rows, not terminal
    /// lines. Interior mutability keeps rendering compatible with other views.
    artwork_offset: Cell<usize>,
}

impl Default for QueueView {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueView {
    /// A fresh queue view in the loading state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: PageState::Loading,
            cursor: 0,
            filter: None,
            artwork_enabled: false,
            artwork_offset: Cell::new(0),
        }
    }

    /// The current fetch state.
    #[must_use]
    pub fn state(&self) -> &PageState<QueueSnapshot> {
        &self.state
    }

    /// Enable tall Queue rows only while an image backend is available.
    pub fn set_artwork_enabled(&mut self, enabled: bool) {
        if self.artwork_enabled != enabled {
            self.artwork_enabled = enabled;
            self.artwork_offset.set(0);
        }
    }

    // -- Data loading --------------------------------------------------------

    /// Apply a fresh queue snapshot (from [`crate::app::AppEvent::QueueSnapshot`]).
    ///
    /// The active filter is preserved across snapshots (the queue re-snapshots
    /// as tracks advance while the bar is open); the cursor is clamped against
    /// the *filtered* row count so it never points past the visible rows.
    pub fn set_snapshot(&mut self, snapshot: QueueSnapshot) {
        self.state = PageState::Loaded(snapshot);
        let visible = self.visible_indices().len();
        self.cursor = if visible == 0 {
            0
        } else {
            self.cursor.min(visible - 1)
        };
    }

    /// Reset to loading (issued when the queue view is first opened).
    pub fn set_loading(&mut self) {
        self.state = PageState::Loading;
        self.cursor = 0;
        self.filter = None;
        self.artwork_offset.set(0);
    }

    /// Transition into the error state.
    pub fn set_error(&mut self, message: impl Into<String>) {
        self.state = PageState::Error(message.into());
    }

    // -- In-page filter ------------------------------------------------------

    /// Set (or clear) the in-page filter query, resetting the cursor on change.
    pub fn set_filter(&mut self, query: Option<&str>) {
        let new = query.map(str::to_owned);
        if new != self.filter {
            self.filter = new;
            self.cursor = 0;
            self.artwork_offset.set(0);
        }
    }

    /// The indices into the snapshot's track list that pass the active filter.
    /// With no filter, every index is returned; empty when not loaded.
    fn visible_indices(&self) -> Vec<usize> {
        let Some(snap) = self.state.loaded() else {
            return Vec::new();
        };
        match &self.filter {
            None => (0..snap.tracks.len()).collect(),
            Some(q) => snap
                .tracks
                .iter()
                .enumerate()
                .filter(|(_, t)| matches_filter(q, &[&t.title, &t.artist, &t.album]))
                .map(|(i, _)| i)
                .collect(),
        }
    }

    /// The `(visible, total)` row counts for the filter bar's label.
    #[must_use]
    pub fn filter_counts(&self) -> (usize, usize) {
        let total = self.state.loaded().map_or(0, |s| s.tracks.len());
        (self.visible_indices().len(), total)
    }

    /// The queue track under the cursor, as a [`PopupItem`] for the action
    /// popup. `None` when empty / not loaded. Respects the active filter.
    #[must_use]
    pub fn selected_popup_item(&self) -> Option<super::popup::PopupItem> {
        let visible = self.visible_indices();
        let original = *visible.get(self.cursor)?;
        let track = self.state.loaded()?.tracks.get(original)?;
        Some(super::popup::PopupItem::Track(track.clone()))
    }

    // -- Navigation ----------------------------------------------------------

    /// The number of *visible* (post-filter) rows.
    fn track_count(&self) -> usize {
        self.visible_indices().len()
    }

    /// Move the cursor down one row, clamping at the end.
    pub fn select_next(&mut self) {
        let last = self.track_count().saturating_sub(1);
        if self.cursor < last {
            self.cursor += 1;
        }
    }

    /// Move the cursor up one row, clamping at the top.
    pub fn select_previous(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Resolve an Enter keypress into a [`QueueAction`], or `None`.
    ///
    /// Plays from the selected (visible) row, queueing the rest of the *visible*
    /// rows. With a filter active this re-seeds the queue with the filtered
    /// subset (matching what the user sees); unfiltered it keeps the whole queue
    /// and resumes from the selected row (Python's
    /// `set_playlist(tracks, start_index=row_index)`).
    #[must_use]
    pub fn activate_selected(&self) -> Option<QueueAction> {
        let snap = self.state.loaded()?;
        let visible = self.visible_indices();
        if visible.is_empty() || self.cursor >= visible.len() {
            return None;
        }
        let tracks: Vec<Track> = visible
            .iter()
            .filter_map(|&i| snap.tracks.get(i).cloned())
            .collect();
        Some(QueueAction::JumpTo {
            tracks,
            start_index: self.cursor,
        })
    }

    // -- Rendering -----------------------------------------------------------

    /// Render the queue view into `area`.
    ///
    /// A muted status line (track count) over a borderless DataTable with
    /// `# / Title / Artist / Album / Duration` columns, matching the queue SVG
    /// (no "Queue" panel border — the screen is flat `surface`).
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        // Status (1 row) + track table.
        let chunks = Layout::vertical([
            Constraint::Length(1), // status
            Constraint::Min(1),    // table
        ])
        .split(area);

        self.render_status(frame, chunks[0], theme);
        self.render_tracks(frame, chunks[1], theme);
    }

    fn uses_artwork(&self, area: Rect) -> bool {
        self.artwork_enabled && area.width >= ART_MIN_WIDTH && area.height >= ART_ROW_HEIGHT + 2
    }

    /// Return the exact filtered row slice shared by text and artwork. Only
    /// complete three-line rows fit; images can never extend into the player.
    fn artwork_rows(&self, area: Rect) -> Vec<(usize, Rect)> {
        if !self.uses_artwork(area) {
            return Vec::new();
        }
        let visible = self.visible_indices();
        let capacity = usize::from((area.height - 2) / ART_ROW_HEIGHT);
        let selected = self.cursor.min(visible.len().saturating_sub(1));
        let mut offset = self
            .artwork_offset
            .get()
            .min(visible.len().saturating_sub(capacity));
        if selected < offset {
            offset = selected;
        } else if selected >= offset + capacity {
            offset = selected + 1 - capacity;
        }
        self.artwork_offset.set(offset);
        visible
            .into_iter()
            .skip(offset)
            .take(capacity)
            .enumerate()
            .map(|(slot, original)| {
                (
                    original,
                    Rect {
                        x: area.x + 5 + 1,                            // position lane, then cell padding
                        y: area.y + 2 + slot as u16 * ART_ROW_HEIGHT, // status + header
                        width: ART_WIDTH,
                        height: ART_ROW_HEIGHT,
                    },
                )
            })
            .collect()
    }

    /// Images for the current viewport, in terminal cell coordinates. Missing
    /// URLs leave a placeholder and never cause extra metadata API requests.
    pub fn artwork_placements(&self, area: Rect) -> Vec<Placement> {
        let Some(snapshot) = self.state.loaded() else {
            return Vec::new();
        };
        self.artwork_rows(area)
            .into_iter()
            .filter_map(|(original, rect)| {
                let track = &snapshot.tracks[original];
                (!track.thumbnail_url.is_empty()).then(|| Placement {
                    identifier: format!("queue-{original}"),
                    url: track.thumbnail_url.clone(),
                    area: rect,
                })
            })
            .collect()
    }

    fn render_status(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (text, is_error) = self.status_text();
        let style = if is_error {
            Style::default().fg(theme.primary)
        } else {
            Style::default()
                .fg(theme.text_muted)
                .add_modifier(Modifier::ITALIC)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, style)))
                .style(Style::default().bg(theme.surface)),
            area,
        );
    }

    fn status_text(&self) -> (String, bool) {
        match &self.state {
            PageState::Loading => ("Loading queue...".to_owned(), false),
            PageState::Error(msg) => (msg.clone(), true),
            PageState::Loaded(snap) => {
                if snap.tracks.is_empty() {
                    ("Queue is empty".to_owned(), false)
                } else if self.filter.is_some() {
                    let visible = self.track_count();
                    (
                        format!("{visible}/{} track(s) in queue", snap.tracks.len()),
                        false,
                    )
                } else {
                    (format!("{} track(s) in queue", snap.tracks.len()), false)
                }
            }
        }
    }

    fn render_tracks(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        // The queue table fills its area with the focused-table styling even
        // when empty (the SVG paints empty rows in `row_bg`), so always draw the
        // header on a Loaded state.
        let PageState::Loaded(snap) = &self.state else {
            return;
        };
        // render_tracks receives the table area; reconstruct the view area
        // used by artwork_placements so both use identical geometry.
        let view_area = Rect {
            y: area.y.saturating_sub(1),
            height: area.height.saturating_add(1),
            ..area
        };
        if self.uses_artwork(view_area) {
            self.render_artwork_tracks(frame, area, view_area, theme, snap);
            return;
        }
        let visible = self.visible_indices();

        // Keep the original queue position number and the current-track marker
        // mapped to the original index, so a filtered view still shows real
        // positions and highlights the playing track when it is visible.
        let rows = visible
            .iter()
            .filter_map(|&i| snap.tracks.get(i).map(|t| (i, t)))
            .map(|(i, t)| {
                table_row(
                    theme,
                    &queue_columns(i + 1, t, snap.current_index == Some(i)),
                    true,
                )
            })
            .collect();

        let header = table_header(theme, &QUEUE_COLUMNS, true);
        let table = borderless_table(theme, header, rows, queue_widths(), true);

        let mut state = TableState::default();
        if !visible.is_empty() {
            state.select(Some(self.cursor.min(visible.len() - 1)));
        }
        frame.render_stateful_widget(table, area, &mut state);
    }

    fn render_artwork_tracks(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        view_area: Rect,
        theme: &Theme,
        snapshot: &QueueSnapshot,
    ) {
        let visible = self.artwork_rows(view_area);
        let rows = visible
            .iter()
            .map(|(original, _)| {
                let track = &snapshot.tracks[*original];
                let mut columns = queue_columns(
                    original + 1,
                    track,
                    snapshot.current_index == Some(*original),
                );
                columns.insert(1, "◇".into());
                for column in &mut columns {
                    *column = format!("\n {column} \n");
                }
                table_row(theme, &columns, true).height(ART_ROW_HEIGHT)
            })
            .collect();
        let header = table_header(
            theme,
            &["#", "", "Title", "Artist", "Album", "Duration"],
            true,
        );
        let table = borderless_table(
            theme,
            header,
            rows,
            vec![
                Constraint::Length(5),
                Constraint::Length(ART_COLUMN_WIDTH),
                Constraint::Min(12),
                Constraint::Percentage(22),
                Constraint::Percentage(22),
                Constraint::Length(10),
            ],
            true,
        );
        let mut state = TableState::default();
        if !visible.is_empty() {
            state.select(Some(self.cursor.saturating_sub(self.artwork_offset.get())));
        }
        frame.render_stateful_widget(table, area, &mut state);
    }
}

/// Column labels for the queue table (matching the queue SVG header row).
const QUEUE_COLUMNS: [&str; 5] = ["#", "Title", "Artist", "Album", "Duration"];

/// Column widths (including the one-space cell padding on each side). Title is
/// the flexible column; the rest are fixed lanes like the SVG.
fn queue_widths() -> Vec<Constraint> {
    vec![
        Constraint::Length(5),  // "#" + marker
        Constraint::Min(20),    // Title (flex)
        Constraint::Length(24), // Artist
        Constraint::Length(24), // Album
        Constraint::Length(10), // Duration
    ]
}

/// Format one queue track into its five column strings.
///
/// The `#` column carries the 1-based position with a leading `>` marker for
/// the currently playing track (Python's `">" if track == current else " "`).
fn queue_columns(num: usize, track: &Track, is_current: bool) -> Vec<String> {
    let marker = if is_current { ">" } else { " " };
    let duration = format_duration(track.duration_seconds);
    let duration = if duration == "—" {
        String::new()
    } else {
        duration
    };
    vec![
        format!("{marker}{num}"),
        track.title.clone(),
        track.artist.clone(),
        track.album.clone(),
        duration,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn artwork_queue(count: usize) -> QueueView {
        let mut view = QueueView::new();
        view.set_snapshot(QueueSnapshot {
            tracks: (0..count)
                .map(|index| {
                    Track::new(
                        format!("v{index}"),
                        format!("Song {index:03}"),
                        "Artist",
                        "Album",
                        180.0,
                        format!("https://example.test/cover-{index}.jpg"),
                    )
                })
                .collect(),
            current_index: Some(0),
        });
        view.set_artwork_enabled(true);
        view
    }

    #[test]
    fn hundred_track_queue_only_places_visible_images_and_scrolls_with_cursor() {
        let mut view = artwork_queue(100);
        let area = Rect::new(0, 0, 120, 12);
        let first = view.artwork_placements(area);
        assert_eq!(first.len(), 3);
        assert_eq!(first[0].identifier, "queue-0");
        assert_eq!(first[2].area, Rect::new(6, 8, 6, 3));
        for _ in 0..99 {
            view.select_next();
        }
        let last = view.artwork_placements(area);
        assert_eq!(last.len(), 3);
        assert_eq!(last[0].identifier, "queue-97");
        assert_eq!(last[2].identifier, "queue-99");
        assert!(
            last.iter()
                .all(|image| area.contains(image.area.as_position())
                    && image.area.bottom() <= area.bottom())
        );
        let text = render_to_string(&view, 120, 12);
        assert!(text.contains("Song 099"));
        assert!(!text.contains("Song 000"));
    }

    #[test]
    fn artwork_and_text_share_three_line_rows_with_centered_details() {
        let view = artwork_queue(3);
        let text = render_to_string(&view, 120, 12);
        let lines: Vec<_> = text.lines().collect();
        for (index, image) in view
            .artwork_placements(Rect::new(0, 0, 120, 12))
            .iter()
            .enumerate()
        {
            assert!(lines[usize::from(image.area.y + 1)].contains(&format!("Song {index:03}")));
            assert!(!lines[usize::from(image.area.y)].contains("Song"));
            assert!(!lines[usize::from(image.area.y + 2)].contains("Song"));
        }
    }

    #[test]
    fn artwork_filter_preserves_original_identifiers_and_resets_scroll() {
        let mut view = artwork_queue(100);
        for _ in 0..99 {
            view.select_next();
        }
        let area = Rect::new(0, 0, 120, 12);
        view.artwork_placements(area);
        view.set_filter(Some("Song 007"));
        let images = view.artwork_placements(area);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].identifier, "queue-7");
        assert_eq!(images[0].area.y, 2);
        match view.activate_selected().unwrap() {
            QueueAction::JumpTo {
                tracks,
                start_index,
            } => {
                assert_eq!(start_index, 0);
                assert_eq!(tracks[0].video_id, "v7");
            }
        }
    }

    #[test]
    fn artwork_resize_and_queue_shrink_keep_selected_row_inside_viewport() {
        let mut view = artwork_queue(100);
        for _ in 0..99 {
            view.select_next();
        }
        let small = view.artwork_placements(Rect::new(4, 2, 120, 5));
        assert_eq!(small.len(), 1);
        assert_eq!(small[0].identifier, "queue-99");
        assert_eq!(small[0].area, Rect::new(10, 4, 6, 3));
        view.set_snapshot(QueueSnapshot {
            tracks: vec![Track::new(
                "new",
                "New",
                "",
                "",
                0.0,
                "https://example.test/new.jpg",
            )],
            current_index: Some(0),
        });
        let images = view.artwork_placements(Rect::new(0, 0, 120, 12));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].identifier, "queue-0");
        assert_eq!(images[0].area.y, 2);
    }

    #[test]
    fn narrow_disabled_and_tiny_views_use_compact_rows_without_images() {
        let mut view = artwork_queue(3);
        assert!(view.artwork_placements(Rect::new(0, 0, 69, 12)).is_empty());
        assert!(view.artwork_placements(Rect::new(0, 0, 120, 4)).is_empty());
        let narrow = render_to_string(&view, 69, 12);
        assert!(narrow.lines().nth(2).unwrap().contains("Song 000"));
        view.set_artwork_enabled(false);
        assert!(view.artwork_placements(Rect::new(0, 0, 120, 12)).is_empty());
        assert!(
            render_to_string(&view, 120, 12)
                .lines()
                .nth(2)
                .unwrap()
                .contains("Song 000")
        );
    }

    #[test]
    fn missing_artwork_keeps_placeholder_without_requesting_an_empty_url() {
        let mut view = artwork_queue(1);
        view.set_snapshot(QueueSnapshot {
            tracks: vec![make_track("v1", "No cover", "Artist")],
            current_index: Some(0),
        });
        assert!(view.artwork_placements(Rect::new(0, 0, 120, 12)).is_empty());
        let text = render_to_string(&view, 120, 12);
        assert!(text.contains("No cover"));
        assert!(text.contains('◇'));
    }

    // -- fixtures ------------------------------------------------------------

    fn make_track(id: &str, title: &str, artist: &str) -> Track {
        Track::new(id, title, artist, "Album", 180.0, "")
    }

    fn snapshot_with_current(current: Option<usize>) -> QueueSnapshot {
        QueueSnapshot {
            tracks: vec![
                make_track("v1", "Airbag", "Radiohead"),
                make_track("v2", "Paranoid Android", "Radiohead"),
                make_track("v3", "Subterranean", "Radiohead"),
            ],
            current_index: current,
        }
    }

    fn loaded_view() -> QueueView {
        let mut view = QueueView::new();
        view.set_snapshot(snapshot_with_current(Some(1)));
        view
    }

    fn render_to_string(view: &QueueView, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| view.render(frame, frame.area(), &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let width = buffer.area().width as usize;
        let mut out = String::new();
        for (i, cell) in buffer.content().iter().enumerate() {
            out.push_str(cell.symbol());
            if (i + 1) % width == 0 {
                out.push('\n');
            }
        }
        out
    }

    // -- initial state -------------------------------------------------------

    #[test]
    fn new_view_is_loading() {
        let view = QueueView::new();
        assert!(matches!(view.state(), PageState::Loading));
        let text = render_to_string(&view, 60, 8);
        assert!(text.contains("Loading queue..."), "text:\n{text}");
    }

    // -- set_snapshot --------------------------------------------------------

    #[test]
    fn set_snapshot_loads_data() {
        let view = loaded_view();
        assert!(matches!(view.state(), PageState::Loaded(_)));
        assert_eq!(view.track_count(), 3);
    }

    #[test]
    fn set_snapshot_clamps_cursor_if_queue_shrank() {
        let mut view = loaded_view();
        view.cursor = 2;
        // Shorter snapshot.
        view.set_snapshot(QueueSnapshot {
            tracks: vec![make_track("v1", "Song", "Artist")],
            current_index: Some(0),
        });
        assert_eq!(view.cursor, 0);
    }

    // -- navigation ----------------------------------------------------------

    #[test]
    fn select_next_clamps_at_end() {
        let mut view = loaded_view(); // 3 tracks
        view.select_next();
        view.select_next();
        view.select_next(); // would be 3 -> clamps at 2
        assert_eq!(view.cursor, 2);
    }

    #[test]
    fn select_previous_clamps_at_top() {
        let mut view = loaded_view();
        view.select_previous();
        assert_eq!(view.cursor, 0);
    }

    // -- activation (Enter) --------------------------------------------------

    #[test]
    fn enter_on_row_produces_jump_to_with_correct_index() {
        let mut view = loaded_view();
        view.select_next(); // cursor 1
        match view.activate_selected() {
            Some(QueueAction::JumpTo {
                tracks,
                start_index,
            }) => {
                assert_eq!(start_index, 1);
                assert_eq!(tracks.len(), 3);
                assert_eq!(tracks[start_index].video_id, "v2");
            }
            other => panic!("expected JumpTo, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_empty_queue_is_none() {
        let mut view = QueueView::new();
        view.set_snapshot(QueueSnapshot {
            tracks: vec![],
            current_index: None,
        });
        assert!(view.activate_selected().is_none());
    }

    // -- error state ---------------------------------------------------------

    #[test]
    fn set_error_renders_message() {
        let mut view = QueueView::new();
        view.set_error("Runtime error");
        let text = render_to_string(&view, 60, 8);
        assert!(text.contains("Runtime error"), "text:\n{text}");
    }

    // -- rendering -----------------------------------------------------------

    #[test]
    fn loaded_render_shows_column_headers_and_is_borderless() {
        // The queue renders a borderless DataTable with the SVG's five columns.
        let view = loaded_view();
        let text = render_to_string(&view, 80, 10);
        for col in ["Title", "Artist", "Album", "Duration"] {
            assert!(text.contains(col), "missing '{col}' column header:\n{text}");
        }
        assert!(
            !text.contains('┌') && !text.contains('│'),
            "queue drew a box border:\n{text}"
        );
    }

    #[test]
    fn loaded_render_shows_tracks_and_count() {
        let view = loaded_view();
        let text = render_to_string(&view, 70, 10);
        assert!(text.contains("Airbag"), "missing first track:\n{text}");
        assert!(
            text.contains("Paranoid Android"),
            "missing second track:\n{text}"
        );
        assert!(
            text.contains("3 track(s) in queue"),
            "missing count:\n{text}"
        );
    }

    #[test]
    fn loaded_render_shows_current_track_marker() {
        let view = loaded_view(); // current_index = 1
        let text = render_to_string(&view, 70, 10);
        // The ">" marker should appear in the rendered output for the current track.
        assert!(text.contains('>'), "missing current marker:\n{text}");
    }

    #[test]
    fn empty_queue_render_shows_empty_message() {
        let mut view = QueueView::new();
        view.set_snapshot(QueueSnapshot {
            tracks: vec![],
            current_index: None,
        });
        let text = render_to_string(&view, 60, 8);
        assert!(text.contains("Queue is empty"), "text:\n{text}");
    }

    // -- in-page filter ------------------------------------------------------

    #[test]
    fn filter_narrows_visible_rows() {
        let mut view = QueueView::new();
        view.set_snapshot(QueueSnapshot {
            tracks: vec![
                make_track("v1", "Airbag", "Radiohead"),
                make_track("v2", "Get Lucky", "Daft Punk"),
                make_track("v3", "Karma Police", "Radiohead"),
            ],
            current_index: Some(0),
        });
        view.set_filter(Some("daft"));
        assert_eq!(view.track_count(), 1);
    }

    #[test]
    fn filter_jump_queues_visible_subset() {
        let mut view = QueueView::new();
        view.set_snapshot(QueueSnapshot {
            tracks: vec![
                make_track("v1", "Airbag", "Radiohead"),
                make_track("v2", "Get Lucky", "Daft Punk"),
                make_track("v3", "Karma Police", "Radiohead"),
            ],
            current_index: Some(0),
        });
        view.set_filter(Some("radiohead"));
        match view.activate_selected() {
            Some(QueueAction::JumpTo {
                tracks,
                start_index,
            }) => {
                assert_eq!(tracks.len(), 2);
                assert_eq!(start_index, 0);
                assert_eq!(tracks[0].video_id, "v1");
                assert_eq!(tracks[1].video_id, "v3");
            }
            other => panic!("expected JumpTo, got {other:?}"),
        }
    }

    #[test]
    fn filter_render_shows_filtered_count() {
        let mut view = QueueView::new();
        view.set_snapshot(QueueSnapshot {
            tracks: vec![
                make_track("v1", "Airbag", "Radiohead"),
                make_track("v2", "Get Lucky", "Daft Punk"),
            ],
            current_index: Some(0),
        });
        view.set_filter(Some("lucky"));
        let text = render_to_string(&view, 70, 10);
        assert!(text.contains("Get Lucky"), "missing match:\n{text}");
        assert!(!text.contains("Airbag"), "non-match shown:\n{text}");
        assert!(
            text.contains("1/2 track(s) in queue"),
            "missing filtered count:\n{text}"
        );
    }

    #[test]
    fn snapshot_preserves_filter() {
        let mut view = QueueView::new();
        view.set_snapshot(snapshot_with_current(Some(0)));
        view.set_filter(Some("airbag"));
        assert_eq!(view.track_count(), 1);
        // A re-snapshot (queue advanced) keeps the filter applied.
        view.set_snapshot(snapshot_with_current(Some(1)));
        assert_eq!(view.track_count(), 1, "filter survives a re-snapshot");
    }
}
