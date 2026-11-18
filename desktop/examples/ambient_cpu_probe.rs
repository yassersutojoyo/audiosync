//! Measures the CPU time and frame gaps of ten seconds of the Now Playing art
//! (`ui/ambient.slint`, driven by `src/ambient.rs` as live sound drives it)
//! over a window full of text, at a given renderer and physical size.
//!
//! From `desktop/`: `cargo run --example ambient_cpu_probe -- --renderer=skia
//! --size=1800x1300`, with `SLINT_SCALE_FACTOR` set for the scale to measure;
//! `--art-fill` makes the art fill the window instead of its 22 px strip. The
//! window opens off every monitor for about twelve seconds.

#[allow(dead_code)]
#[path = "../src/ambient.rs"]
mod ambient;
#[allow(dead_code)]
#[path = "../src/window_rules.rs"]
mod window_rules;

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::slint! {
    import { AmbientArt, AmbientLook } from "ui/ambient.slint";

    export component Probe inherits Window {
        in property <AmbientLook> look;
        in property <bool> art-fill;
        background: #1c1c1e;
        VerticalLayout {
            if root.art-fill: AmbientArt {
                look: root.look;
            }
            if !root.art-fill: AmbientArt {
                height: 22px;
                look: root.look;
            }
            for row in root.art-fill ? 0 : 60: Rectangle {
                height: 20px;
                background: row.mod(2) == 0 ? #20304a : #304a20;
                drop-shadow-blur: 6px;
                drop-shadow-color: #0008;
                Text {
                    text: "row " + row + " The quick brown fox jumps over the lazy dog 0123456789";
                    font-size: 13px;
                    color: white;
                }
            }
        }
    }
}

fn cpu_seconds() -> f64 {
    let script = format!(
        "(Get-Process -Id {}).TotalProcessorTime.TotalSeconds",
        std::process::id()
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("powershell runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0)
}

fn arg(name: &str) -> Option<String> {
    std::env::args().find_map(|arg| arg.strip_prefix(name).map(str::to_owned))
}

fn look(driver: &ambient::Driver) -> AmbientLook {
    let cover = [[0.8, 0.3, 0.2], [0.2, 0.5, 0.8], [0.3, 0.7, 0.3]];
    let color = |[r, g, b]: [f64; 3]| slint::Color::from_rgb_f32(r as f32, g as f32, b as f32);
    let strands: Vec<AmbientStrand> = driver
        .strands(&cover, ambient::WINDOW_DENSITY, ambient::WINDOW_SLOT, false)
        .into_iter()
        .map(|strand| AmbientStrand {
            commands: strand.commands.into(),
            breath: strand.breath as f32,
            sound: strand.sound as f32,
            tone_light: color(strand.tone),
            tone_dark: color(strand.tone),
            width: strand.width as f32,
        })
        .collect();
    AmbientLook {
        strands: slint::ModelRc::new(slint::VecModel::from(strands)),
        line_light: color([0.2; 3]),
        line_dark: color([0.9; 3]),
    }
}

fn main() {
    let renderer = arg("--renderer=").unwrap_or_else(|| "skia".to_owned());
    slint::BackendSelector::new()
        .backend_name("winit".to_owned())
        .renderer_name(renderer.clone())
        .select()
        .expect("the renderer");
    let (width, height) = arg("--size=")
        .and_then(|size| {
            let (w, h) = size.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1800, 1300));
    let probe = Probe::new().unwrap();
    probe.set_art_fill(std::env::args().any(|arg| arg == "--art-fill"));
    let window = probe.window();
    window.set_position(slint::PhysicalPosition::new(-20_000, -20_000));
    window.set_size(slint::PhysicalSize::new(width, height));
    window.show().unwrap();

    let frames: Rc<RefCell<Vec<Duration>>> = Rc::default();
    let last_frame = Rc::new(Cell::new(None::<Instant>));
    let (f, l) = (frames.clone(), last_frame.clone());
    let notified = window.set_rendering_notifier(move |state, _| {
        if matches!(state, slint::RenderingState::AfterRendering) {
            let now = Instant::now();
            if let Some(before) = l.replace(Some(now)) {
                f.borrow_mut().push(now - before);
            }
        }
    });

    let driver = Rc::new(RefCell::new(ambient::Driver::default()));
    let started = Instant::now();
    let tick_at = Rc::new(Cell::new(started));
    let ticks: Rc<RefCell<Vec<Duration>>> = Rc::default();
    let cpu_from = Rc::new(Cell::new(None::<f64>));
    let timer = slint::Timer::default();
    let weak = probe.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs_f64(1.0 / 60.0),
        move || {
            let Some(probe) = weak.upgrade() else {
                return;
            };
            let now = Instant::now();
            let dt = now.duration_since(tick_at.replace(now));
            let elapsed = now.duration_since(started).as_secs_f64();
            let wobble = (elapsed * 3.0).sin().abs() as f32;
            driver
                .borrow_mut()
                .advance(dt.as_secs_f64(), || Some([0.8 * wobble, 0.6, 0.4]));
            probe.set_look(look(&driver.borrow()));
            if elapsed < 2.0 {
                return;
            }
            let Some(from) = cpu_from.get() else {
                frames.borrow_mut().clear();
                cpu_from.set(Some(cpu_seconds()));
                return;
            };
            ticks.borrow_mut().push(dt);
            if elapsed < 12.0 {
                return;
            }
            let cpu = cpu_seconds() - from;
            let stats = |gaps: &[Duration]| {
                let mut ms: Vec<f64> = gaps.iter().map(|g| g.as_secs_f64() * 1000.0).collect();
                ms.sort_by(f64::total_cmp);
                let at = |q: f64| ms.get(((ms.len() as f64 - 1.0) * q) as usize).copied();
                format!(
                    "{} gaps, p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms, over 20 ms {}",
                    ms.len(),
                    at(0.5).unwrap_or(0.0),
                    at(0.99).unwrap_or(0.0),
                    ms.last().copied().unwrap_or(0.0),
                    ms.iter().filter(|g| **g > 20.0).count()
                )
            };
            println!(
                "{renderer} {width}x{height} scale {}: CPU {cpu:.2} s over 10 s ({:.0}% of a core); ticks: {}; frames: {}",
                probe.window().scale_factor(),
                cpu * 10.0,
                stats(&ticks.borrow()),
                if notified.is_ok() {
                    stats(&frames.borrow())
                } else {
                    "no rendering notifier".to_owned()
                }
            );
            let _ = slint::quit_event_loop();
        },
    );
    slint::run_event_loop_until_quit().unwrap();
}
