use std::sync::Arc;

use agent_servers::quota_target_for_agent;
use ai_usage::QuotaTarget;
use gpui::App;
use language_model::LanguageModel;
use project::AgentId;

use crate::ModelSelectorPopover;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveQuotaTarget(pub QuotaTarget);

impl ActiveQuotaTarget {
    pub(crate) fn from_language_model(model: Arc<dyn LanguageModel>) -> Self {
        Self(language_models::quota_target_for_model(&model))
    }

    pub(crate) fn from_external(
        agent_id: &AgentId,
        upstream_provider_id: Option<&str>,
        model_selector: Option<&ModelSelectorPopover>,
        cx: &App,
    ) -> Self {
        let model = model_selector.and_then(|selector| selector.active_model(cx));
        Self::from_external_model(
            agent_id,
            upstream_provider_id,
            model.map(|model| model.id.as_ref()),
            model.map(|model| model.name.clone()),
        )
    }

    fn from_external_model(
        agent_id: &AgentId,
        upstream_provider_id: Option<&str>,
        model_id: Option<&str>,
        model_name: Option<gpui::SharedString>,
    ) -> Self {
        Self(quota_target_for_agent(
            agent_id,
            upstream_provider_id,
            model_id,
            model_name,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::ContextQuotaIndicator;
    use futures::future::BoxFuture;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use http_client::FakeHttpClient;
    use language_model::fake_provider::FakeLanguageModel;
    use std::time::Duration;

    struct FakeCollector;

    impl ai_usage::QuotaCollector for FakeCollector {
        fn id(&self) -> Arc<str> {
            Arc::from("test-collector")
        }

        fn display_name(&self) -> gpui::SharedString {
            "Test collector".into()
        }

        fn supports(&self, target: &QuotaTarget) -> bool {
            target.provider_or_agent_id.as_ref() == "test-provider"
        }

        fn minimum_ttl(&self) -> Duration {
            Duration::from_secs(15)
        }

        fn cache_scope(
            &self,
            target: &QuotaTarget,
            _account: &ai_usage::QuotaAccount,
        ) -> ai_usage::QuotaCacheScope {
            ai_usage::QuotaCacheScope::Model(
                target
                    .model_id
                    .clone()
                    .unwrap_or_else(|| Arc::from("model-less")),
            )
        }

        fn resolve_account(
            &self,
            _target: QuotaTarget,
            _cx: &gpui::AsyncApp,
        ) -> BoxFuture<'static, Result<ai_usage::QuotaAccount, ai_usage::QuotaError>> {
            Box::pin(async {
                Ok(ai_usage::QuotaAccount::from_stable_identity(
                    "test", b"account", None,
                ))
            })
        }

        fn fetch(
            &self,
            target: QuotaTarget,
            _account: ai_usage::QuotaAccount,
            _context: ai_usage::QuotaFetchContext,
            _cx: &gpui::AsyncApp,
        ) -> BoxFuture<'static, Result<ai_usage::QuotaSnapshot, ai_usage::QuotaError>> {
            Box::pin(async move {
                Ok(ai_usage::QuotaSnapshot {
                    provider_id: Arc::from("test-provider"),
                    provider_name: "Test provider".into(),
                    account: ai_usage::QuotaAccountSummary {
                        fingerprint: Arc::from("account"),
                        safe_label: None,
                    },
                    plan: None,
                    fetched_at_unix_ms: 0,
                    active_model_id: target.model_id,
                    windows: Vec::new(),
                    model_windows: Default::default(),
                    groups: Vec::new(),
                    available_resets: None,
                })
            })
        }
    }

    fn native_model(id: &str) -> Arc<dyn LanguageModel> {
        Arc::new(FakeLanguageModel::with_id_and_thinking(
            "test-provider",
            id,
            id,
            false,
        ))
    }

    fn setup(cx: &mut TestAppContext) -> Option<Entity<ai_usage::QuotaStore>> {
        cx.update(|cx| {
            ai_usage::register_collector(Arc::new(FakeCollector), cx);
            ai_usage::init(FakeHttpClient::with_404_response(), cx);
            ai_usage::store(cx)
        })
    }

    #[gpui::test]
    async fn switching_native_models_retargets_unified_indicator(cx: &mut TestAppContext) {
        let Some(store) = setup(cx) else {
            assert!(false, "test quota store initialized");
            return;
        };
        let first = ActiveQuotaTarget::from_language_model(native_model("native-a"));
        let second = ActiveQuotaTarget::from_language_model(native_model("native-b"));
        let indicator = cx
            .update(|cx| cx.new(|cx| ContextQuotaIndicator::new(first.clone(), store.clone(), cx)));
        indicator.update(cx, |indicator, cx| indicator.set_target(second.clone(), cx));
        cx.run_until_parked();

        let snapshot = store.update(cx, |store, _| store.view(&second.0));
        let active_model_id = snapshot
            .and_then(|view| view.snapshot)
            .and_then(|snapshot| snapshot.active_model_id);
        assert_eq!(active_model_id.as_deref(), Some("native-b"));
    }

    #[gpui::test]
    async fn switching_external_models_retargets_unified_indicator(cx: &mut TestAppContext) {
        let Some(store) = setup(cx) else {
            assert!(false, "test quota store initialized");
            return;
        };
        let first = ActiveQuotaTarget::from_external_model(
            &AgentId::new("test-provider"),
            None,
            Some("external-a"),
            Some("External A".into()),
        );
        let second = ActiveQuotaTarget::from_external_model(
            &AgentId::new("test-provider"),
            None,
            Some("external-b"),
            Some("External B".into()),
        );
        let indicator =
            cx.update(|cx| cx.new(|cx| ContextQuotaIndicator::new(first, store.clone(), cx)));
        indicator.update(cx, |indicator, cx| indicator.set_target(second.clone(), cx));
        cx.run_until_parked();

        let snapshot = store.update(cx, |store, _| store.view(&second.0));
        let active_model_id = snapshot
            .and_then(|view| view.snapshot)
            .and_then(|snapshot| snapshot.active_model_id);
        assert_eq!(active_model_id.as_deref(), Some("external-b"));
        assert_eq!(second.0.upstream_provider_id, None);
    }

    #[gpui::test]
    async fn api_model_keeps_context_only_indicator(cx: &mut TestAppContext) {
        let Some(store) = setup(cx) else {
            assert!(false, "test quota store initialized");
            return;
        };
        let target = ActiveQuotaTarget::from_external_model(
            &AgentId::new("api-provider"),
            None,
            Some("api-model"),
            Some("API model".into()),
        );
        let indicator = cx.update(|cx| {
            cx.new(|cx| ContextQuotaIndicator::new(target.clone(), store.clone(), cx))
        });
        cx.run_until_parked();

        assert!(indicator.read_with(cx, |_, _| true));
        assert!(store.update(cx, |store, _| store.view(&target.0)).is_none());
    }
}
