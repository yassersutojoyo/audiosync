//! The graph thread across restarts of the daemon and of the session
//! manager, ignored by default: the Linux CI job runs them in a daemon of
//! their own, with WirePlumber and the null sinks `as-a` and `as-b`, since
//! they stop and start both.

use std::process::Command;
use std::time::{Duration, Instant};

use super::graph::Graph;
use super::registry::Registry;
use crate::audio::backend::BackendError;

fn run(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Starts `program` in a shell that backgrounds it and exits, so it outlives
/// the test.
fn detached(program: &str) {
    Command::new("sh")
        .args(["-c", &format!("{program} >/dev/null 2>&1 &")])
        .status()
        .unwrap();
}

/// Whether a process named `program` runs; a zombie doesn't count, since the
/// CI step's shell reaps the daemons it started only when it ends.
fn running(program: &str) -> bool {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
        .any(|stat| {
            let Some((name, rest)) = stat
                .split_once(" (")
                .and_then(|(_, rest)| rest.rsplit_once(") "))
            else {
                return false;
            };
            name == program && !rest.starts_with('Z')
        })
}

/// Waits up to `seconds` for `done` to hold.
fn until(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

fn sink(name: &str) {
    run(
        "pw-cli",
        &[
            "create-node",
            "adapter",
            &format!(
                "{{ factory.name=support.null-audio-sink node.name={name} media.class=Audio/Sink object.linger=true audio.position=[FL FR] }}"
            ),
        ],
    );
}

fn set_default(registry: &Registry, name: &str) {
    let (id, _) = registry.graph().unwrap().named(name).unwrap();
    run("wpctl", &["set-default", &id.to_string()]);
}

fn default_sink(registry: &Registry) -> Option<String> {
    registry.graph().ok().and_then(|graph| graph.default_sink)
}

fn has(graph: Result<Graph, BackendError>, name: &str) -> bool {
    graph.is_ok_and(|graph| graph.named(name).is_some())
}

#[test]
#[ignore = "restarts the CI job's WirePlumber"]
fn the_defaults_are_followed_again_after_the_session_manager_restarts() {
    let registry = Registry::start().unwrap();
    assert!(until(5, || has(registry.graph(), "as-b")));
    run("pkill", &["-x", "wireplumber"]);
    assert!(until(5, || !running("wireplumber")));
    detached("wireplumber");
    std::thread::sleep(Duration::from_secs(2));
    for name in ["as-b", "as-a"] {
        set_default(&registry, name);
        assert!(
            until(5, || default_sink(&registry).as_deref() == Some(name)),
            "default {:?}, wanted {name}",
            default_sink(&registry)
        );
    }
}

#[test]
#[ignore = "restarts the CI job's PipeWire daemon"]
fn the_graph_comes_back_after_the_daemon_restarts() {
    let registry = Registry::start().unwrap();
    assert!(until(5, || has(registry.graph(), "as-a")));
    run("pkill", &["-x", "pipewire"]);
    assert!(until(5, || matches!(
        registry.graph(),
        Err(BackendError::ServiceNotRunning)
    )));
    detached("pipewire");
    std::thread::sleep(Duration::from_secs(1));
    if !running("wireplumber") {
        detached("wireplumber");
    }
    std::thread::sleep(Duration::from_secs(2));
    sink("as-a");
    assert!(
        until(10, || has(registry.graph(), "as-a")),
        "graph after the restart: {:?}",
        registry.graph().map(|graph| graph.default_sink)
    );
    assert!(registry.take_outputs_changed());
}
