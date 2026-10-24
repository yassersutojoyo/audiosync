//! A stand-in StatusNotifier watcher for the Linux tray smoke test
//! (Tests/linux-tray-smoke.sh): it owns `org.kde.StatusNotifierWatcher` on
//! the session bus, says a host is registered, and prints `registered
//! <service>` for every item that registers. Test tooling only; it never
//! ships.
//!
//!     cargo run --example sni_watcher_stub

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match stub::serve() {
        Ok(never) => match never {},
        Err(error) => {
            eprintln!("sni_watcher_stub: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sni_watcher_stub: Linux only");
}

#[cfg(target_os = "linux")]
mod stub {
    use std::convert::Infallible;

    use zbus::message::Header;

    const NAME: &str = "org.kde.StatusNotifierWatcher";
    const PATH: &str = "/StatusNotifierWatcher";

    struct Watcher {
        items: Vec<String>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl Watcher {
        fn register_status_notifier_item(
            &mut self,
            service: String,
            #[zbus(header)] header: Header<'_>,
        ) {
            // An item may register by object path; its bus name is then the sender.
            let service = match header.sender() {
                Some(sender) if service.starts_with('/') => format!("{sender}{service}"),
                _ => service,
            };
            println!("registered {service}");
            self.items.push(service);
        }

        fn register_status_notifier_host(&mut self, _service: String) {}

        #[zbus(property)]
        fn registered_status_notifier_items(&self) -> Vec<String> {
            self.items.clone()
        }

        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn protocol_version(&self) -> i32 {
            0
        }
    }

    pub fn serve() -> zbus::Result<Infallible> {
        let _connection = zbus::blocking::connection::Builder::session()?
            .name(NAME)?
            .serve_at(PATH, Watcher { items: Vec::new() })?
            .build()?;
        println!("watcher ready");
        loop {
            std::thread::park();
        }
    }
}
