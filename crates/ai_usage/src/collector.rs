use crate::{QuotaAccount, QuotaError, QuotaSnapshot, QuotaTarget};
use futures::future::BoxFuture;
use gpui::{AsyncApp, SharedString};
use http_client::HttpClient;
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub struct QuotaFetchContext {
    pub http_client: Arc<dyn HttpClient>,
}

pub trait QuotaCollector: Send + Sync {
    fn id(&self) -> Arc<str>;
    fn display_name(&self) -> SharedString;
    fn supports(&self, target: &QuotaTarget) -> bool;
    fn minimum_ttl(&self) -> Duration;

    fn discovery_target(&self) -> Option<QuotaTarget> {
        None
    }

    fn model_match_id(&self, target: &QuotaTarget) -> Option<Arc<str>> {
        target.model_id.clone()
    }

    fn cache_scope(&self, target: &QuotaTarget, account: &QuotaAccount) -> crate::QuotaCacheScope;

    fn resolve_account(
        &self,
        target: QuotaTarget,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<QuotaAccount, QuotaError>>;

    fn fetch(
        &self,
        target: QuotaTarget,
        account: QuotaAccount,
        context: QuotaFetchContext,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<QuotaSnapshot, QuotaError>>;
}
