//! The daemon's implementations of the capability traits `core` defines.

use std::sync::Arc;

use async_trait::async_trait;
use shimmer_core::{Error, Event, EventSink, HttpBackend, HttpResponse, ModuleId, Result, StoreBackend, TxPlan};
use shimmer_store::Store;

use crate::bus::Bus;

/// Files + log + bus. Store transactions log their own events; everything else that emits
/// goes through [`EventSink::emit`]. Either way: durable first, published second.
pub struct Backend {
    pub store: Arc<Store>,
    pub bus: Bus,
}

impl StoreBackend for Backend {
    fn read(&self, namespace: &str, path: &str) -> Result<Option<Vec<u8>>> {
        self.store.read(namespace, path)
    }

    fn list(&self, namespace: &str, prefix: &str) -> Result<Vec<String>> {
        self.store.list(namespace, prefix)
    }

    /// Published only once logged: a transaction whose last step failed is committed, but its
    /// events wait with it and are published when a later commit lands them (ADR 0040, #309's
    /// review). Recovery at start lands them before anything subscribes; the indexer reads them
    /// from the log.
    fn commit(&self, namespace: &str, plan: TxPlan) -> Result<()> {
        for e in self.store.commit_logged(namespace, plan)? {
            self.bus.publish(e);
        }
        Ok(())
    }
}

impl EventSink for Backend {
    fn emit(&self, event: Event) -> Result<()> {
        self.store.append_event(&event)?;
        self.bus.publish(event);
        Ok(())
    }
}

impl Backend {
    /// Bus only, not the log. For high-frequency, low-value events (task progress) where an
    /// fsync each would cost more than the history is worth.
    pub fn publish_ephemeral(&self, event: Event) {
        self.bus.publish(event);
    }
}

/// The default `ctx.http` for any module that doesn't declare the `"network"` capability
/// (ADR 0027 §4) — fails closed: nothing reaches the network, and callers see `unavailable`,
/// which they may retry.
pub struct DisabledHttp;

#[async_trait]
impl HttpBackend for DisabledHttp {
    async fn get(&self, module: &ModuleId, url: &str, _credential: Option<&str>) -> Result<HttpResponse> {
        Err(Error::unavailable(format!("http gateway not enabled ({module} requested {url})")))
    }
}
