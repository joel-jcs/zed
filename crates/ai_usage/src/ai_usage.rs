pub mod codex;
mod collector;
mod registry;
mod store;
mod types;

pub use codex::*;
pub use collector::*;
pub use registry::*;
pub use store::*;
pub use types::*;

use gpui::{App, AppContext, Entity, Global};
use std::sync::Arc;

pub type QuotaRegistryHandle = Arc<parking_lot::RwLock<QuotaRegistry>>;

struct GlobalQuotaRegistry(QuotaRegistryHandle);
impl Global for GlobalQuotaRegistry {}

pub fn init_registry(cx: &mut App) -> QuotaRegistryHandle {
    if let Some(registry) = cx.try_global::<GlobalQuotaRegistry>() {
        return registry.0.clone();
    }

    let registry = Arc::new(parking_lot::RwLock::new(QuotaRegistry::default()));
    cx.set_global(GlobalQuotaRegistry(registry.clone()));
    registry
}

pub fn registry(cx: &App) -> Option<QuotaRegistryHandle> {
    cx.try_global::<GlobalQuotaRegistry>()
        .map(|registry| registry.0.clone())
}

pub fn register_collector(collector: Arc<dyn QuotaCollector>, cx: &mut App) {
    init_registry(cx).write().register(collector);
}

pub fn collector_for(target: &QuotaTarget, cx: &App) -> Option<Arc<dyn QuotaCollector>> {
    registry(cx).and_then(|registry| registry.read().collector_for(target))
}

struct GlobalQuotaStore(Entity<QuotaStore>);
impl Global for GlobalQuotaStore {}

pub fn init(http_client: Arc<dyn http_client::HttpClient>, cx: &mut App) {
    let registry = init_registry(cx);
    if cx.try_global::<GlobalQuotaStore>().is_none() {
        let store = cx.new(|_| QuotaStore::new(http_client, registry));
        cx.set_global(GlobalQuotaStore(store));
    }
}

pub fn try_store(cx: &App) -> Option<Entity<QuotaStore>> {
    cx.try_global::<GlobalQuotaStore>()
        .map(|store| store.0.clone())
}

pub fn store(cx: &App) -> Option<Entity<QuotaStore>> {
    try_store(cx)
}
