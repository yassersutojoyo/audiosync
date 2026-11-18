//! Opens and closes each window but the main window and the tray flyout ten
//! times on the GPU renderer, with the main window shown as in use, and checks
//! that private memory ends within 15 MB of where it began for each. It needs
//! a GPU and a desktop session, which the CI machines lack, so it runs by hand:
//! `cargo test --release window_memory -- --ignored --nocapture`.

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};

use super::{App, Engine};
use crate::debug_log::DebugLog;
use crate::ui_model::MockBackend;

const WINDOWS: [&str; 10] = [
    "equalizer",
    "zone dialog",
    "PIN dialog",
    "password dialog",
    "about",
    "guide",
    "help",
    "remote",
    "setup",
    "settings",
];
const CYCLES: usize = 10;
const ALLOWED_GROWTH_MB: f64 = 15.0;

fn private_mb() -> f64 {
    let script = format!(
        "(Get-Process -Id {}).PrivateMemorySize64",
        std::process::id()
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("powershell runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1_048_576.0
}

/// Opens window `index` of [`WINDOWS`] as the app does.
fn open(app: &App, index: usize) {
    let shown = |result: Result<(), slint::PlatformError>| result.expect("the window shows");
    match index {
        0 => app.show_equalizer(),
        1 => shown(app.zone_dialog().show()),
        2 => shown(app.pin_dialog().show()),
        3 => shown(app.password_dialog().show()),
        4 => shown(app.about().show()),
        5 => shown(app.guide().show()),
        6 => app.show_help(),
        7 => app.show_remote(),
        8 => shown(app.setup().show()),
        _ => app.show_settings(),
    }
}

/// Runs `f` on the native window of window `index` of [`WINDOWS`].
fn with_window(app: &App, index: usize, f: impl FnOnce(&slint::Window)) {
    match index {
        0 => f(app.equalizer().window()),
        1 => f(app.zone_dialog().window()),
        2 => f(app.pin_dialog().window()),
        3 => f(app.password_dialog().window()),
        4 => f(app.about().window()),
        5 => f(app.guide().window()),
        6 => f(app.help().window()),
        7 => f(app.remote_window().window()),
        8 => f(app.setup().window()),
        _ => f(app.settings().window()),
    }
}

#[derive(Clone, Copy)]
enum Step {
    Open(usize),
    Close(usize),
    Wait,
    Measure(Option<usize>),
    Quit,
}

#[test]
#[ignore = "opens real windows on the GPU: run alone with --ignored on a desktop"]
fn window_memory_stays_flat_over_ten_opens_of_each_window() {
    slint::BackendSelector::new()
        .backend_name("winit".to_owned())
        .renderer_name("skia".to_owned())
        .select()
        .expect("the Skia backend");
    let app = App::new(
        Engine::here(Box::new(MockBackend::new())),
        DebugLog::new(None),
    )
    .expect("the windows build");
    app.main
        .window()
        .set_position(slint::PhysicalPosition::new(-20_000, -20_000));
    app.main.window().show().expect("the main window shows");

    // Settling time before each reading: the GPU memory of a dropped window
    // comes back some frames after the drop.
    let settle = [Step::Wait; 16];
    let mut steps = Vec::new();
    for index in 0..WINDOWS.len() {
        steps.extend(settle);
        steps.push(Step::Measure(None));
        for _ in 0..CYCLES {
            steps.extend([
                Step::Open(index),
                Step::Close(index),
                Step::Wait,
                Step::Wait,
            ]);
        }
        steps.extend(settle);
        steps.push(Step::Measure(Some(index)));
    }
    steps.push(Step::Quit);

    let opened_at: Rc<Cell<Option<Instant>>> = Rc::default();
    let open_ms: Rc<RefCell<Vec<Vec<f64>>>> = Rc::new(RefCell::new(vec![vec![]; WINDOWS.len()]));
    let call_ms: Rc<RefCell<Vec<Vec<f64>>>> = Rc::new(RefCell::new(vec![vec![]; WINDOWS.len()]));
    let results: Rc<RefCell<Vec<(usize, f64, f64)>>> = Rc::default();
    let start_mb = Cell::new(0.0);
    let next = Cell::new(0);
    let timer = Timer::default();
    let (app_t, opened_t, open_ms_t, call_ms_t, results_t) = (
        app.clone(),
        opened_at.clone(),
        open_ms.clone(),
        call_ms.clone(),
        results.clone(),
    );
    timer.start(TimerMode::Repeated, Duration::from_millis(400), move || {
        let Some(step) = steps.get(next.get()).copied() else {
            return;
        };
        next.set(next.get() + 1);
        match step {
            Step::Open(index) => {
                let at = Instant::now();
                opened_t.set(Some(at));
                open(&app_t, index);
                call_ms_t.borrow_mut()[index].push(at.elapsed().as_secs_f64() * 1000.0);
                let (opened, times) = (opened_t.clone(), open_ms_t.clone());
                with_window(&app_t, index, |window| {
                    let _ = window.set_rendering_notifier(move |state, _| {
                        if matches!(state, slint::RenderingState::AfterRendering)
                            && let Some(at) = opened.take()
                        {
                            times.borrow_mut()[index].push(at.elapsed().as_secs_f64() * 1000.0);
                        }
                    });
                    window.set_position(slint::PhysicalPosition::new(-20_000, -20_000));
                });
            }
            Step::Close(index) => {
                with_window(&app_t, index, |window| {
                    window.hide().expect("the window hides")
                });
            }
            Step::Wait => {}
            Step::Measure(None) => start_mb.set(private_mb()),
            Step::Measure(Some(index)) => {
                results_t
                    .borrow_mut()
                    .push((index, start_mb.get(), private_mb()));
            }
            Step::Quit => {
                let _ = slint::quit_event_loop();
            }
        }
    });
    slint::run_event_loop_until_quit().expect("the event loop runs");

    let mut grown = Vec::new();
    for &(index, start, end) in results.borrow().iter() {
        let mut times = open_ms.borrow()[index].clone();
        times.sort_by(f64::total_cmp);
        let median = times.get(times.len() / 2).copied().unwrap_or(f64::NAN);
        let slowest = times.last().copied().unwrap_or(f64::NAN);
        let mut calls = call_ms.borrow()[index].clone();
        calls.sort_by(f64::total_cmp);
        let call = calls.get(calls.len() / 2).copied().unwrap_or(f64::NAN);
        let call_slowest = calls.last().copied().unwrap_or(f64::NAN);
        println!(
            "{}: {start:.1} -> {end:.1} MB ({:+.1}), open call median {call:.0} ms, slowest {call_slowest:.0} ms, open to first frame median {median:.0} ms, slowest {slowest:.0} ms",
            WINDOWS[index],
            end - start
        );
        if end - start > ALLOWED_GROWTH_MB {
            grown.push(WINDOWS[index]);
        }
    }
    assert!(
        grown.is_empty(),
        "these windows keep memory after closing: {grown:?}"
    );
}
