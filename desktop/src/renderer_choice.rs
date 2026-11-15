//! The Slint renderer a Windows start uses: Skia, whose DirectWrite text is
//! sharper than FemtoVG's, unless a start with Skia ended before it drew a
//! frame or quit cleanly; from then on the software renderer, so the window
//! still opens on a GPU or driver that Skia fails on.

/// The file in the data folder that records the renderer of the last start.
pub const RECORD_NAME: &str = "renderer.txt";

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    Skia,
    Software,
}

impl Renderer {
    /// The name Slint's backend selector takes, also what the record holds.
    pub fn name(self) -> &'static str {
        match self {
            Renderer::Skia => "skia",
            Renderer::Software => "software",
        }
    }
}

/// The renderer for this start from what the record holds: none after a
/// start with Skia drew a frame or quit cleanly, `skia` when it did neither.
pub fn for_start(record: Option<&str>) -> Renderer {
    match record.map(str::trim) {
        Some("skia" | "software") => Renderer::Software,
        _ => Renderer::Skia,
    }
}

/// This start's renderer from the record in `folder`; the record's path
/// when it is Skia, for [`arm`] and [`confirm`]. Writes nothing: only a
/// window about to draw arms the record.
pub fn choose(folder: Option<&Path>) -> (Renderer, Option<PathBuf>) {
    let record = folder.map(|folder| folder.join(RECORD_NAME));
    let held = record
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());
    let renderer = for_start(held.as_deref());
    (renderer, record.filter(|_| renderer == Renderer::Skia))
}

/// Records that a window is about to draw with Skia: a start that ends
/// before [`confirm`] makes the next one use the software renderer.
pub fn arm(record: &Path) -> io::Result<()> {
    if let Some(folder) = record.parent() {
        std::fs::create_dir_all(folder)?;
    }
    std::fs::write(record, Renderer::Skia.name())
}

/// Picks this start's renderer and selects it for Slint; the record's path
/// when it is Skia.
#[cfg(windows)]
pub fn select(log: &crate::debug_log::DebugLog) -> Option<PathBuf> {
    let (renderer, record) = choose(crate::data_folder::folder().as_deref());
    log.log(|| match renderer {
        Renderer::Skia => "renderer: Skia".to_owned(),
        Renderer::Software => {
            "renderer: software (a start with Skia neither drew nor quit)".to_owned()
        }
    });
    if let Err(error) = slint::BackendSelector::new()
        .backend_name("winit".to_owned())
        .renderer_name(renderer.name().to_owned())
        .select()
    {
        log.log(|| format!("renderer: not selected ({error})"));
    }
    record
}

/// Clears the record once `window`'s first frame is drawn.
#[cfg(windows)]
pub fn confirm_on_first_frame(
    window: &slint::Window,
    record: PathBuf,
    log: &crate::debug_log::DebugLog,
) {
    let mut record = Some(record);
    let set = window.set_rendering_notifier(move |state, _| {
        if matches!(state, slint::RenderingState::AfterRendering)
            && let Some(path) = record.take()
        {
            confirm(&path);
        }
    });
    if let Err(error) = set {
        log.log(|| format!("renderer: no first-frame check ({error:?})"));
    }
}

/// A start with Skia drew or quit cleanly, so the next one uses Skia too.
pub fn confirm(record: &Path) {
    let _ = std::fs::remove_file(record);
}

#[cfg(test)]
mod tests {
    use super::{Renderer, arm, choose, confirm, for_start};

    #[test]
    fn skia_until_a_start_with_it_neither_draws_nor_quits_then_software_for_good() {
        assert_eq!(for_start(None), Renderer::Skia);
        assert_eq!(for_start(Some("skia")), Renderer::Software);
        assert_eq!(for_start(Some("software")), Renderer::Software);
        assert_eq!(for_start(Some("skia\r\n")), Renderer::Software);
        assert_eq!(for_start(Some("")), Renderer::Skia);
    }

    fn empty_folder(name: &str) -> std::path::PathBuf {
        let folder =
            std::env::temp_dir().join(format!("audiosync-renderer-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        folder
    }

    // A Launch at Sign-In start ended before its window opened, or the audio failed to start.
    #[test]
    fn a_start_that_ends_before_a_window_shows_leaves_the_next_on_skia() {
        let folder = empty_folder("no-window");
        let (renderer, record) = choose(Some(&folder));
        assert_eq!(renderer, Renderer::Skia);
        assert!(record.is_some());
        assert_eq!(choose(Some(&folder)).0, Renderer::Skia);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_start_that_ends_after_a_window_shows_but_before_it_draws_falls_back() {
        let folder = empty_folder("no-frame");
        let record = choose(Some(&folder)).1.expect("a Skia start has a record");
        arm(&record).expect("the record is written");
        assert_eq!(choose(Some(&folder)).0, Renderer::Software);
        assert_eq!(choose(Some(&folder)).0, Renderer::Software);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_start_that_draws_keeps_the_next_on_skia() {
        let folder = empty_folder("drew");
        let record = choose(Some(&folder)).1.expect("a Skia start has a record");
        arm(&record).expect("the record is written");
        confirm(&record);
        assert_eq!(choose(Some(&folder)).0, Renderer::Skia);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
