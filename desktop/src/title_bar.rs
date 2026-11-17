//! The main window's own top row, which replaces the system title bar: where
//! a point on the window falls (the Windows hit test), and the keys that
//! put the focus on the header's gear.

/// The top row's height and each window button's width, in logical pixels;
/// the `TitleBar` in ui/main.slint draws them at these sizes. The hit test
/// that reads these items runs on Windows only.
#[cfg(any(windows, test))]
pub const ROW_HEIGHT: f32 = 32.0;
#[cfg(any(windows, test))]
pub const BUTTON_WIDTH: f32 = 46.0;
#[cfg(any(windows, test))]
/// How far inside the window's edge a press resizes it.
pub const RESIZE_BORDER: f32 = 8.0;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// The window's content and its own buttons, which Slint handles.
    Client,
    /// Empty top row: drags the window, as a title bar does.
    Caption,
    /// The maximize button, which Windows 11 needs to know to offer its
    /// snap layouts on hover.
    Maximize,
    Resize(Edge),
}

#[cfg(any(windows, test))]
/// The window as the hit test reads it, in logical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

#[cfg(any(windows, test))]
pub fn hit_test(x: f32, y: f32, layout: Layout) -> Region {
    if !layout.maximized {
        let west = x < RESIZE_BORDER;
        let east = x >= layout.width - RESIZE_BORDER;
        let north = y < RESIZE_BORDER;
        let south = y >= layout.height - RESIZE_BORDER;
        let edge = match (north, south, west, east) {
            (true, _, true, _) => Some(Edge::NorthWest),
            (true, _, _, true) => Some(Edge::NorthEast),
            (_, true, true, _) => Some(Edge::SouthWest),
            (_, true, _, true) => Some(Edge::SouthEast),
            (true, ..) => Some(Edge::North),
            (_, true, ..) => Some(Edge::South),
            (_, _, true, _) => Some(Edge::West),
            (_, _, _, true) => Some(Edge::East),
            _ => None,
        };
        if let Some(edge) = edge {
            return Region::Resize(edge);
        }
    }
    if y >= ROW_HEIGHT {
        return Region::Client;
    }
    let buttons = layout.width - 3.0 * BUTTON_WIDTH;
    let maximize = layout.width - 2.0 * BUTTON_WIDTH;
    if x >= maximize && x < maximize + BUTTON_WIDTH {
        Region::Maximize
    } else if x < buttons {
        Region::Caption
    } else {
        Region::Client
    }
}

/// A key as the gear's Alt and F10 read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKeyInput {
    Alt,
    F10,
    Other,
}

/// The modifiers held with a key; AltGr reads as Ctrl and Alt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
}

/// Remembers whether Alt went down with no other key since, so its release
/// focuses the gear, where a Windows menu bar would take the focus.
#[derive(Debug, Default)]
pub struct MenuKeys {
    alt_alone: bool,
}

impl MenuKeys {
    /// The window lost or gained the focus: a held Alt went to another window.
    pub fn focus_changed(&mut self) {
        self.alt_alone = false;
    }

    /// True when the key puts the focus on the gear: Alt pressed and
    /// released alone, or F10.
    pub fn key(&mut self, key: MenuKeyInput, pressed: bool, mods: Mods) -> bool {
        let alt_alone = std::mem::take(&mut self.alt_alone);
        match (key, pressed) {
            (MenuKeyInput::Alt, true) => {
                self.alt_alone = true;
                false
            }
            (MenuKeyInput::Alt, false) => alt_alone,
            (MenuKeyInput::F10, true) => !mods.alt && !mods.ctrl && !mods.shift,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE: Layout = Layout {
        width: 800.0,
        height: 600.0,
        maximized: false,
    };

    #[test]
    fn edges_and_corners_resize_within_the_border() {
        assert_eq!(hit_test(2.0, 300.0, WIDE), Region::Resize(Edge::West));
        assert_eq!(hit_test(797.0, 300.0, WIDE), Region::Resize(Edge::East));
        assert_eq!(hit_test(400.0, 1.0, WIDE), Region::Resize(Edge::North));
        assert_eq!(hit_test(400.0, 595.0, WIDE), Region::Resize(Edge::South));
        assert_eq!(hit_test(3.0, 3.0, WIDE), Region::Resize(Edge::NorthWest));
        assert_eq!(hit_test(796.0, 2.0, WIDE), Region::Resize(Edge::NorthEast));
        assert_eq!(hit_test(1.0, 599.0, WIDE), Region::Resize(Edge::SouthWest));
        assert_eq!(
            hit_test(799.0, 598.0, WIDE),
            Region::Resize(Edge::SouthEast)
        );
        assert_eq!(hit_test(8.0, 300.0, WIDE), Region::Client);
    }

    #[test]
    fn the_top_row_drags_up_to_the_buttons() {
        assert_eq!(hit_test(100.0, 16.0, WIDE), Region::Caption);
        assert_eq!(hit_test(500.0, 31.0, WIDE), Region::Caption);
        assert_eq!(hit_test(500.0, 32.0, WIDE), Region::Client);
        let buttons = 800.0 - 3.0 * BUTTON_WIDTH;
        assert_eq!(hit_test(buttons - 1.0, 16.0, WIDE), Region::Caption);
        assert_eq!(hit_test(buttons + 1.0, 16.0, WIDE), Region::Client);
        assert_eq!(hit_test(800.0 - 20.0, 16.0, WIDE), Region::Client);
    }

    #[test]
    fn the_middle_button_is_the_maximize_button() {
        let start = 800.0 - 2.0 * BUTTON_WIDTH;
        assert_eq!(hit_test(start, 16.0, WIDE), Region::Maximize);
        assert_eq!(
            hit_test(start + BUTTON_WIDTH - 1.0, 16.0, WIDE),
            Region::Maximize
        );
        assert_eq!(hit_test(start - 1.0, 16.0, WIDE), Region::Client);
    }

    #[test]
    fn a_maximized_window_has_no_resize_border() {
        let maximized = Layout {
            maximized: true,
            ..WIDE
        };
        assert_eq!(hit_test(2.0, 300.0, maximized), Region::Client);
        assert_eq!(hit_test(400.0, 1.0, maximized), Region::Caption);
        assert_eq!(
            hit_test(800.0 - 2.0 * BUTTON_WIDTH + 1.0, 1.0, maximized),
            Region::Maximize
        );
    }

    fn press(keys: &mut MenuKeys, key: MenuKeyInput) -> bool {
        let alt = key == MenuKeyInput::Alt;
        keys.key(
            key,
            true,
            Mods {
                alt,
                ..Mods::default()
            },
        )
    }

    fn release(keys: &mut MenuKeys, key: MenuKeyInput) -> bool {
        keys.key(key, false, Mods::default())
    }

    #[test]
    fn alt_alone_or_f10_focuses_the_gear() {
        let mut keys = MenuKeys::default();
        assert!(!press(&mut keys, MenuKeyInput::Alt));
        assert!(release(&mut keys, MenuKeyInput::Alt));
        assert!(press(&mut keys, MenuKeyInput::F10));
        assert!(!release(&mut keys, MenuKeyInput::F10));
        assert!(
            !keys.key(
                MenuKeyInput::F10,
                true,
                Mods {
                    shift: true,
                    ..Mods::default()
                },
            ),
            "Shift+F10 opens a context menu instead"
        );
    }

    #[test]
    fn alt_released_after_another_key_does_nothing() {
        let mut keys = MenuKeys::default();
        press(&mut keys, MenuKeyInput::Alt);
        press(&mut keys, MenuKeyInput::Other);
        assert!(!release(&mut keys, MenuKeyInput::Alt));
    }

    #[test]
    fn alt_released_after_the_focus_moved_does_nothing() {
        let mut keys = MenuKeys::default();
        press(&mut keys, MenuKeyInput::Alt);
        keys.focus_changed();
        assert!(!release(&mut keys, MenuKeyInput::Alt), "Alt+Tab");
    }
}
