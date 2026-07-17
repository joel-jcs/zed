use crate::{
    QuotaAccount, QuotaCacheKey, QuotaCollector, QuotaError, QuotaFetchContext, QuotaRefreshPolicy,
    QuotaRegistryHandle, QuotaSnapshot, QuotaTarget, QuotaView,
};
use anyhow::Result;
use collections::HashMap;
use futures::{FutureExt as _, future::BoxFuture, future::Shared};
use gpui::{Context, Task, WeakEntity};
use http_client::HttpClient;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const ENTRY_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct QuotaResolutionKey {
    collector_id: Arc<str>,
    target: QuotaTarget,
}

struct TargetState {
    cache_key: Option<QuotaCacheKey>,
    account: Option<crate::QuotaAccountSummary>,
    active_count: usize,
    is_fetching: bool,
    error: Option<QuotaError>,
    last_attempt_at: Option<Instant>,
    last_used_at: Instant,
}

struct QuotaEntry {
    snapshot: Option<QuotaSnapshot>,
    error: Option<QuotaError>,
    fetched_at_monotonic: Option<Instant>,
    last_used_at: Instant,
    cooldown_until: Option<Instant>,
}

#[derive(Clone)]
struct FetchedQuota {
    snapshot: QuotaSnapshot,
    fetched_at_monotonic: Instant,
    fetched_at_unix_ms: i64,
}

type SharedFetch = Shared<BoxFuture<'static, Result<FetchedQuota, Arc<QuotaError>>>>;

pub struct QuotaStore {
    http_client: Arc<dyn HttpClient>,
    registry: QuotaRegistryHandle,
    refresh_policy: QuotaRefreshPolicy,
    monotonic_now: Arc<dyn Fn() -> Instant + Send + Sync>,
    unix_now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    targets: HashMap<QuotaTarget, TargetState>,
    entries: HashMap<QuotaCacheKey, QuotaEntry>,
    in_flight_targets: HashMap<QuotaResolutionKey, Task<Result<()>>>,
    in_flight_fetches: HashMap<QuotaCacheKey, SharedFetch>,
    scheduled_refreshes: HashMap<QuotaTarget, Task<Result<()>>>,
}

impl QuotaStore {
    pub fn new(http_client: Arc<dyn HttpClient>, registry: QuotaRegistryHandle) -> Self {
        Self::new_with_dependencies(
            http_client,
            registry,
            QuotaRefreshPolicy::default(),
            Arc::new(Instant::now),
            Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or_default()
            }),
        )
    }

    fn new_with_dependencies(
        http_client: Arc<dyn HttpClient>,
        registry: QuotaRegistryHandle,
        refresh_policy: QuotaRefreshPolicy,
        monotonic_now: Arc<dyn Fn() -> Instant + Send + Sync>,
        unix_now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            http_client,
            registry,
            refresh_policy: refresh_policy.normalized(),
            monotonic_now,
            unix_now_ms,
            targets: HashMap::default(),
            entries: HashMap::default(),
            in_flight_targets: HashMap::default(),
            in_flight_fetches: HashMap::default(),
            scheduled_refreshes: HashMap::default(),
        }
    }

    pub fn activate(&mut self, target: QuotaTarget, cx: &mut Context<Self>) {
        let now = (self.monotonic_now)();
        let should_refresh = {
            let state = self.targets.entry(target.clone()).or_insert(TargetState {
                cache_key: None,
                account: None,
                active_count: 0,
                is_fetching: false,
                error: None,
                last_attempt_at: None,
                last_used_at: now,
            });
            state.active_count += 1;
            state.last_used_at = now;
            let has_snapshot = state
                .cache_key
                .as_ref()
                .and_then(|key| self.entries.get(key))
                .and_then(|entry| entry.snapshot.as_ref())
                .is_some();
            !has_snapshot && !state.is_fetching
        };

        if should_refresh {
            self.refresh(target, false, cx);
        }
    }

    pub fn deactivate(&mut self, target: &QuotaTarget, _cx: &mut Context<Self>) {
        let Some(state) = self.targets.get_mut(target) else {
            return;
        };
        state.active_count = state.active_count.saturating_sub(1);
        if state.active_count == 0 {
            self.scheduled_refreshes.remove(target);
        }
    }

    pub fn view(&mut self, target: &QuotaTarget) -> Option<QuotaView> {
        let collector = self.registry.read().collector_for(target)?;
        let now = (self.monotonic_now)();
        let state = self.targets.entry(target.clone()).or_insert(TargetState {
            cache_key: None,
            account: None,
            active_count: 0,
            is_fetching: false,
            error: None,
            last_attempt_at: None,
            last_used_at: now,
        });
        state.last_used_at = now;
        let cache_key = state.cache_key.clone();
        let cached_account = state.account.clone();
        let (mut snapshot, entry_error) = cache_key
            .as_ref()
            .and_then(|key| self.entries.get_mut(key))
            .map(|entry| {
                entry.last_used_at = now;
                (entry.snapshot.clone(), entry.error.clone())
            })
            .unwrap_or((None, None));
        let account = snapshot
            .as_ref()
            .map(|snapshot| snapshot.account.clone())
            .or(cached_account);
        if let Some(snapshot) = snapshot.as_mut() {
            snapshot.active_model_id = collector.model_match_id(target);
        }
        Some(QuotaView {
            provider_id: target.provider_or_agent_id.clone(),
            quota_family_id: collector.quota_family_id(),
            provider_name: collector.display_name(),
            account,
            snapshot,
            is_fetching: state.is_fetching,
            error: state.error.clone().or(entry_error),
        })
    }

    pub fn refresh(&mut self, target: QuotaTarget, force: bool, cx: &mut Context<Self>) {
        let Some(collector) = self.registry.read().collector_for(&target) else {
            return;
        };
        let resolution_key = QuotaResolutionKey {
            collector_id: collector.id(),
            target: target.clone(),
        };
        if self.in_flight_targets.contains_key(&resolution_key) {
            return;
        }
        let now = (self.monotonic_now)();
        let state = self.targets.entry(target.clone()).or_insert(TargetState {
            cache_key: None,
            account: None,
            active_count: 0,
            is_fetching: false,
            error: None,
            last_attempt_at: None,
            last_used_at: now,
        });
        state.is_fetching = true;
        state.error = None;
        state.last_attempt_at = Some(now);
        state.last_used_at = now;

        let weak = cx.weak_entity();
        let task_resolution_key = resolution_key.clone();
        let task = cx.spawn(async move |_this, cx| {
            run_target_refresh(weak, resolution_key, target, collector, force, cx).await
        });
        self.in_flight_targets.insert(task_resolution_key, task);
    }

    pub fn set_refresh_policy(&mut self, policy: QuotaRefreshPolicy, cx: &mut Context<Self>) {
        self.refresh_policy = policy.normalized();
        self.scheduled_refreshes.clear();
        if self.refresh_policy.auto_refresh {
            let targets = self
                .targets
                .iter()
                .filter(|(_, state)| state.active_count > 0)
                .map(|(target, _)| target.clone())
                .collect::<Vec<_>>();
            for target in targets {
                self.schedule_next_refresh(target, cx);
            }
        }
    }

    pub fn evict_unused(&mut self, now: Instant) {
        self.entries.retain(|_, entry| {
            now.saturating_duration_since(entry.last_used_at) <= ENTRY_RETENTION
        });
        self.targets.retain(|_, state| {
            state.active_count > 0
                || now.saturating_duration_since(state.last_used_at) <= ENTRY_RETENTION
        });
    }

    fn prepare_fetch(
        &mut self,
        target: &QuotaTarget,
        collector: &Arc<dyn QuotaCollector>,
        account: &QuotaAccount,
        force: bool,
        async_app: &gpui::AsyncApp,
    ) -> PreparedFetch {
        let now = (self.monotonic_now)();
        let scope = collector.cache_scope(target, account);
        let cache_key = QuotaCacheKey {
            collector_id: collector.id(),
            credential_source: account.credential_source.clone(),
            account_fingerprint: account.fingerprint.clone(),
            provider_or_agent_id: target.provider_or_agent_id.clone(),
            scope,
        };
        let target_state = self.targets.entry(target.clone()).or_insert(TargetState {
            cache_key: None,
            account: None,
            active_count: 0,
            is_fetching: true,
            error: None,
            last_attempt_at: Some(now),
            last_used_at: now,
        });
        target_state.cache_key = Some(cache_key.clone());
        target_state.account = Some(account.summary());
        target_state.last_used_at = now;
        let entry = self.entries.entry(cache_key.clone()).or_insert(QuotaEntry {
            snapshot: None,
            error: None,
            fetched_at_monotonic: None,
            last_used_at: now,
            cooldown_until: None,
        });
        entry.last_used_at = now;
        let effective_ttl = self.refresh_policy.interval.max(collector.minimum_ttl());
        let is_fresh = !force
            && entry.snapshot.is_some()
            && entry
                .fetched_at_monotonic
                .is_some_and(|fetched| now.saturating_duration_since(fetched) < effective_ttl);
        if is_fresh {
            return PreparedFetch::FreshCache(cache_key);
        }
        if entry.cooldown_until.is_some_and(|until| until > now) {
            return PreparedFetch::Cooldown(cache_key);
        }
        let task = if let Some(task) = self.in_flight_fetches.get(&cache_key) {
            task.clone()
        } else {
            let http_client = self.http_client.clone();
            let account = account.clone();
            let collector = collector.clone();
            let target = target.clone();
            let monotonic_now = self.monotonic_now.clone();
            let unix_now_ms = self.unix_now_ms.clone();
            let fetch = collector.fetch(
                target,
                account.clone(),
                QuotaFetchContext { http_client },
                async_app,
            );
            let task = async move {
                let mut snapshot = fetch.await.map_err(Arc::new)?;
                let fetched_at_monotonic = monotonic_now();
                let fetched_at_unix_ms = unix_now_ms();
                snapshot.account = account.summary();
                Ok(FetchedQuota {
                    snapshot,
                    fetched_at_monotonic,
                    fetched_at_unix_ms,
                })
            }
            .boxed()
            .shared();
            self.in_flight_fetches
                .insert(cache_key.clone(), task.clone());
            task
        };
        PreparedFetch::Fetch { cache_key, task }
    }

    fn finish_target(
        &mut self,
        resolution_key: &QuotaResolutionKey,
        target: &QuotaTarget,
        cache_key: Option<&QuotaCacheKey>,
        result: Option<Result<FetchedQuota, Arc<QuotaError>>>,
        error: Option<QuotaError>,
        cx: &mut Context<Self>,
    ) {
        if let (Some(cache_key), Some(result)) = (cache_key, result) {
            if let Some(entry) = self.entries.get_mut(cache_key) {
                match result {
                    Ok(fetched) => {
                        let mut snapshot = fetched.snapshot;
                        snapshot.fetched_at_unix_ms = fetched.fetched_at_unix_ms;
                        entry.snapshot = Some(snapshot);
                        entry.fetched_at_monotonic = Some(fetched.fetched_at_monotonic);
                        entry.error = None;
                        entry.cooldown_until = None;
                    }
                    Err(error) => {
                        entry.error = Some((*error).clone());
                        if let Some(retry_after) = error.retry_after() {
                            entry.cooldown_until = Some((self.monotonic_now)() + retry_after);
                        }
                    }
                }
                entry.last_used_at = (self.monotonic_now)();
            }
        }
        if let Some(state) = self.targets.get_mut(target) {
            state.is_fetching = false;
            state.error = error;
            state.last_used_at = (self.monotonic_now)();
        }
        self.in_flight_targets.remove(resolution_key);
        if let Some(task) = cache_key {
            self.in_flight_fetches.remove(task);
        }
        self.schedule_next_refresh(target.clone(), cx);
        cx.notify();
    }

    pub fn targets_for_popover(&self, active: Option<&QuotaTarget>) -> Vec<QuotaTarget> {
        let registry = self.registry.read();
        let active_collector = active.and_then(|target| registry.collector_for(target));
        let active_collector_id = active_collector.as_ref().map(|collector| collector.id());
        let mut targets = Vec::new();
        if active_collector.is_some()
            && let Some(active) = active
        {
            targets.push(active.clone());
        }

        for target in registry.discovery_targets() {
            let Some(collector) = registry.collector_for(&target) else {
                continue;
            };
            if (active_collector_id
                .as_ref()
                .is_some_and(|active_id| *active_id == collector.id())
                && target.model_id.is_none())
                || targets.contains(&target)
            {
                continue;
            }
            targets.push(target);
        }
        targets
    }

    fn schedule_next_refresh(&mut self, target: QuotaTarget, cx: &mut Context<Self>) {
        if !self.refresh_policy.auto_refresh
            || self
                .targets
                .get(&target)
                .is_none_or(|state| state.active_count == 0)
        {
            self.scheduled_refreshes.remove(&target);
            return;
        }

        let Some(collector) = self.registry.read().collector_for(&target) else {
            return;
        };
        let delay = self.refresh_policy.interval.max(collector.minimum_ttl());
        let weak = cx.weak_entity();
        let scheduled_target = target.clone();
        let task = cx.spawn(async move |_this, cx| {
            cx.background_executor().timer(delay).await;
            weak.update(cx, |store, cx| {
                store.scheduled_refreshes.remove(&scheduled_target);
                store.refresh(scheduled_target.clone(), false, cx);
            })?;
            Ok(())
        });
        self.scheduled_refreshes.insert(target, task);
    }
}

enum PreparedFetch {
    FreshCache(QuotaCacheKey),
    Cooldown(QuotaCacheKey),
    Fetch {
        cache_key: QuotaCacheKey,
        task: SharedFetch,
    },
}

async fn run_target_refresh(
    this: WeakEntity<QuotaStore>,
    resolution_key: QuotaResolutionKey,
    target: QuotaTarget,
    collector: Arc<dyn QuotaCollector>,
    force: bool,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let mut account_resolution_attempts = 0;
    loop {
        let account = match collector.resolve_account(target.clone(), cx).await {
            Ok(account) => account,
            Err(error)
                if error == QuotaError::AmbiguousAccount && account_resolution_attempts == 0 =>
            {
                account_resolution_attempts += 1;
                continue;
            }
            Err(error) => {
                this.update(cx, |store, cx| {
                    store.finish_target(&resolution_key, &target, None, None, Some(error), cx);
                })?;
                return Ok(());
            }
        };

        let async_app = cx.clone();
        let prepared = this.update(cx, |store, _| {
            store.prepare_fetch(&target, &collector, &account, force, &async_app)
        })?;
        match prepared {
            PreparedFetch::FreshCache(cache_key) => {
                this.update(cx, |store, cx| {
                    store.finish_target(&resolution_key, &target, Some(&cache_key), None, None, cx);
                })?;
                return Ok(());
            }
            PreparedFetch::Cooldown(cache_key) => {
                this.update(cx, |store, cx| {
                    store.finish_target(
                        &resolution_key,
                        &target,
                        Some(&cache_key),
                        None,
                        Some(QuotaError::Cooldown),
                        cx,
                    );
                })?;
                return Ok(());
            }
            PreparedFetch::Fetch { cache_key, task } => {
                let result = task.await;
                if result
                    .as_ref()
                    .err()
                    .is_some_and(|error| **error == QuotaError::AmbiguousAccount)
                    && account_resolution_attempts == 0
                {
                    account_resolution_attempts += 1;
                    this.update(cx, |store, _| {
                        if let Some(state) = store.targets.get_mut(&target) {
                            state.cache_key = None;
                        }
                        store.in_flight_fetches.remove(&cache_key);
                    })?;
                    continue;
                }
                let error = result.as_ref().err().map(|error| (**error).clone());
                this.update(cx, |store, cx| {
                    store.finish_target(
                        &resolution_key,
                        &target,
                        Some(&cache_key),
                        Some(result),
                        error,
                        cx,
                    );
                })?;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use futures::{channel::oneshot, future::BoxFuture};
    use gpui::{AppContext, Entity, TestAppContext};
    use parking_lot::Mutex;
    use std::{
        collections::VecDeque,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    #[allow(dead_code)]
    enum FakeQuotaResponse {
        Snapshot(QuotaSnapshot),
        MissingCredentials,
        AmbiguousAccount,
        RateLimited(Duration),
        ProviderError(&'static str),
    }

    struct FakeCollector {
        id: Arc<str>,
        provider_id: Arc<str>,
        account: QuotaAccount,
        scope: QuotaCacheScope,
        minimum_ttl: Duration,
        resolve_count: Arc<AtomicUsize>,
        fetch_count: Arc<AtomicUsize>,
        resolve_gate: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
        fetch_gate: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
        responses: Arc<Mutex<VecDeque<FakeQuotaResponse>>>,
        resolve_error: Arc<Mutex<Option<QuotaError>>>,
        scope_by_model: bool,
        discovery_target: Option<QuotaTarget>,
        normalize_model_ids: bool,
    }

    impl FakeCollector {
        fn new(id: &str, provider_id: &str) -> Arc<Self> {
            Arc::new(Self {
                id: Arc::from(id),
                provider_id: Arc::from(provider_id),
                account: QuotaAccount::from_stable_identity(
                    "fake",
                    b"fake-account",
                    Some("fake@example.test".into()),
                ),
                scope: QuotaCacheScope::Account,
                minimum_ttl: Duration::from_secs(15),
                resolve_count: Arc::new(AtomicUsize::new(0)),
                fetch_count: Arc::new(AtomicUsize::new(0)),
                resolve_gate: Arc::new(Mutex::new(None)),
                fetch_gate: Arc::new(Mutex::new(None)),
                responses: Arc::new(Mutex::new(VecDeque::new())),
                resolve_error: Arc::new(Mutex::new(None)),
                scope_by_model: false,
                discovery_target: None,
                normalize_model_ids: false,
            })
        }

        fn with_model_scope(self: Arc<Self>) -> Arc<Self> {
            Arc::new(Self {
                scope_by_model: true,
                ..(*self).clone_for_test()
            })
        }

        fn clone_for_test(&self) -> Self {
            Self {
                id: self.id.clone(),
                provider_id: self.provider_id.clone(),
                account: self.account.clone(),
                scope: self.scope.clone(),
                minimum_ttl: self.minimum_ttl,
                resolve_count: self.resolve_count.clone(),
                fetch_count: self.fetch_count.clone(),
                resolve_gate: self.resolve_gate.clone(),
                fetch_gate: self.fetch_gate.clone(),
                responses: self.responses.clone(),
                resolve_error: self.resolve_error.clone(),
                scope_by_model: self.scope_by_model,
                discovery_target: self.discovery_target.clone(),
                normalize_model_ids: self.normalize_model_ids,
            }
        }

        fn with_minimum_ttl(self: Arc<Self>, minimum_ttl: Duration) -> Arc<Self> {
            Arc::new(Self {
                minimum_ttl,
                ..(*self).clone_for_test()
            })
        }

        fn with_discovery_target(self: Arc<Self>, discovery_target: QuotaTarget) -> Arc<Self> {
            Arc::new(Self {
                discovery_target: Some(discovery_target),
                ..(*self).clone_for_test()
            })
        }

        fn with_normalized_model_ids(self: Arc<Self>) -> Arc<Self> {
            Arc::new(Self {
                normalize_model_ids: true,
                ..(*self).clone_for_test()
            })
        }

        fn set_resolve_gate(&self, receiver: oneshot::Receiver<()>) {
            self.resolve_gate.lock().replace(receiver);
        }

        fn set_fetch_gate(&self, receiver: oneshot::Receiver<()>) {
            self.fetch_gate.lock().replace(receiver);
        }

        fn set_resolve_error(&self, error: QuotaError) {
            self.resolve_error.lock().replace(error);
        }

        fn push_response(&self, response: FakeQuotaResponse) {
            self.responses.lock().push_back(response);
        }
    }

    impl QuotaCollector for FakeCollector {
        fn id(&self) -> Arc<str> {
            self.id.clone()
        }

        fn display_name(&self) -> gpui::SharedString {
            self.id.clone().into()
        }

        fn supports(&self, target: &QuotaTarget) -> bool {
            target.provider_or_agent_id == self.provider_id
        }

        fn discovery_target(&self) -> Option<QuotaTarget> {
            self.discovery_target.clone()
        }

        fn model_match_id(&self, target: &QuotaTarget) -> Option<Arc<str>> {
            if self.normalize_model_ids {
                target.model_id.as_ref().map(|model_id| {
                    if model_id.contains("spark") {
                        Arc::from("spark")
                    } else {
                        Arc::from("regular")
                    }
                })
            } else {
                target.model_id.clone()
            }
        }

        fn minimum_ttl(&self) -> Duration {
            self.minimum_ttl
        }

        fn cache_scope(&self, target: &QuotaTarget, _account: &QuotaAccount) -> QuotaCacheScope {
            if self.scope_by_model {
                QuotaCacheScope::Model(target.model_id.clone().unwrap_or_default())
            } else {
                self.scope.clone()
            }
        }

        fn resolve_account(
            &self,
            _target: QuotaTarget,
            _cx: &gpui::AsyncApp,
        ) -> BoxFuture<'static, Result<QuotaAccount, QuotaError>> {
            let resolve_count = self.resolve_count.clone();
            let resolve_gate = self.resolve_gate.clone();
            let account = self.account.clone();
            let resolve_error = self.resolve_error.clone();
            Box::pin(async move {
                resolve_count.fetch_add(1, Ordering::SeqCst);
                let gate = resolve_gate.lock().take();
                if let Some(gate) = gate {
                    gate.await.map_err(|_| QuotaError::AmbiguousAccount)?;
                }
                if let Some(error) = resolve_error.lock().clone() {
                    return Err(error);
                }
                Ok(account)
            })
        }

        fn fetch(
            &self,
            _target: QuotaTarget,
            _account: QuotaAccount,
            _context: QuotaFetchContext,
            _cx: &gpui::AsyncApp,
        ) -> BoxFuture<'static, Result<QuotaSnapshot, QuotaError>> {
            let fetch_count = self.fetch_count.clone();
            let fetch_gate = self.fetch_gate.clone();
            let responses = self.responses.clone();
            Box::pin(async move {
                fetch_count.fetch_add(1, Ordering::SeqCst);
                let gate = fetch_gate.lock().take();
                if let Some(gate) = gate {
                    gate.await.map_err(|_| QuotaError::AmbiguousAccount)?;
                }
                match responses.lock().pop_front() {
                    Some(FakeQuotaResponse::Snapshot(snapshot)) => Ok(snapshot),
                    Some(FakeQuotaResponse::MissingCredentials) => {
                        Err(QuotaError::MissingCredentials)
                    }
                    Some(FakeQuotaResponse::AmbiguousAccount) => Err(QuotaError::AmbiguousAccount),
                    Some(FakeQuotaResponse::RateLimited(retry_after)) => {
                        Err(QuotaError::RateLimited {
                            message: "quota temporarily rate limited".into(),
                            retry_after,
                        })
                    }
                    Some(FakeQuotaResponse::ProviderError(message)) => {
                        Err(QuotaError::Provider(message.into()))
                    }
                    None => Err(QuotaError::Provider("fake response queue exhausted".into())),
                }
            })
        }
    }

    struct Fixture {
        store: Entity<QuotaStore>,
        monotonic_now: Arc<Mutex<Instant>>,
    }

    fn target(provider_id: &str, model_id: &str) -> QuotaTarget {
        QuotaTarget {
            kind: QuotaTargetKind::ExternalAgent,
            provider_or_agent_id: Arc::from(provider_id),
            upstream_provider_id: None,
            model_id: Some(Arc::from(model_id)),
            model_name: Some(model_id.into()),
        }
    }

    fn model_less_target(provider_id: &str) -> QuotaTarget {
        QuotaTarget {
            kind: QuotaTargetKind::ExternalAgent,
            provider_or_agent_id: Arc::from(provider_id),
            upstream_provider_id: None,
            model_id: None,
            model_name: None,
        }
    }

    fn snapshot(remaining_percent: f64) -> QuotaSnapshot {
        QuotaSnapshot {
            provider_id: Arc::from("fake"),
            provider_name: "Fake".into(),
            account: QuotaAccountSummary {
                fingerprint: Arc::from("unassigned"),
                safe_label: None,
            },
            plan: None,
            fetched_at_unix_ms: 0,
            active_model_id: Some(Arc::from("model-a")),
            windows: vec![QuotaWindow::percentage("5h", remaining_percent)],
            model_windows: Default::default(),
            groups: Vec::new(),
            available_resets: None,
        }
    }

    fn grouped_snapshot() -> QuotaSnapshot {
        let mut snapshot = snapshot(72.0);
        snapshot.active_model_id = Some(Arc::from("active"));
        snapshot.groups = vec![
            QuotaGroup {
                id: Arc::from("active-models"),
                display_name: "Active models".into(),
                description: None,
                applies_to_model_ids: vec![Arc::from("active")],
                affects_severity: true,
                buckets: vec![QuotaBucket {
                    id: Arc::from("active-window"),
                    display_name: "active".into(),
                    description: None,
                    window: QuotaWindow::percentage("active", 70.0),
                }],
            },
            QuotaGroup {
                id: Arc::from("inactive-models"),
                display_name: "Inactive models".into(),
                description: None,
                applies_to_model_ids: vec![Arc::from("inactive")],
                affects_severity: true,
                buckets: vec![QuotaBucket {
                    id: Arc::from("inactive-window"),
                    display_name: "inactive".into(),
                    description: None,
                    window: QuotaWindow::percentage("inactive", 2.0),
                }],
            },
        ];
        snapshot
    }

    fn fixture(cx: &mut TestAppContext, collector: Arc<FakeCollector>) -> Fixture {
        fixture_with_collectors(cx, vec![collector])
    }

    fn fixture_with_collectors(
        cx: &mut TestAppContext,
        collectors: Vec<Arc<FakeCollector>>,
    ) -> Fixture {
        let registry = Arc::new(parking_lot::RwLock::new(QuotaRegistry::default()));
        for collector in collectors {
            registry.write().register(collector);
        }
        let monotonic_now = Arc::new(Mutex::new(Instant::now()));
        let unix_now_ms = Arc::new(Mutex::new(1_700_000_000_000));
        let monotonic_clock = monotonic_now.clone();
        let unix_clock = unix_now_ms;
        let http_client: Arc<dyn http_client::HttpClient> =
            http_client::FakeHttpClient::with_404_response();
        let store = cx.update(|cx| {
            cx.new(|_| {
                QuotaStore::new_with_dependencies(
                    http_client,
                    registry,
                    QuotaRefreshPolicy::default(),
                    Arc::new(move || *monotonic_clock.lock()),
                    Arc::new(move || *unix_clock.lock()),
                )
            })
        });
        Fixture {
            store,
            monotonic_now,
        }
    }

    fn settle(cx: &mut TestAppContext) {
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn switches_targets_while_preserving_cache_and_applicability(cx: &mut TestAppContext) {
        let collector_a = FakeCollector::new("fake-a", "provider-a");
        collector_a.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        collector_a.push_response(FakeQuotaResponse::ProviderError("provider failed"));
        collector_a.push_response(FakeQuotaResponse::Snapshot(grouped_snapshot()));
        let collector_b = FakeCollector::new("fake-b", "provider-b");
        collector_b.push_response(FakeQuotaResponse::Snapshot(snapshot(18.0)));
        let fixture = fixture_with_collectors(cx, vec![collector_a.clone(), collector_b.clone()]);
        let target_a = target("provider-a", "model-a");
        let target_b = target("provider-b", "model-b");

        fixture.store.update(cx, |store, cx| {
            store.activate(target_a.clone(), cx);
        });
        settle(cx);
        let fetched_at = fixture.store.update(cx, |store, _| {
            store
                .view(&target_a)
                .and_then(|view| view.snapshot.map(|snapshot| snapshot.fetched_at_unix_ms))
        });
        assert_eq!(collector_a.fetch_count.load(Ordering::SeqCst), 1);

        fixture.store.update(cx, |store, cx| {
            store.deactivate(&target_a, cx);
            store.activate(target_b.clone(), cx);
        });
        settle(cx);
        assert_eq!(collector_b.fetch_count.load(Ordering::SeqCst), 1);

        fixture.store.update(cx, |store, cx| {
            store.deactivate(&target_b, cx);
            store.activate(target_a.clone(), cx);
        });
        settle(cx);
        assert_eq!(collector_a.fetch_count.load(Ordering::SeqCst), 1);

        fixture.store.update(cx, |store, cx| {
            store.refresh(target_a.clone(), true, cx);
        });
        settle(cx);
        fixture.store.update(cx, |store, _| {
            let view = store
                .view(&target_a)
                .expect("collector should support target");
            assert_eq!(
                view.snapshot
                    .as_ref()
                    .and_then(|s| s.windows[0].remaining_percent),
                Some(72.0)
            );
            assert_eq!(
                view.snapshot.as_ref().map(|s| s.fetched_at_unix_ms),
                fetched_at
            );
            assert_eq!(
                view.error,
                Some(QuotaError::Provider("provider failed".into()))
            );
        });

        fixture.store.update(cx, |store, cx| {
            store.refresh(target_a.clone(), true, cx);
        });
        settle(cx);
        fixture.store.update(cx, |store, _| {
            let view = store
                .view(&target_a)
                .expect("collector should support target");
            assert_eq!(
                view.snapshot.as_ref().map(QuotaSnapshot::severity),
                Some(Some(QuotaSeverity::Normal))
            );
        });

        fixture.store.update(cx, |store, cx| {
            store.deactivate(&target_a, cx);
            store.set_refresh_policy(
                QuotaRefreshPolicy {
                    auto_refresh: true,
                    interval: Duration::from_secs(15),
                },
                cx,
            );
        });
        *fixture.monotonic_now.lock() += Duration::from_secs(60);
        cx.background_executor
            .advance_clock(Duration::from_secs(60));
        settle(cx);
        assert_eq!(collector_a.fetch_count.load(Ordering::SeqCst), 3);
    }

    #[gpui::test]
    async fn deduplicates_account_resolution_for_the_same_target(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        let fixture = fixture(cx, collector.clone());
        let (release, gate) = oneshot::channel();
        collector.set_resolve_gate(gate);
        let target = target("provider-a", "model-a");

        fixture.store.update(cx, |store, cx| {
            store.refresh(target.clone(), false, cx);
            store.refresh(target, false, cx);
        });
        settle(cx);
        assert_eq!(collector.resolve_count.load(Ordering::SeqCst), 1);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 0);

        release.send(()).ok();
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);
    }

    #[gpui::test]
    async fn deduplicates_fetch_after_two_targets_resolve_to_one_account_key(
        cx: &mut TestAppContext,
    ) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        let fixture = fixture(cx, collector.clone());
        let (release, gate) = oneshot::channel();
        collector.set_fetch_gate(gate);
        let model_a = target("provider-a", "model-a");
        let model_b = target("provider-a", "model-b");

        fixture.store.update(cx, |store, cx| {
            store.refresh(model_a, false, cx);
            store.refresh(model_b, false, cx);
        });
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);

        release.send(()).ok();
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);
    }

    #[gpui::test]
    async fn exposes_resolution_failure_before_a_cache_key_exists(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.set_resolve_error(QuotaError::MissingCredentials);
        let fixture = fixture(cx, collector);
        let target = target("provider-a", "model-a");
        fixture.store.update(cx, |store, cx| {
            store.refresh(target.clone(), false, cx);
        });
        settle(cx);

        fixture.store.update(cx, |store, _| {
            let view = store.view(&target).unwrap();
            assert!(view.snapshot.is_none());
            assert!(!view.is_fetching);
            assert_eq!(view.error, Some(QuotaError::MissingCredentials));
        });
    }

    #[gpui::test]
    async fn resolved_account_survives_initial_provider_failure(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.push_response(FakeQuotaResponse::ProviderError("provider failed"));
        let fixture = fixture(cx, collector.clone());
        let target = target("provider-a", "model-a");

        fixture.store.update(cx, |store, cx| {
            store.refresh(target.clone(), false, cx);
        });
        settle(cx);

        fixture.store.update(cx, |store, _| {
            let view = store.view(&target).unwrap();
            assert_eq!(view.account, Some(collector.account.summary()));
            assert!(view.snapshot.is_none());
            assert_eq!(
                view.error,
                Some(QuotaError::Provider("provider failed".into()))
            );
        });
    }

    #[gpui::test]
    async fn keeps_cached_snapshot_and_last_updated_after_fetch_failure(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        collector.push_response(FakeQuotaResponse::ProviderError("provider failed"));
        let fixture = fixture(cx, collector);
        let target = target("provider-a", "model-a");

        fixture.store.update(cx, |store, cx| {
            store.refresh(target.clone(), false, cx);
        });
        settle(cx);
        let first_fetched_at = fixture.store.update(cx, |store, _| {
            store
                .view(&target)
                .and_then(|view| view.snapshot.map(|snapshot| snapshot.fetched_at_unix_ms))
                .unwrap_or_default()
        });

        fixture.store.update(cx, |store, cx| {
            store.refresh(target.clone(), true, cx);
        });
        settle(cx);
        fixture.store.update(cx, |store, _| {
            let view = store.view(&target).unwrap();
            assert_eq!(
                view.snapshot.as_ref().unwrap().windows[0].remaining_percent,
                Some(72.0)
            );
            assert_eq!(
                view.snapshot.as_ref().unwrap().fetched_at_unix_ms,
                first_fetched_at
            );
            assert_eq!(
                view.error,
                Some(QuotaError::Provider("provider failed".into()))
            );
        });
    }

    #[gpui::test]
    async fn account_scope_reuses_cache_across_models(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        let fixture = fixture(cx, collector.clone());
        let model_a = target("provider-a", "model-a");
        let model_b = target("provider-a", "model-b");

        fixture.store.update(cx, |store, cx| {
            store.refresh(model_a, false, cx);
        });
        settle(cx);
        fixture.store.update(cx, |store, cx| {
            store.refresh(model_b.clone(), false, cx);
        });
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);
        assert!(fixture.store.update(cx, |store, _| {
            store.view(&model_b).unwrap().snapshot.is_some()
        }));
    }

    #[gpui::test]
    async fn account_cache_overlays_each_requesting_model(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a").with_normalized_model_ids();
        let mut quota_snapshot = snapshot(72.0);
        quota_snapshot.model_windows.insert(
            Arc::from("regular"),
            vec![QuotaWindow::percentage("regular", 80.0)],
        );
        quota_snapshot.model_windows.insert(
            Arc::from("spark"),
            vec![QuotaWindow::percentage("spark", 10.0)],
        );
        collector.push_response(FakeQuotaResponse::Snapshot(quota_snapshot));
        let fixture = fixture(cx, collector.clone());
        let regular_target = target("provider-a", "regular-v1");
        let spark_target = target("provider-a", "spark-v1");

        fixture.store.update(cx, |store, cx| {
            store.refresh(regular_target.clone(), false, cx);
        });
        settle(cx);
        fixture.store.update(cx, |store, cx| {
            store.refresh(spark_target.clone(), false, cx);
        });
        settle(cx);

        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);
        fixture.store.update(cx, |store, _| {
            let regular_view = store.view(&regular_target).unwrap();
            assert_eq!(
                regular_view
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.active_model_id.as_deref()),
                Some("regular")
            );
            assert_eq!(
                regular_view.snapshot.as_ref().map(QuotaSnapshot::severity),
                Some(Some(QuotaSeverity::Normal))
            );

            let spark_view = store.view(&spark_target).unwrap();
            assert_eq!(
                spark_view
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.active_model_id.as_deref()),
                Some("spark")
            );
            assert_eq!(
                spark_view.snapshot.as_ref().map(QuotaSnapshot::severity),
                Some(Some(QuotaSeverity::Critical))
            );
        });
    }

    #[gpui::test]
    async fn popover_targets_keep_active_collector_once(cx: &mut TestAppContext) {
        let active_collector = FakeCollector::new("active", "provider-a")
            .with_discovery_target(model_less_target("provider-a"));
        let other_target = model_less_target("provider-b");
        let other_collector =
            FakeCollector::new("other", "provider-b").with_discovery_target(other_target.clone());
        let fixture = fixture_with_collectors(cx, vec![active_collector, other_collector]);
        let active_target = target("provider-a", "model-a");

        let targets = fixture.store.update(cx, |store, _| {
            store.targets_for_popover(Some(&active_target))
        });

        assert_eq!(targets, vec![active_target, other_target]);
    }

    #[gpui::test]
    async fn rate_limit_sets_and_success_clears_cooldown(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.push_response(FakeQuotaResponse::RateLimited(Duration::from_secs(10)));
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        let fixture = fixture(cx, collector.clone());
        let target = target("provider-a", "model-a");

        fixture.store.update(cx, |store, cx| {
            store.refresh(target.clone(), true, cx);
        });
        settle(cx);
        fixture.store.update(cx, |store, cx| {
            store.refresh(target.clone(), true, cx);
        });
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);

        *fixture.monotonic_now.lock() += Duration::from_secs(10);
        cx.background_executor
            .advance_clock(Duration::from_secs(10));
        fixture.store.update(cx, |store, cx| {
            store.refresh(target.clone(), true, cx);
        });
        settle(cx);

        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 2);
        fixture.store.update(cx, |store, _| {
            let view = store.view(&target).unwrap();
            assert!(view.snapshot.is_some());
            assert_eq!(view.error, None);
            let cache_key = store.targets[&target].cache_key.as_ref().unwrap();
            assert_eq!(store.entries[cache_key].cooldown_until, None);
        });
    }

    #[gpui::test]
    async fn model_scope_separates_cache_entries(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a").with_model_scope();
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(41.0)));
        let fixture = fixture(cx, collector.clone());
        let model_a = target("provider-a", "model-a");
        let model_b = target("provider-a", "model-b");

        fixture.store.update(cx, |store, cx| {
            store.refresh(model_a.clone(), false, cx);
        });
        settle(cx);
        fixture.store.update(cx, |store, cx| {
            store.refresh(model_b.clone(), false, cx);
        });
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 2);
        fixture.store.update(cx, |store, _| {
            assert_eq!(store.entries.len(), 2);
        });
    }

    #[gpui::test]
    async fn collector_floor_overrides_refresh_policy_interval(cx: &mut TestAppContext) {
        let collector =
            FakeCollector::new("fake", "provider-a").with_minimum_ttl(Duration::from_secs(60));
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(41.0)));
        let fixture = fixture(cx, collector.clone());
        let target = target("provider-a", "model-a");

        fixture.store.update(cx, |store, cx| {
            store.set_refresh_policy(
                QuotaRefreshPolicy {
                    auto_refresh: true,
                    interval: Duration::from_secs(15),
                },
                cx,
            );
            store.activate(target, cx);
        });
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);
        cx.background_executor
            .advance_clock(Duration::from_secs(59));
        *fixture.monotonic_now.lock() += Duration::from_secs(59);
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);
        assert!(cx.dispatcher.advance_clock_to_next_timer());
        *fixture.monotonic_now.lock() += Duration::from_secs(1);
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn deactivated_target_is_not_rescheduled_after_in_flight_completion(
        cx: &mut TestAppContext,
    ) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(41.0)));
        let fixture = fixture(cx, collector.clone());
        let (release, gate) = oneshot::channel();
        collector.set_fetch_gate(gate);
        let target = target("provider-a", "model-a");

        fixture.store.update(cx, |store, cx| {
            store.set_refresh_policy(
                QuotaRefreshPolicy {
                    auto_refresh: true,
                    interval: Duration::from_secs(15),
                },
                cx,
            );
            store.activate(target.clone(), cx);
        });
        settle(cx);
        fixture.store.update(cx, |store, cx| {
            store.deactivate(&target, cx);
        });
        release.send(()).ok();
        settle(cx);
        cx.background_executor
            .advance_clock(Duration::from_secs(60));
        settle(cx);
        assert_eq!(collector.fetch_count.load(Ordering::SeqCst), 1);
    }

    #[gpui::test]
    async fn evicts_entries_unused_for_twenty_four_hours(cx: &mut TestAppContext) {
        let collector = FakeCollector::new("fake", "provider-a");
        collector.push_response(FakeQuotaResponse::Snapshot(snapshot(72.0)));
        let fixture = fixture(cx, collector);
        let target = target("provider-a", "model-a");
        fixture.store.update(cx, |store, cx| {
            store.refresh(target, false, cx);
        });
        settle(cx);

        let now = *fixture.monotonic_now.lock();
        fixture.store.update(cx, |store, _| {
            store.evict_unused(now + Duration::from_secs(24 * 60 * 60 + 1));
            assert!(store.entries.is_empty());
        });
    }
}
