//! Whether this PC is on a network Windows marks Public, such as a cafe or
//! hotel Wi-Fi, where anyone on the network can reach the web remote's
//! pairing page. The server listens on every address, so a single connected
//! Public network is enough for the warning.

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// A network's profile, as Windows' Network List Manager names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Public,
    Private,
    Domain,
}

/// Where the profiles come from: Windows, or a fake in the tests.
pub trait ProfileSource: Send + Sync {
    /// The categories of the networks this PC is connected to; `None` when
    /// Windows didn't answer.
    fn connected(&self) -> Option<Vec<Category>>;
}

/// The warning while the web remote listens: whether a connected network is
/// Public, and whether its notice shows. A dismissed notice stays away until
/// no connected network is Public any more, or the remote starts again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicNetworkWarning {
    public: bool,
    dismissed: bool,
}

impl PublicNetworkWarning {
    /// Takes a reading; one Windows didn't answer keeps the last. Returns
    /// whether anything the windows show changed.
    pub fn read(&mut self, reading: Option<&[Category]>) -> bool {
        let Some(categories) = reading else {
            return false;
        };
        let before = *self;
        self.public = categories.contains(&Category::Public);
        if !self.public {
            self.dismissed = false;
        }
        before != *self
    }

    pub fn public(self) -> bool {
        self.public
    }

    pub fn notice(self) -> bool {
        self.public && !self.dismissed
    }

    /// Hides the notice; returns whether it was showing.
    pub fn dismiss(&mut self) -> bool {
        let showing = self.notice();
        self.dismissed |= showing;
        showing
    }
}

/// Reads a [`ProfileSource`] on a thread of its own, at once and then every
/// `every`, until this value is dropped.
pub struct Watch {
    readings: mpsc::Receiver<Option<Vec<Category>>>,
    _stop: mpsc::Sender<()>,
}

impl Watch {
    pub fn start(source: Arc<dyn ProfileSource>, every: Duration) -> Watch {
        let (reading_sender, readings) = mpsc::channel();
        let (stop, stopped) = mpsc::channel::<()>();
        // The thread isn't joined: a reading may take a moment, and it ends
        // by itself at its next send or wait once this value is gone. Without
        // the thread no reading comes, and the warning never shows.
        let _ = std::thread::Builder::new()
            .name("network profiles".to_owned())
            .spawn(move || {
                loop {
                    if reading_sender.send(source.connected()).is_err() {
                        return;
                    }
                    if !matches!(stopped.recv_timeout(every), Err(RecvTimeoutError::Timeout)) {
                        return;
                    }
                }
            });
        Watch {
            readings,
            _stop: stop,
        }
    }

    /// The newest reading since the last call, if one came.
    pub fn latest(&self) -> Option<Option<Vec<Category>>> {
        self.readings.try_iter().last()
    }
}

/// Windows' Network List Manager.
#[cfg(windows)]
pub struct NetworkList;

#[cfg(windows)]
impl ProfileSource for NetworkList {
    fn connected(&self) -> Option<Vec<Category>> {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

        // SAFETY: initializes COM for this thread only, balanced below.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let categories = connected_categories();
        if initialized {
            // SAFETY: balances the successful CoInitializeEx above; the COM
            // objects were released when `connected_categories` returned.
            unsafe { CoUninitialize() };
        }
        categories
    }
}

#[cfg(windows)]
fn connected_categories() -> Option<Vec<Category>> {
    use windows::Win32::Networking::NetworkListManager::{
        INetworkListManager, NLM_ENUM_NETWORK_CONNECTED, NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED,
        NLM_NETWORK_CATEGORY_PRIVATE, NetworkListManager,
    };
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};

    // SAFETY: plain COM calls on interfaces this function owns.
    unsafe {
        let manager: INetworkListManager =
            CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).ok()?;
        let networks = manager.GetNetworks(NLM_ENUM_NETWORK_CONNECTED).ok()?;
        let mut categories = Vec::new();
        loop {
            let mut slot = [None];
            let mut fetched = 0u32;
            networks.Next(&mut slot, Some(&mut fetched)).ok()?;
            let [Some(network)] = slot else {
                break;
            };
            if fetched == 0 {
                break;
            }
            categories.push(match network.GetCategory().ok()? {
                NLM_NETWORK_CATEGORY_PRIVATE => Category::Private,
                NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED => Category::Domain,
                // Public, or a category newer than this code: warn rather than not.
                _ => Category::Public,
            });
        }
        Some(categories)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    use Category::{Domain, Private, Public};

    struct Fake(Mutex<Option<Vec<Category>>>);

    impl ProfileSource for Fake {
        fn connected(&self) -> Option<Vec<Category>> {
            self.0.lock().unwrap().clone()
        }
    }

    #[test]
    fn any_connected_public_network_warns() {
        let mut warning = PublicNetworkWarning::default();
        assert!(!warning.notice());
        assert!(!warning.read(Some(&[Private, Domain])));
        assert!(!warning.public());
        assert!(
            warning.read(Some(&[Private, Public])),
            "Wi-Fi Public, Ethernet Private"
        );
        assert!(warning.public() && warning.notice());
        assert!(
            !warning.read(Some(&[Public])),
            "still Public: nothing changes"
        );
        assert!(warning.read(Some(&[])), "disconnected");
        assert!(!warning.public() && !warning.notice());
    }

    #[test]
    fn a_reading_windows_didnt_answer_keeps_the_last() {
        let mut warning = PublicNetworkWarning::default();
        warning.read(Some(&[Public]));
        assert!(!warning.read(None));
        assert!(warning.notice());
    }

    #[test]
    fn a_dismissed_notice_comes_back_only_after_a_non_public_reading() {
        let mut warning = PublicNetworkWarning::default();
        assert!(!warning.dismiss(), "nothing showed");
        warning.read(Some(&[Public]));
        assert!(
            warning.notice(),
            "a dismissal before the network was Public doesn't count"
        );

        let mut warning = PublicNetworkWarning::default();
        warning.read(Some(&[Public]));
        assert!(warning.dismiss());
        assert!(!warning.notice());
        assert!(warning.public(), "the remote window still says so");
        assert!(
            !warning.read(Some(&[Public])),
            "the same network: it stays away"
        );
        assert!(!warning.read(None));
        assert!(!warning.notice());
        assert!(warning.read(Some(&[Private])));
        assert!(warning.read(Some(&[Public])), "a Public network again");
        assert!(warning.notice());
    }

    #[test]
    fn the_watch_reads_at_once_and_again_until_dropped() {
        let fake = Arc::new(Fake(Mutex::new(Some(vec![Private]))));
        let source: Arc<dyn ProfileSource> = fake.clone();
        let watch = Watch::start(source, Duration::from_millis(10));
        let wait_for = |wanted: &[Category]| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(Some(reading)) = watch.latest()
                    && reading == wanted
                {
                    return;
                }
                assert!(Instant::now() < deadline, "no reading of {wanted:?}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for(&[Private]);
        *fake.0.lock().unwrap() = Some(vec![Public]);
        wait_for(&[Public]);
        drop(watch);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&fake) > 1 {
            assert!(Instant::now() < deadline, "the thread ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_answers_with_the_connected_networks() {
        assert!(NetworkList.connected().is_some());
    }
}
