//! Center ueberzugpp's image windows and ensure their initial paint reaches
//! X11. Runs on the artwork worker, never the terminal thread.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::terminal::WindowSize;
use x11rb::connection::Connection;
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt, EXPOSE_EVENT, EventMask, ExposeEvent,
    GetGeometryReply, Window,
};
use x11rb::rust_connection::RustConnection;

use super::Displayed;

pub(super) struct ImageWindows {
    connection: RustConnection,
    root: Window,
    parent: Option<Window>,
    renderer_pid: u32,
    windows: HashMap<Window, InitialPaint>,
}

enum InitialPaint {
    Pending(Instant),
    Ready,
}

impl ImageWindows {
    pub fn connect(renderer_pid: u32) -> Option<Self> {
        let parent = std::env::var("WINDOWID").ok().and_then(|window| {
            match window
                .strip_prefix("0x")
                .or_else(|| window.strip_prefix("0X"))
            {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => window.parse().ok(),
            }
        });
        let (connection, screen) = x11rb::connect(None).ok()?;
        let root = connection.setup().roots[screen].root;
        Some(Self {
            connection,
            root,
            parent,
            renderer_pid,
            windows: HashMap::new(),
        })
    }

    pub fn refresh(&mut self, images: &HashMap<String, Displayed>) {
        if images.is_empty() {
            self.windows.clear();
            return;
        }
        let _ = self.try_refresh(images);
    }

    fn try_refresh(&mut self, images: &HashMap<String, Displayed>) -> Option<()> {
        // Some terminals omit WINDOWID. Discover the parent once our renderer
        // creates its first image, so initial painting still works there.
        let parent = self.parent.or_else(|| self.find_parent())?;
        self.parent = Some(parent);
        let cells = crossterm::terminal::window_size()
            .ok()
            .and_then(|terminal| {
                let geometry = self.connection.get_geometry(parent).ok()?.reply().ok()?;
                CellGeometry::new(&terminal, geometry.width, geometry.height)
            });
        let children = self
            .connection
            .query_tree(parent)
            .ok()?
            .reply()
            .ok()?
            .children;
        self.windows.retain(|window, _| children.contains(window));
        for window in children {
            match self.windows.get(&window) {
                Some(InitialPaint::Ready) => continue,
                // Mapping precedes frame generation in ueberzugpp. Let it
                // finish before requesting the one-time redraw burst.
                Some(InitialPaint::Pending(at)) if at.elapsed() < Duration::from_millis(100) => {
                    continue;
                }
                Some(InitialPaint::Pending(_)) => {}
                None => {
                    if self.owns_window(window) {
                        self.windows
                            .insert(window, InitialPaint::Pending(Instant::now()));
                    }
                    continue;
                }
            }
            let Ok(geometry) = self.connection.get_geometry(window) else {
                continue;
            };
            let Ok(geometry) = geometry.reply() else {
                continue;
            };
            // Only Y changes. X, thumbnail size, and the terminal's own windows
            // remain as ueberzugpp placed them. A disappearing window is normal
            // during scroll; checked requests keep that race non-fatal.
            if let Some(y) = cells
                .as_ref()
                .and_then(|cells| cells.centered_y(images, &geometry))
            {
                self.connection
                    .configure_window(window, &ConfigureWindowAux::new().y(y))
                    .ok()?
                    .check()
                    .ok()?;
            }
            self.redraw(window, &geometry)?;
            self.windows.insert(window, InitialPaint::Ready);
        }
        self.connection.flush().ok()?;
        Some(())
    }

    fn owns_window(&self, window: Window) -> bool {
        let matches = || {
            let name = self
                .connection
                .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 4)
                .ok()?
                .reply()
                .ok()?;
            if name.value != b"ueberzugpp" {
                return None;
            }
            let owner = self
                .connection
                .res_query_client_ids(&[ClientIdSpec {
                    client: window,
                    mask: ClientIdMask::LOCAL_CLIENT_PID,
                }])
                .ok()?
                .reply()
                .ok()?;
            Some(
                owner
                    .ids
                    .iter()
                    .any(|id| id.value.first() == Some(&self.renderer_pid)),
            )
        };
        matches().unwrap_or(false)
    }

    fn find_parent(&self) -> Option<Window> {
        let mut pending = vec![self.root];
        while let Some(parent) = pending.pop() {
            let Ok(tree) = self.connection.query_tree(parent) else {
                continue;
            };
            let Ok(tree) = tree.reply() else { continue };
            for child in tree.children {
                if self.owns_window(child) {
                    return Some(parent);
                }
                pending.push(child);
            }
        }
        None
    }

    fn redraw(&self, window: Window, geometry: &GetGeometryReply) -> Option<()> {
        let bits_per_pixel = self
            .connection
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == geometry.depth)?
            .bits_per_pixel;
        let event = ExposeEvent {
            response_type: EXPOSE_EVENT,
            window,
            width: geometry.width,
            height: geometry.height,
            ..Default::default()
        };
        // ueberzugpp 2.9.10's X11Window::draw queues xcb_image_put without flushing:
        // https://github.com/jstkdng/ueberzugpp/blob/master/src/canvas/x11/window/x11.cpp
        // Small video thumbnails can remain entirely in libxcb's 16 KiB output
        // buffer. Enough Expose events to overflow it guarantee a complete
        // frame reaches the server. Do this once per new XID, not each UI tick.
        for _ in 0..redraw_count(geometry.width, geometry.height, bits_per_pixel) {
            self.connection
                .send_event(false, window, EventMask::EXPOSURE, event)
                .ok()?;
        }
        Some(())
    }
}

fn redraw_count(width: u16, height: u16, bits_per_pixel: u8) -> u64 {
    let bytes = (u64::from(width) * u64::from(height) * u64::from(bits_per_pixel) / 8).max(1);
    16 * 1024 / bytes + 2
}

struct CellGeometry {
    height: f64,
    padding_y: f64,
}

impl CellGeometry {
    /// Match ueberzugpp's cell metrics, including Alacritty's padding: ioctl
    /// can report either the text grid's pixel size or the whole window size.
    fn new(terminal: &WindowSize, parent_width: u16, parent_height: u16) -> Option<Self> {
        if terminal.rows == 0 || terminal.columns == 0 {
            return None;
        }
        let (width, height) = if terminal.width == 0 || terminal.height == 0 {
            (parent_width, parent_height)
        } else {
            (terminal.width, terminal.height)
        };
        let rows = f64::from(terminal.rows);
        let width = f64::from(width);
        let height = f64::from(height);
        let text_grid = width < f64::from(parent_width) && height < f64::from(parent_height);
        let cells = if text_grid {
            Self {
                height: height / rows,
                padding_y: (f64::from(parent_height) - height) / 2.0,
            }
        } else {
            let cell_height = (height / rows).floor();
            Self {
                height: cell_height,
                padding_y: (height - rows * cell_height) / 2.0,
            }
        };
        (cells.height > 0.0).then_some(cells)
    }

    fn centered_y(
        &self,
        images: &HashMap<String, Displayed>,
        window: &GetGeometryReply,
    ) -> Option<i32> {
        let image_center = f64::from(window.y) + f64::from(window.height) / 2.0;
        images
            .values()
            .filter_map(|image| {
                let area = image.image.area;
                let center = self.padding_y
                    + (f64::from(area.y) + f64::from(area.height) / 2.0) * self.height;
                let distance = (center - image_center).abs();
                (distance <= self.height).then_some((
                    distance,
                    (center - f64::from(window.height) / 2.0).round() as i32,
                ))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, y)| y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artwork::Placement;
    use ratatui::layout::Rect;
    use std::path::PathBuf;

    #[test]
    fn small_frames_force_a_complete_upload_out_of_the_xcb_buffer() {
        // Portrait, square, and video thumbnails at several terminal zooms,
        // including the 60x33 video frame that reproduced a black rectangle.
        for (width, height) in [(1, 1), (6, 3), (30, 60), (60, 60), (60, 33), (120, 67)] {
            for bits in [16, 24, 32] {
                let bytes = u64::from(width) * u64::from(height) * u64::from(bits) / 8;
                let count = redraw_count(width, height, bits);
                assert!((count - 1) * bytes > 16 * 1024);
            }
        }
        // Large uploads already exceed the buffer; avoid unbounded bursts or
        // arithmetic overflow when a terminal reports extreme dimensions.
        assert_eq!(redraw_count(u16::MAX, u16::MAX, 32), 2);
    }

    fn images() -> HashMap<String, Displayed> {
        HashMap::from([(
            "queue-0".into(),
            Displayed {
                image: Placement {
                    identifier: "queue-0".into(),
                    url: String::new(),
                    area: Rect::new(6, 3, 6, 3),
                },
                path: PathBuf::new(),
            },
        )])
    }

    #[test]
    fn square_and_wide_images_center_on_same_row_in_pixels() {
        let cells = CellGeometry {
            height: 24.0,
            padding_y: 4.0,
        };
        for height in [60_u16, 34_u16] {
            let window = GetGeometryReply {
                x: 62,
                y: 76,
                height,
                ..Default::default()
            };
            let y = cells.centered_y(&images(), &window).unwrap();
            assert_eq!(f64::from(y) + f64::from(height) / 2.0, 112.0);
            assert!(y >= 76);
            assert!(y + i32::from(height) <= 148);
        }
    }

    #[test]
    fn text_grid_and_whole_window_metrics_handle_terminal_padding() {
        let text = WindowSize {
            columns: 100,
            rows: 30,
            width: 1000,
            height: 720,
        };
        let padded = WindowSize {
            columns: 100,
            rows: 30,
            width: 1004,
            height: 728,
        };
        for size in [text, padded] {
            let cells = CellGeometry::new(&size, 1004, 728).unwrap();
            assert_eq!(cells.height, 24.0);
            assert_eq!(cells.padding_y, 4.0);
        }
        let missing_pixels = WindowSize {
            columns: 100,
            rows: 30,
            width: 0,
            height: 0,
        };
        assert_eq!(
            CellGeometry::new(&missing_pixels, 1004, 728)
                .unwrap()
                .height,
            24.0
        );
    }

    #[test]
    fn windows_outside_queue_rows_are_not_moved() {
        let cells = CellGeometry {
            height: 24.0,
            padding_y: 4.0,
        };
        assert!(
            cells
                .centered_y(
                    &images(),
                    &GetGeometryReply {
                        x: 62,
                        y: 400,
                        height: 34,
                        ..Default::default()
                    }
                )
                .is_none()
        );
    }
}
