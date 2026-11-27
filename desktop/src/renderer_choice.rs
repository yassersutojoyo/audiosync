//! The Slint renderer a Windows start uses: Skia, whose DirectWrite text is
//! sharper than FemtoVG's, unless three starts with Skia in a row ended
//! within a second of showing the main window, or failed to show it; from
//! then on the software renderer, so the window still opens on a GPU or
//! driver that Skia fails on.

/// The file in the data folder that records the renderer of the last start.
pub const RECORD_NAME: &str = "renderer.txt";

/// Starts with Skia in a row that failed before the software renderer takes
/// over: one crash, kill or render error in the first second proves little.
const STARTS_WITHOUT_A_FRAME: u32 = 3;

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
/// start with Skia drew a frame or quit cleanly, `skia <n>` after n starts in
/// a row that did neither.
pub fn for_start(record: Option<&str>) -> Renderer {
    match record.map(str::trim) {
        Some("software") => Renderer::Software,
        Some(held) if starts_without_a_frame(held) >= STARTS_WITHOUT_A_FRAME => Renderer::Software,
        _ => Renderer::Skia,
    }
}

/// The starts in a row a record counts; earlier builds wrote a bare `skia`
/// after one.
fn starts_without_a_frame(held: &str) -> u32 {
    match held.trim().strip_prefix("skia").map(str::trim) {
        Some("") => 1,
        Some(count) => count.parse().unwrap_or(0),
        None => 0,
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
/// before [`confirm`] counts toward the software renderer.
pub fn arm(record: &Path) -> io::Result<()> {
    if let Some(folder) = record.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let held = std::fs::read_to_string(record).unwrap_or_default();
    let starts = starts_without_a_frame(&held) + 1;
    std::fs::write(record, format!("{} {starts}", Renderer::Skia.name()))
}

/// Puts every start from the next on the software renderer, as after a GPU
/// that keeps losing the window's surface.
pub fn fall_back(folder: Option<&Path>) -> io::Result<()> {
    let Some(folder) = folder else {
        return Ok(());
    };
    std::fs::create_dir_all(folder)?;
    std::fs::write(folder.join(RECORD_NAME), Renderer::Software.name())
}

/// Picks this start's renderer and selects it for Slint; the record's path
/// when it is Skia.
#[cfg(windows)]
pub fn select(log: &crate::debug_log::DebugLog) -> Option<PathBuf> {
    let (renderer, record) = choose(crate::data_folder::folder().as_deref());
    log.log(|| match renderer {
        Renderer::Skia => "renderer: Skia".to_owned(),
        Renderer::Software => {
            "renderer: software (Skia failed three times in a row, at start or by losing the window's surface)"
                .to_owned()
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

/// Clears the record when the event loop still runs a second after the main
/// window has shown, which it reaches only past the window's first frame.
/// Slint 1.18's Skia surfaces on Windows never call a rendering notifier.
#[cfg(any(windows, test))]
pub fn confirm_when_drawn(record: PathBuf, log: crate::debug_log::DebugLog) {
    slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
        confirm(&record, &log)
    });
}

/// A start with Skia drew or quit cleanly, so the count starts again.
pub fn confirm(record: &Path, log: &crate::debug_log::DebugLog) {
    match std::fs::remove_file(record) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            log.log(|| format!("renderer: record not cleared ({error})"));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{Renderer, arm, choose, confirm, fall_back, for_start};

    #[test]
    fn skia_until_three_starts_in_a_row_neither_draw_nor_quit_then_software() {
        assert_eq!(for_start(None), Renderer::Skia);
        assert_eq!(for_start(Some("skia 1")), Renderer::Skia);
        assert_eq!(for_start(Some("skia 2")), Renderer::Skia);
        assert_eq!(for_start(Some("skia 3")), Renderer::Software);
        assert_eq!(
            for_start(Some(
                "skia 3
"
            )),
            Renderer::Software
        );
        assert_eq!(for_start(Some("software")), Renderer::Software);
        assert_eq!(for_start(Some("")), Renderer::Skia);
    }

    // Earlier builds wrote a bare "skia" after one start that never drew, as one on a sleeping display.
    #[test]
    fn the_record_of_an_earlier_build_counts_as_one_start_that_never_drew() {
        assert_eq!(for_start(Some("skia")), Renderer::Skia);
        let folder = empty_folder("earlier-build");
        std::fs::create_dir_all(&folder).expect("the folder is made");
        std::fs::write(folder.join(super::RECORD_NAME), "skia").expect("the record is written");
        let (renderer, record) = choose(Some(&folder));
        assert_eq!(renderer, Renderer::Skia);
        arm(&record.expect("a Skia start has a record")).expect("the record is written");
        assert_eq!(
            std::fs::read_to_string(folder.join(super::RECORD_NAME))
                .ok()
                .as_deref(),
            Some("skia 2")
        );
        let _ = std::fs::remove_dir_all(&folder);
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

    // One start that ends early, killed or crashed, is no proof that Skia fails.
    #[test]
    fn starts_that_never_draw_fall_back_only_on_the_third_in_a_row_and_stay() {
        let folder = empty_folder("no-frame");
        for _ in 0..3 {
            let (renderer, record) = choose(Some(&folder));
            assert_eq!(renderer, Renderer::Skia);
            arm(&record.expect("a Skia start has a record")).expect("the record is written");
        }
        assert_eq!(choose(Some(&folder)), (Renderer::Software, None));
        assert_eq!(choose(Some(&folder)).0, Renderer::Software);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_start_that_draws_keeps_the_next_on_skia_and_clears_the_count() {
        let folder = empty_folder("drew");
        for _ in 0..2 {
            arm(&choose(Some(&folder)).1.expect("a Skia start has a record"))
                .expect("the record is written");
        }
        let record = choose(Some(&folder)).1.expect("a Skia start has a record");
        arm(&record).expect("the record is written");
        confirm(&record, &crate::debug_log::DebugLog::new(None));
        for _ in 0..2 {
            arm(&choose(Some(&folder)).1.expect("a Skia start has a record"))
                .expect("the record is written");
        }
        assert_eq!(choose(Some(&folder)).0, Renderer::Skia);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_fall_back_keeps_every_next_start_on_software() {
        let folder = empty_folder("fall-back");
        fall_back(Some(&folder)).expect("the record is written");
        assert_eq!(choose(Some(&folder)), (Renderer::Software, None));
        assert_eq!(
            std::fs::read_to_string(folder.join(super::RECORD_NAME))
                .ok()
                .as_deref(),
            Some("software")
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    thread_local! {
        static NOW: std::cell::Cell<std::time::Duration> = const { std::cell::Cell::new(std::time::Duration::ZERO) };
    }

    struct Clock;

    impl slint::platform::Platform for Clock {
        fn create_window_adapter(
            &self,
        ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Err(slint::PlatformError::NoPlatform)
        }

        fn duration_since_start(&self) -> std::time::Duration {
            NOW.with(std::cell::Cell::get)
        }
    }

    // Slint's Skia surfaces on Windows never call a rendering notifier, so a Skia start that drew left its record behind.
    #[test]
    fn a_start_whose_event_loop_still_runs_a_second_after_the_window_shows_clears_the_record() {
        let _ = slint::platform::set_platform(Box::new(Clock));
        let folder = empty_folder("still-running");
        let record = choose(Some(&folder)).1.expect("a Skia start has a record");
        arm(&record).expect("the record is written");
        super::confirm_when_drawn(record.clone(), crate::debug_log::DebugLog::new(None));
        NOW.with(|now| now.set(now.get() + std::time::Duration::from_millis(900)));
        slint::platform::update_timers_and_animations();
        assert!(record.exists());
        NOW.with(|now| now.set(now.get() + std::time::Duration::from_millis(200)));
        slint::platform::update_timers_and_animations();
        assert!(!record.exists());
        let _ = std::fs::remove_dir_all(&folder);
    }
}
