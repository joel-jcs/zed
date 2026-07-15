use crate::{QuotaCollector, QuotaTarget};
use std::sync::Arc;

#[derive(Default)]
pub struct QuotaRegistry {
    collectors: Vec<Arc<dyn QuotaCollector>>,
}

impl QuotaRegistry {
    pub fn register(&mut self, collector: Arc<dyn QuotaCollector>) {
        if let Some(index) = self
            .collectors
            .iter()
            .position(|existing| existing.id() == collector.id())
        {
            self.collectors[index] = collector;
        } else {
            self.collectors.push(collector);
        }
    }

    pub fn collector_for(&self, target: &QuotaTarget) -> Option<Arc<dyn QuotaCollector>> {
        self.collectors
            .iter()
            .find(|collector| collector.supports(target))
            .cloned()
    }

    pub fn registered_ids(&self) -> Vec<Arc<str>> {
        self.collectors
            .iter()
            .map(|collector| collector.id())
            .collect()
    }

    pub fn discovery_targets(&self) -> Vec<QuotaTarget> {
        self.collectors
            .iter()
            .filter_map(|collector| collector.discovery_target())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        QuotaAccount, QuotaCollector, QuotaError, QuotaFetchContext, QuotaSnapshot, QuotaTarget,
    };
    use futures::future::BoxFuture;
    use std::{sync::Arc, time::Duration};

    struct FakeCollector {
        id: &'static str,
        provider: &'static str,
        discovery: Option<QuotaTarget>,
    }

    impl QuotaCollector for FakeCollector {
        fn id(&self) -> Arc<str> {
            Arc::from(self.id)
        }

        fn display_name(&self) -> gpui::SharedString {
            self.id.into()
        }

        fn supports(&self, target: &QuotaTarget) -> bool {
            target.provider_or_agent_id.as_ref() == self.provider
        }

        fn discovery_target(&self) -> Option<QuotaTarget> {
            self.discovery.clone()
        }

        fn minimum_ttl(&self) -> Duration {
            Duration::from_secs(15)
        }

        fn cache_scope(
            &self,
            _target: &QuotaTarget,
            _account: &QuotaAccount,
        ) -> crate::QuotaCacheScope {
            crate::QuotaCacheScope::Account
        }

        fn resolve_account(
            &self,
            _target: QuotaTarget,
            _cx: &gpui::AsyncApp,
        ) -> BoxFuture<'static, Result<QuotaAccount, QuotaError>> {
            Box::pin(async { Ok(QuotaAccount::from_stable_identity("fake", b"account", None)) })
        }

        fn fetch(
            &self,
            _target: QuotaTarget,
            _account: QuotaAccount,
            _context: QuotaFetchContext,
            _cx: &gpui::AsyncApp,
        ) -> BoxFuture<'static, Result<QuotaSnapshot, QuotaError>> {
            Box::pin(async { Err(QuotaError::Provider("not used".into())) })
        }
    }

    #[test]
    fn resolves_only_the_matching_collector() {
        let mut registry = QuotaRegistry::default();
        registry.register(Arc::new(FakeCollector {
            id: "a",
            provider: "alpha",
            discovery: None,
        }));
        registry.register(Arc::new(FakeCollector {
            id: "b",
            provider: "beta",
            discovery: None,
        }));

        let target = QuotaTarget {
            kind: crate::QuotaTargetKind::NativeLanguageModel,
            provider_or_agent_id: Arc::from("beta"),
            upstream_provider_id: None,
            model_id: None,
            model_name: None,
        };

        assert_eq!(registry.collector_for(&target).unwrap().id().as_ref(), "b");
    }

    #[test]
    fn registry_returns_only_declared_discovery_targets() {
        let declared_target = QuotaTarget {
            kind: crate::QuotaTargetKind::NativeLanguageModel,
            provider_or_agent_id: Arc::from("alpha"),
            upstream_provider_id: None,
            model_id: None,
            model_name: None,
        };
        let mut registry = QuotaRegistry::default();
        registry.register(Arc::new(FakeCollector {
            id: "a",
            provider: "alpha",
            discovery: Some(declared_target.clone()),
        }));
        registry.register(Arc::new(FakeCollector {
            id: "b",
            provider: "beta",
            discovery: None,
        }));

        assert_eq!(registry.discovery_targets(), vec![declared_target]);
    }
}
