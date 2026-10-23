//! Correct ueberzugpp's cell-rounded vertical origin using the rendered X11
//! window height. Runs on the artwork worker, never the terminal thread.

use std::collections::{HashMap, HashSet};

use crossterm::terminal::WindowSize;
use x11rb::connection::Connection;
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt, GetGeometryReply, Window,
};
use x11rb::rust_connection::RustConnection;

use super::Displayed;

pub(super) struct VerticalAlignment {
    connection: RustConnection,
    parent: Window,
    renderer_pid: u32,
    aligned: HashSet<Window>,
}

impl VerticalAlignment {
    pub fn connect(renderer_pid: u32) -> Option<Self> {
        let window = std::env::var("WINDOWID").ok()?;
        let parent = match window
            .strip_prefix("0x")
            .or_else(|| window.strip_prefix("0X"))
        {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => window.parse().ok()?,
        };
        let (connection, _) = x11rb::connect(None).ok()?;
        Some(Self {
            connection,
            parent,
            renderer_pid,
            aligned: HashSet::new(),
        })
    }

    pub fn center(&mut self, images: &HashMap<String, Displayed>) {
        if images.is_empty() {
            self.aligned.clear();
            return;
        }
        let _ = self.try_center(images);
    }

    fn try_center(&mut self, images: &HashMap<String, Displayed>) -> Option<()> {
        let terminal = crossterm::terminal::window_size().ok()?;
        let parent = self
            .connection
            .get_geometry(self.parent)
            .ok()?
            .reply()
            .ok()?;
        let cells = CellGeometry::new(&terminal, parent.width, parent.height)?;
        let children = self
            .connection
            .query_tree(self.parent)
            .ok()?
            .reply()
            .ok()?
            .children;
        self.aligned.retain(|window| children.contains(window));
        for window in children {
            if self.aligned.contains(&window) {
                continue;
            }
            let Ok(name) = self.connection.get_property(
                false,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                0,
                4,
            ) else {
                continue;
            };
            let Ok(name) = name.reply() else { continue };
            if name.value != b"ueberzugpp" {
                continue;
            }
            // Match our renderer's process, rather than relying on X offsets:
            // ueberzugpp may infer different horizontal terminal padding. This
            // also leaves images from another pane/application untouched.
            let Ok(owner) = self.connection.res_query_client_ids(&[ClientIdSpec {
                client: window,
                mask: ClientIdMask::LOCAL_CLIENT_PID,
            }]) else {
                continue;
            };
            let Ok(owner) = owner.reply() else { continue };
            if !owner
                .ids
                .iter()
                .any(|id| id.value.first() == Some(&self.renderer_pid))
            {
                continue;
            }
            let Ok(geometry) = self.connection.get_geometry(window) else {
                continue;
            };
            let Ok(geometry) = geometry.reply() else {
                continue;
            };
            let Some(y) = cells.centered_y(images, &geometry) else {
                continue;
            };
            // Only Y changes. X, thumbnail size, and the terminal's own windows
            // remain as ueberzugpp placed them. A disappearing window is normal
            // during scroll; checked requests keep that race non-fatal.
            let Ok(change) = self
                .connection
                .configure_window(window, &ConfigureWindowAux::new().y(y))
            else {
                continue;
            };
            if change.check().is_ok() {
                self.aligned.insert(window);
            }
        }
        self.connection.flush().ok()?;
        Some(())
    }
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
