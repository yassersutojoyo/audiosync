//! Linux: the tray icon where a StatusNotifier host draws it, and the
//! window-only mode everywhere else, where closing the main window quits.
//! tray-icon (over ksni) doesn't say when an icon can't show, so the
//! session bus is asked first, on a worker thread with a time budget. The
//! same thread then follows the watcher: a shown icon whose watcher stays
//! gone hands over to the main window, and a host that turns up after
//! launch is logged (the icon shows after a restart).

// tray.rs is built on tray-icon, a Windows and Linux dependency only.
#[path = "../tray.rs"]
mod tray;

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::blocking::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::{BusName, InterfaceName};

use super::tray_watch::{LossWatch, Watcher};
use crate::debug_log::DebugLog;
use crate::view::App;

/// The watcher's name as ksni registers with it.
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const HOST_PROPERTY: &str = "IsStatusNotifierHostRegistered";

/// How long the launch waits for the session bus to answer.
const PROBE_BUDGET: Duration = Duration::from_secs(1);

/// How often a shown icon's watcher is looked at again.
const POLL_EVERY: Duration = Duration::from_secs(2);

pub type Tray = Rc<tray::Tray>;

thread_local! {
    /// The app and its log, for what the watcher thread reports.
    static TARGET: RefCell<Option<(Weak<App>, DebugLog)>> = const { RefCell::new(None) };
}

/// What the watcher thread reports to the UI thread.
enum Report {
    /// The watcher of a shown icon stayed gone for the grace period.
    Lost,
    /// A host registered while the app runs without an icon.
    HostAppeared,
}

/// Adds the tray icon for `app` where a host shows it; `None` otherwise,
/// so closing the main window quits.
pub fn create_tray(app: &Rc<App>, log: &DebugLog) -> Option<Tray> {
    TARGET.with(|cell| *cell.borrow_mut() = Some((Rc::downgrade(app), log.clone())));
    let (answer, answered) = mpsc::channel();
    let (mode, told) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("tray watch".to_owned())
        .spawn(move || watch(&answer, &told));
    let probed = match spawned {
        Ok(_) => answered
            .recv_timeout(PROBE_BUDGET)
            .unwrap_or_else(|_| Err("the session bus didn't answer within 1 s".to_owned())),
        Err(error) => Err(format!("no thread to ask the session bus ({error})")),
    };
    let tray = match probed {
        Ok(Watcher::Ready) => tray::Tray::create(app).map_err(|error| error.to_string()),
        Ok(watcher) => Err(watcher.missing_reason().unwrap_or_default().to_owned()),
        Err(reason) => Err(reason),
    };
    let _ = mode.send(tray.is_ok());
    match tray {
        Ok(tray) => {
            log.log(|| "tray icon: shown".to_owned());
            app.set_tray_available(true);
            Some(tray)
        }
        Err(reason) => {
            log.log(|| format!("tray icon: not shown ({reason}); closing the main window quits"));
            None
        }
    }
}

/// The watcher thread: answers the launch's question, then follows the
/// watcher in the mode the launch settled on.
fn watch(answer: &mpsc::Sender<Result<Watcher, String>>, told: &mpsc::Receiver<bool>) {
    let connection = match Connection::session() {
        Ok(connection) => connection,
        Err(error) => {
            let _ = answer.send(Err(format!("no session bus ({error})")));
            return;
        }
    };
    let first = ask(&connection).map_err(|error| format!("the session bus: {error}"));
    if answer.send(first).is_err() {
        return;
    }
    match told.recv() {
        Ok(true) => follow_shown(&connection),
        Ok(false) => wait_for_host(&connection),
        Err(_) => {}
    }
}

/// Whether the watcher runs, and whether it has a host.
fn ask(connection: &Connection) -> zbus::Result<Watcher> {
    let name = BusName::try_from(WATCHER_NAME)?;
    if !DBusProxy::new(connection)?.name_has_owner(name)? {
        return Ok(Watcher::Missing);
    }
    let properties = PropertiesProxy::builder(connection)
        .destination(WATCHER_NAME)?
        .path(WATCHER_PATH)?
        .build()?;
    let registered = properties.get(InterfaceName::try_from(WATCHER_NAME)?, HOST_PROPERTY)?;
    Ok(if bool::try_from(registered)? {
        Watcher::Ready
    } else {
        Watcher::NoHost
    })
}

fn follow_shown(connection: &Connection) {
    let mut loss = LossWatch::default();
    loop {
        thread::sleep(POLL_EVERY);
        let watcher = ask(connection).unwrap_or(Watcher::Missing);
        if loss.observe(watcher, Instant::now()) {
            report(Report::Lost);
            return;
        }
    }
}

/// Blocks on the watcher's host signal, which costs nothing while it
/// never comes; the bus delivers it from whoever owns the name by then.
fn wait_for_host(connection: &Connection) {
    let proxy = zbus::blocking::proxy::Builder::<zbus::blocking::Proxy<'_>>::new(connection)
        .destination(WATCHER_NAME)
        .and_then(|builder| builder.path(WATCHER_PATH))
        .and_then(|builder| builder.interface(WATCHER_NAME))
        .map(|builder| builder.cache_properties(zbus::proxy::CacheProperties::No))
        .and_then(|builder| builder.build());
    let Ok(proxy) = proxy else {
        return;
    };
    if let Ok(mut signals) = proxy.receive_signal("StatusNotifierHostRegistered")
        && signals.next().is_some()
    {
        report(Report::HostAppeared);
    }
}

fn report(report: Report) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some((app, log)) = TARGET.with(|cell| cell.borrow().clone()) else {
            return;
        };
        match report {
            Report::Lost => {
                log.log(|| {
                    format!(
                        "tray icon: the StatusNotifier watcher is gone for {} s; the main window shows, and closing it quits",
                        super::tray_watch::LOSS_GRACE.as_secs()
                    )
                });
                if let Some(app) = app.upgrade() {
                    app.set_tray_available(false);
                    app.hide_flyout();
                    app.show_main();
                }
            }
            Report::HostAppeared => log.log(|| {
                "tray icon: a StatusNotifier host registered after launch; the icon shows after a restart"
                    .to_owned()
            }),
        }
    });
}
