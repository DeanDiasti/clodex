//! Configuration changes retire connection pools; only path changes interrupt leases.
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::watch;

pub struct NetworkMonitor {
    generation: watch::Sender<u64>,
    path_generation: watch::Sender<u64>,
    log: bool,
}

impl NetworkMonitor {
    /// An isolated monitor, also useful for deterministic transport tests.
    pub fn isolated() -> Arc<Self> {
        Self::new(false)
    }

    fn new(log: bool) -> Arc<Self> {
        Arc::new(Self {
            generation: watch::channel(0).0,
            path_generation: watch::channel(0).0,
            log,
        })
    }

    pub fn system() -> Arc<Self> {
        static SYSTEM: OnceLock<Arc<NetworkMonitor>> = OnceLock::new();
        SYSTEM
            .get_or_init(|| {
                let monitor = Self::new(true);
                #[cfg(target_os = "macos")]
                macos::start(monitor.clone());
                monitor
            })
            .clone()
    }

    pub fn generation(&self) -> u64 {
        *self.generation.borrow()
    }

    pub fn lease(&self) -> NetworkLease {
        let mut receiver = self.path_generation.subscribe();
        let generation = *receiver.borrow_and_update();
        NetworkLease {
            receiver,
            generation,
        }
    }

    /// Publish a confirmed path change, including a transition to offline.
    pub fn notify_change(&self) {
        self.generation.send_modify(|generation| *generation += 1);
        self.path_generation
            .send_modify(|generation| *generation += 1);
        self.log_change("path");
    }

    /// Refresh connections without interrupting work on the current path.
    pub fn notify_pool_change(&self) {
        self.generation.send_modify(|generation| *generation += 1);
        self.log_change("pool");
    }

    fn log_change(&self, kind: &str) {
        if self.log {
            crate::logging::create_logger("network").info(
                "network_changed",
                Some(
                    serde_json::json!({"generation": self.generation(), "kind": kind})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            );
        }
    }
}

#[derive(Clone)]
pub struct NetworkLease {
    receiver: watch::Receiver<u64>,
    generation: u64,
}

impl NetworkLease {
    pub fn is_current(&self) -> bool {
        *self.receiver.borrow() == self.generation
    }

    pub async fn changed(&mut self) {
        while self.is_current() {
            if self.receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    pub async fn run<F: Future>(&mut self, future: F) -> Result<F::Output, std::io::Error> {
        tokio::select! {
            biased;
            result = future => Ok(result),
            _ = self.changed() => Err(interrupted()),
        }
    }
}

pub fn interrupted() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        "Network changed; the previous connection was interrupted. Retry on the new network.",
    )
}

/// Rebuild rather than clone the old client: clones share the old socket pool.
/// Recreation is lazy, on the next request, and serialized across callers.
pub struct ClientPool {
    network: Arc<NetworkMonitor>,
    current: Mutex<CachedClient>,
    factory: Box<dyn Fn() -> reqwest::Client + Send + Sync>,
}

struct CachedClient {
    network_generation: u64,
    revision: u64,
    client: Option<reqwest::Client>,
}

/// Identifies the pool used by a request, including after its response starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientRevision(u64);

impl ClientPool {
    pub fn new(
        network: Arc<NetworkMonitor>,
        factory: impl Fn() -> reqwest::Client + Send + Sync + 'static,
    ) -> Self {
        let generation = network.generation();
        let client = factory();
        Self {
            network,
            current: Mutex::new(CachedClient {
                network_generation: generation,
                revision: 0,
                client: Some(client),
            }),
            factory: Box::new(factory),
        }
    }

    /// A caller-supplied client's configuration cannot be reconstructed.
    pub fn fixed(client: reqwest::Client) -> Self {
        Self::new(NetworkMonitor::isolated(), move || client.clone())
    }

    pub fn get(&self) -> reqwest::Client {
        self.get_with_revision().0
    }

    pub fn get_with_revision(&self) -> (reqwest::Client, ClientRevision) {
        let mut current = self.current.lock().unwrap();
        let generation = self.network.generation();
        if current.network_generation != generation || current.client.is_none() {
            current.network_generation = generation;
            current.revision += 1;
            current.client = Some((self.factory)());
        }
        (
            current.client.as_ref().unwrap().clone(),
            ClientRevision(current.revision),
        )
    }

    /// Retire failed connections without letting late errors discard a newer pool.
    /// Existing responses retain their client and are not interrupted here.
    pub fn retire(&self, revision: ClientRevision) {
        let mut current = self.current.lock().unwrap();
        if current.revision == revision.0 {
            current.client = None;
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use core_foundation::{
        array::CFArray,
        base::{CFType, TCFType},
        dictionary::CFDictionary,
        runloop::{CFRunLoop, kCFRunLoopDefaultMode},
        string::CFString,
    };
    use system_configuration::dynamic_store::{
        SCDynamicStore, SCDynamicStoreBuilder, SCDynamicStoreCallBackContext,
    };

    use std::{
        cell::Cell,
        rc::Rc,
        time::{Duration, Instant},
    };

    const QUIET: Duration = Duration::from_millis(1500);

    #[derive(Clone, Default, PartialEq)]
    struct Snapshot {
        path: Vec<Option<CFType>>,
        ipv4_addresses: Vec<String>,
        pool: Vec<Option<CFType>>,
    }

    // Addresses are a set: configd ordering alone is not a path change.
    fn addresses(value: Option<CFType>) -> Vec<String> {
        let mut addresses: Vec<_> = value
            .and_then(|value| value.downcast::<CFArray>())
            .map(|array| {
                array
                    .iter()
                    .filter_map(|value| {
                        // SAFETY: dynamic-store arrays contain CF property-list values.
                        unsafe { CFType::wrap_under_get_rule(*value) }
                            .downcast::<CFString>()
                            .map(|value| value.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        addresses.sort();
        addresses.dedup();
        addresses
    }

    fn field(store: &SCDynamicStore, path: &str, key: &str) -> Option<CFType> {
        let dict = store.get(path)?.downcast_into::<CFDictionary>()?;
        let key = CFString::new(key);
        dict.find(key.as_CFTypeRef()).map(|value| {
            // SAFETY: dynamic-store dictionaries contain retained CF property-list values.
            unsafe { CFType::wrap_under_get_rule(*value) }
        })
    }

    // Compare routing/address/DNS/proxy identity, not lease timestamps or every
    // configd notification. Repeated notifications must not abort healthy work.
    fn snapshot(store: &SCDynamicStore) -> Snapshot {
        let mut values = Snapshot::default();
        for family in ["IPv4", "IPv6"] {
            let path = format!("State:/Network/Global/{family}");
            for key in ["PrimaryInterface", "PrimaryService", "Router"] {
                values.path.push(field(store, &path, key));
            }
            let interface = field(store, &path, "PrimaryInterface")
                .and_then(|value| value.downcast::<CFString>())
                .map(|value| value.to_string());
            values.pool.push(interface.as_ref().and_then(|interface| {
                field(
                    store,
                    &format!("State:/Network/Interface/{interface}/Link"),
                    "Active",
                )
            }));
            let address_values = interface.and_then(|interface| {
                field(
                    store,
                    &format!("State:/Network/Interface/{interface}/{family}"),
                    "Addresses",
                )
            });
            if family == "IPv4" {
                values.ipv4_addresses = addresses(address_values.clone());
            }
            // IPv6 privacy-address rotation must not abort active work.
            values.pool.push(address_values);
        }
        for key in ["ServerAddresses", "SearchDomains", "DomainName"] {
            values
                .pool
                .push(field(store, "State:/Network/Global/DNS", key));
        }
        values.pool.push(
            store
                .get("State:/Network/Global/Proxies")
                .map(|value| value.as_CFType()),
        );
        values
    }

    fn changed(_: SCDynamicStore, _: CFArray<CFString>, pending: &mut Rc<Cell<Option<Instant>>>) {
        pending.set(Some(Instant::now()));
    }

    fn observe(monitor: &NetworkMonitor, previous: &mut Snapshot, next: Snapshot) {
        if next != *previous {
            if next.path != previous.path || next.ipv4_addresses != previous.ipv4_addresses {
                monitor.notify_change();
            } else {
                monitor.notify_pool_change();
            }
            *previous = next;
        }
    }

    fn settled(pending: &Cell<Option<Instant>>, now: Instant) -> bool {
        if pending
            .get()
            .is_some_and(|last| now.duration_since(last) >= QUIET)
        {
            pending.set(None);
            true
        } else {
            false
        }
    }

    fn watch(monitor: Arc<NetworkMonitor>, ready: std::sync::mpsc::SyncSender<()>) -> Option<()> {
        let initial = SCDynamicStoreBuilder::new("clodex-network-initial").build()?;
        let mut previous = snapshot(&initial);
        let pending = Rc::new(Cell::new(None));
        let store = SCDynamicStoreBuilder::new("clodex-network")
            .callback_context(SCDynamicStoreCallBackContext {
                callout: changed,
                info: pending.clone(),
            })
            .build()?;
        let keys: CFArray<CFString> = CFArray::from_CFTypes(&[]);
        let patterns = CFArray::from_CFTypes(&[
            CFString::new("State:/Network/Global/(IPv4|IPv6|DNS|Proxies)"),
            CFString::new("State:/Network/Interface/.*/(IPv4|IPv6|Link)"),
        ]);
        if !store.set_notification_keys(&keys, &patterns) {
            return None;
        }
        let source = store.create_run_loop_source()?;
        let run_loop = CFRunLoop::get_current();
        // SAFETY: this is the framework's static run-loop mode constant.
        run_loop.add_source(&source, unsafe { kCFRunLoopDefaultMode });
        // Include changes during registration in the same settling window.
        pending.set(Some(Instant::now()));
        let _ = ready.send(());
        loop {
            let wait = pending.get().map_or(Duration::from_secs(60), |last| {
                QUIET.saturating_sub(last.elapsed())
            });
            // Dispatch notifications on this thread and restart the quiet window
            // after each callback. No CF objects cross thread boundaries.
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, wait, true);
            if settled(&pending, Instant::now()) {
                observe(&monitor, &mut previous, snapshot(&store));
            }
        }
    }

    #[test]
    fn repeated_notifications_do_not_retire_healthy_connections() {
        let monitor = NetworkMonitor::isolated();
        let identity = || Snapshot {
            path: vec![Some(CFString::new("network-a").as_CFType())],
            ..Snapshot::default()
        };
        let mut previous = identity();
        observe(&monitor, &mut previous, identity());
        assert_eq!(monitor.generation(), 0);
        let lease = monitor.lease();
        observe(&monitor, &mut previous, Snapshot::default()); // offline
        observe(&monitor, &mut previous, Snapshot::default());
        assert_eq!(monitor.generation(), 1);
        assert!(!lease.is_current());
        observe(&monitor, &mut previous, identity()); // reconnect
        assert_eq!(monitor.generation(), 2);
    }

    #[test]
    fn dns_and_proxy_changes_only_retire_pools() {
        let monitor = NetworkMonitor::isolated();
        let lease = monitor.lease();
        let mut previous = Snapshot::default();
        for value in ["dns", "proxy"] {
            let mut next = previous.clone();
            next.pool.push(Some(CFString::new(value).as_CFType()));
            observe(&monitor, &mut previous, next);
            assert!(lease.is_current());
        }
        assert_eq!(monitor.generation(), 2);
    }

    #[test]
    fn ipv4_address_change_interrupts_leases() {
        let monitor = NetworkMonitor::isolated();
        let lease = monitor.lease();
        observe(
            &monitor,
            &mut Snapshot::default(),
            Snapshot {
                ipv4_addresses: vec!["192.0.2.1".into()],
                ..Snapshot::default()
            },
        );
        assert!(!lease.is_current());
    }

    #[test]
    fn notifications_restart_the_quiet_window() {
        let start = Instant::now();
        let pending = Cell::new(Some(start));
        assert!(!settled(&pending, start + Duration::from_secs(1)));
        pending.set(Some(start + Duration::from_secs(1)));
        assert!(!settled(&pending, start + QUIET));
        assert!(settled(&pending, start + Duration::from_secs(1) + QUIET));
        assert!(!settled(&pending, start + Duration::from_secs(10)));
    }

    pub(super) fn start(monitor: Arc<NetworkMonitor>) {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let result = std::thread::Builder::new()
            .name("clodex-network".into())
            .spawn(move || {
                if watch(monitor, ready_tx).is_none() {
                    crate::logging::create_logger("network")
                        .warn("network_monitor_unavailable", None);
                }
            });
        if result.is_err()
            || ready_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_err()
        {
            crate::logging::create_logger("network").warn("network_monitor_start_failed", None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ready_future_wins_over_change() {
        let monitor = NetworkMonitor::isolated();
        let mut lease = monitor.lease();
        monitor.notify_change();
        assert_eq!(lease.run(std::future::ready(42)).await.unwrap(), 42);
    }

    #[test]
    fn isolated_monitors_disable_logging() {
        let monitor = NetworkMonitor::isolated();
        assert!(!monitor.log);
        monitor.notify_change();
        monitor.notify_pool_change();
    }

    #[tokio::test]
    async fn change_interrupts_pending_work_and_new_lease_recovers() {
        let monitor = NetworkMonitor::isolated();
        let mut old = monitor.lease();
        monitor.notify_change();
        let error = old.run(std::future::pending::<()>()).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionAborted);
        assert_eq!(monitor.lease().run(async { 42 }).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn change_wakes_an_already_waiting_request() {
        let monitor = NetworkMonitor::isolated();
        let mut lease = monitor.lease();
        let waiting = tokio::spawn(async move { lease.run(std::future::pending::<()>()).await });
        tokio::task::yield_now().await;
        monitor.notify_change();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn clients_are_rebuilt_once_per_observed_generation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let monitor = NetworkMonitor::isolated();
        let builds = Arc::new(AtomicUsize::new(0));
        let count = builds.clone();
        let pool = ClientPool::new(monitor.clone(), move || {
            count.fetch_add(1, Ordering::Relaxed);
            reqwest::Client::new()
        });
        pool.get();
        pool.get();
        assert_eq!(builds.load(Ordering::Relaxed), 1);
        let lease = monitor.lease();
        monitor.notify_pool_change();
        assert!(lease.is_current());
        pool.get();
        pool.get();
        assert_eq!(builds.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn concurrent_failures_retire_only_the_pool_they_used() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let builds = Arc::new(AtomicUsize::new(0));
        let count = builds.clone();
        let pool = ClientPool::new(NetworkMonitor::isolated(), move || {
            count.fetch_add(1, Ordering::Relaxed);
            reqwest::Client::new()
        });
        let (_, old) = pool.get_with_revision();
        pool.retire(old);
        pool.retire(old);
        let (_, fresh) = pool.get_with_revision();
        assert_ne!(fresh, old);
        pool.retire(old); // A late error on an old connection must not evict fresh connections.
        assert_eq!(pool.get_with_revision().1, fresh);
        assert_eq!(builds.load(Ordering::Relaxed), 2);
    }
}
