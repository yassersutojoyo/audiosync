//! The main window's own top row, which replaces the system title bar: where
//! a point on the window falls (the Windows hit test), and the keys that
//! reach its menus.

/// The top row's height and each window button's width, in logical pixels;
/// the `TitleBar` in ui/main.slint draws them at these sizes.
pub const ROW_HEIGHT: f32 = 32.0;
pub const BUTTON_WIDTH: f32 = 46.0;
/// How far inside the window's edge a press resizes it.
pub const RESIZE_BORDER: f32 = 8.0;

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

/// The window as the hit test reads it, in logical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    /// Where the menus end and the draggable part of the top row begins.
    pub caption_start: f32,
    pub maximized: bool,
}

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
    } else if x >= layout.caption_start && x < buttons {
        Region::Caption
    } else {
        Region::Client
    }
}

/// A key as the menu row reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKeyInput {
    Alt,
    F10,
    Char(char),
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKey {
    /// Alt pressed and released alone, or F10: the first menu takes the focus.
    FocusRow,
    /// Alt and a menu's first letter: that menu opens.
    Open(usize),
}

/// The modifiers held with a key; AltGr reads as Ctrl and Alt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
}

/// Remembers whether Alt went down with no other key since, so its release
/// focuses the menu row as a Windows menu bar does.
#[derive(Debug, Default)]
pub struct MenuKeys {
    alt_alone: bool,
}

impl MenuKeys {
    /// The window lost or gained the focus: a held Alt went to another window.
    pub fn focus_changed(&mut self) {}

    pub fn key(
        &mut self,
        key: MenuKeyInput,
        pressed: bool,
        mods: Mods,
        titles: &[&str],
    ) -> Option<MenuKey> {
        let alt_alone = std::mem::take(&mut self.alt_alone);
        match (key, pressed) {
            (MenuKeyInput::Alt, true) => {
                self.alt_alone = true;
                None
            }
            (MenuKeyInput::Alt, false) => alt_alone.then_some(MenuKey::FocusRow),
            (MenuKeyInput::F10, true) if !mods.alt && !mods.ctrl && !mods.shift => {
                Some(MenuKey::FocusRow)
            }
            (MenuKeyInput::Char(c), true) if mods.alt && !mods.ctrl => {
                let c = c.to_lowercase().next()?;
                titles
                    .iter()
                    .position(|title| {
                        title
                            .chars()
                            .next()
                            .and_then(|first| first.to_lowercase().next())
                            == Some(c)
                    })
                    .map(MenuKey::Open)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE: Layout = Layout {
        width: 800.0,
        height: 600.0,
        caption_start: 220.0,
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
    fn the_top_row_drags_after_the_menus_and_before_the_buttons() {
        assert_eq!(hit_test(100.0, 16.0, WIDE), Region::Client);
        assert_eq!(hit_test(220.0, 16.0, WIDE), Region::Caption);
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

    #[test]
    fn menus_running_into_the_buttons_leave_no_caption() {
        let narrow = Layout {
            caption_start: 700.0,
            ..WIDE
        };
        assert_eq!(hit_test(680.0, 16.0, narrow), Region::Client);
    }

    const TITLES: [&str; 4] = ["File", "Speakers", "View", "Help"];

    const ALT: Mods = Mods {
        alt: true,
        ctrl: false,
        shift: false,
    };

    fn press(keys: &mut MenuKeys, key: MenuKeyInput) -> Option<MenuKey> {
        let alt = key == MenuKeyInput::Alt;
        keys.key(
            key,
            true,
            Mods {
                alt,
                ..Mods::default()
            },
            &TITLES,
        )
    }

    fn release(keys: &mut MenuKeys, key: MenuKeyInput) -> Option<MenuKey> {
        keys.key(key, false, Mods::default(), &TITLES)
    }

    #[test]
    fn alt_alone_or_f10_focuses_the_menu_row() {
        let mut keys = MenuKeys::default();
        assert_eq!(press(&mut keys, MenuKeyInput::Alt), None);
        assert_eq!(
            release(&mut keys, MenuKeyInput::Alt),
            Some(MenuKey::FocusRow)
        );
        assert_eq!(press(&mut keys, MenuKeyInput::F10), Some(MenuKey::FocusRow));
        assert_eq!(release(&mut keys, MenuKeyInput::F10), None);
        assert_eq!(
            keys.key(
                MenuKeyInput::F10,
                true,
                Mods {
                    shift: true,
                    ..Mods::default()
                },
                &TITLES
            ),
            None,
            "Shift+F10 opens a context menu instead"
        );
    }

    #[test]
    fn alt_released_after_another_key_does_nothing() {
        let mut keys = MenuKeys::default();
        press(&mut keys, MenuKeyInput::Alt);
        press(&mut keys, MenuKeyInput::Other);
        assert_eq!(release(&mut keys, MenuKeyInput::Alt), None);
        press(&mut keys, MenuKeyInput::Alt);
        press(&mut keys, MenuKeyInput::Char('v'));
        assert_eq!(release(&mut keys, MenuKeyInput::Alt), None);
    }

    #[test]
    fn alt_released_after_the_focus_moved_does_nothing() {
        let mut keys = MenuKeys::default();
        press(&mut keys, MenuKeyInput::Alt);
        keys.focus_changed();
        assert_eq!(release(&mut keys, MenuKeyInput::Alt), None, "Alt+Tab");
    }

    #[test]
    fn alt_and_a_first_letter_opens_that_menu() {
        let mut keys = MenuKeys::default();
        press(&mut keys, MenuKeyInput::Alt);
        let alt = |keys: &mut MenuKeys, c| keys.key(MenuKeyInput::Char(c), true, ALT, &TITLES);
        assert_eq!(alt(&mut keys, 's'), Some(MenuKey::Open(1)));
        assert_eq!(alt(&mut keys, 'H'), Some(MenuKey::Open(3)));
        assert_eq!(alt(&mut keys, 'x'), None);
        let alt_gr = Mods { ctrl: true, ..ALT };
        assert_eq!(
            keys.key(MenuKeyInput::Char('f'), true, alt_gr, &TITLES),
            None,
            "AltGr types instead"
        );
        assert_eq!(
            press(&mut keys, MenuKeyInput::Char('s')),
            None,
            "s alone types"
        );
    }
}
