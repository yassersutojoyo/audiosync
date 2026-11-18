//! Measures what a Skia window keeps in private memory after it hides: the
//! main window shows the whole run, a second window opens and closes ten times,
//! then the main window hides and shows ten times.
//!
//! From `desktop/`: `cargo run --example window_memory_probe`, then again
//! with `-- --drop-closed`, which drops the second window after each close,
//! or `-- --renderer=skia-software`. It also prints the CPU time of ten
//! seconds of the main window redrawing whole at 60 frames a second. The windows open off every
//! monitor; the run takes about a minute and prints one line per step.

use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

slint::slint! {
    export component Probe inherits Window {
        width: 900px;
        height: 700px;
        in property <string> tag;
        in property <float> phase;
        VerticalLayout {
            for row in 40: Rectangle {
                height: 16px;
                background: (row.mod(2) == 0 ? #20304a : #304a20).brighter(Math.sin((phase + row * 9) * 1deg) * 0.5 + 0.5);
                x: Math.cos((phase + row * 9) * 1deg) * 20px;
                drop-shadow-blur: 6px;
                drop-shadow-color: #0008;
                Text {
                    text: tag + " row " + row + " The quick brown fox jumps over the lazy dog 0123456789";
                    font-size: 12px + row.mod(5) * 1px;
                    color: white;
                }
            }
        }
    }
}

fn private_mb() -> f64 {
    process_value("PrivateMemorySize64") / 1_048_576.0
}

fn process_value(field: &str) -> f64 {
    let script = format!("(Get-Process -Id {}).{field}", std::process::id());
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("powershell runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0)
}

#[derive(Clone, Copy)]
enum Step {
    Measure(&'static str),
    Show(usize),
    Hide(usize),
    Drop(usize),
    Animate,
    Cpu,
    Quit,
}

fn main() {
    slint::BackendSelector::new()
        .backend_name("winit".to_owned())
        .renderer_name(
            std::env::args()
                .find_map(|arg| arg.strip_prefix("--renderer=").map(str::to_owned))
                .unwrap_or_else(|| "skia".to_owned()),
        )
        .select()
        .expect("the Skia backend");
    let drop_closed = std::env::args().any(|arg| arg == "--drop-closed");
    let mut windows = [Some(Probe::new().unwrap()), None];
    let mut steps = vec![Step::Show(0), Step::Measure("main shown"), Step::Cpu];
    steps.extend([Step::Animate; 25]);
    steps.push(Step::Cpu);
    for _ in 0..10 {
        steps.extend([Step::Show(1), Step::Hide(1)]);
        if drop_closed {
            steps.push(Step::Drop(1));
        }
        steps.push(Step::Measure("second window open+close"));
    }
    for _ in 0..10 {
        steps.extend([Step::Hide(0), Step::Show(0)]);
        steps.push(Step::Measure("main hide+show"));
    }
    steps.push(Step::Quit);
    let mut next = 0;
    let mut cpu_from = 0.0;
    let animation = Rc::new(slint::Timer::default());
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(400),
        move || {
            let Some(step) = steps.get(next).copied() else {
                return;
            };
            next += 1;
            match step {
                Step::Measure(label) => println!("{label}: {:.1} MB", private_mb()),
                Step::Show(index) => {
                    let probe = windows[index].get_or_insert_with(|| Probe::new().unwrap());
                    probe.set_tag(if index == 0 { "main" } else { "settings" }.into());
                    let window = probe.window();
                    window.set_position(slint::PhysicalPosition::new(-20_000, -20_000));
                    window.show().unwrap();
                }
                Step::Hide(index) => {
                    if let Some(probe) = &windows[index] {
                        probe.window().hide().unwrap();
                    }
                }
                Step::Drop(index) => windows[index] = None,
                Step::Animate => {
                    if !animation.running() {
                        let main = windows[0].as_ref().unwrap().as_weak();
                        animation.start(
                            slint::TimerMode::Repeated,
                            Duration::from_millis(16),
                            move || {
                                if let Some(main) = main.upgrade() {
                                    main.set_phase(main.get_phase() + 3.0);
                                }
                            },
                        );
                    }
                }
                Step::Cpu => {
                    let seconds = process_value("TotalProcessorTime.TotalSeconds");
                    if animation.running() {
                        animation.stop();
                        println!(
                            "CPU over 10 s of full-window animation: {:.2} s",
                            seconds - cpu_from
                        );
                    }
                    cpu_from = seconds;
                }
                Step::Quit => {
                    let _ = slint::quit_event_loop();
                }
            }
        },
    );
    slint::run_event_loop_until_quit().unwrap();
}
