use ai_usage::{
    CodexQuotaSource, QuotaAccount, QuotaCacheScope, QuotaCollector, QuotaError, QuotaFetchContext,
    QuotaTarget, QuotaTargetKind,
};
use fs::Fs;
use futures::future::BoxFuture;
use gpui::{AsyncApp, SharedString};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};

const COLLECTOR_ID: &str = "codex-acp";
const PROVIDER_ID: &str = "codex";
const PROVIDER_NAME: &str = "Codex CLI";
const AUTHENTICATION_ERROR: &str = "Codex CLI session expired; re-authenticate Codex";

pub(crate) struct CodexQuotaCollector {
    fs: Arc<dyn Fs>,
}

impl CodexQuotaCollector {
    pub(crate) fn new(fs: Arc<dyn Fs>) -> Self {
        Self { fs }
    }
}

struct CodexAuth {
    access_token: String,
    account_id: Option<String>,
}

impl CodexAuth {
    fn account(&self) -> QuotaAccount {
        let identity = self.account_id.as_deref().unwrap_or(&self.access_token);
        QuotaAccount::from_stable_identity(COLLECTOR_ID, identity.as_bytes(), None)
    }
}

impl QuotaCollector for CodexQuotaCollector {
    fn id(&self) -> Arc<str> {
        Arc::from(COLLECTOR_ID)
    }

    fn quota_family_id(&self) -> Arc<str> {
        Arc::from("openai-codex")
    }

    fn display_name(&self) -> SharedString {
        PROVIDER_NAME.into()
    }

    fn supports(&self, target: &QuotaTarget) -> bool {
        supports_target(target)
    }

    fn minimum_ttl(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn discovery_target(&self) -> Option<QuotaTarget> {
        Some(discovery_target())
    }

    fn model_match_id(&self, target: &QuotaTarget) -> Option<Arc<str>> {
        target
            .model_id
            .as_deref()
            .map(ai_usage::normalize_codex_model_id)
    }

    fn cache_scope(&self, _target: &QuotaTarget, _account: &QuotaAccount) -> QuotaCacheScope {
        QuotaCacheScope::Account
    }

    fn resolve_account(
        &self,
        _target: QuotaTarget,
        _cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<QuotaAccount, QuotaError>> {
        let fs = self.fs.clone();
        Box::pin(async move { load_auth(fs.as_ref()).await.map(|auth| auth.account()) })
    }

    fn fetch(
        &self,
        target: QuotaTarget,
        account: QuotaAccount,
        context: QuotaFetchContext,
        _cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<ai_usage::QuotaSnapshot, QuotaError>> {
        let fs = self.fs.clone();
        Box::pin(async move {
            let auth = load_auth(fs.as_ref()).await?;
            if auth.account() != account {
                return Err(QuotaError::AmbiguousAccount);
            }
            ai_usage::fetch_codex_snapshot(
                &context.http_client,
                &auth.access_token,
                auth.account_id.as_deref(),
                &target,
                CodexQuotaSource {
                    provider_id: Arc::from(PROVIDER_ID),
                    provider_name: PROVIDER_NAME.into(),
                    authentication_error: AUTHENTICATION_ERROR.into(),
                },
            )
            .await
        })
    }
}

pub(crate) fn register(fs: Arc<dyn Fs>, cx: &mut gpui::App) {
    ai_usage::register_collector(Arc::new(CodexQuotaCollector::new(fs)), cx);
}

fn supports_target(target: &QuotaTarget) -> bool {
    target.kind == QuotaTargetKind::ExternalAgent
        && target.provider_or_agent_id.as_ref() == COLLECTOR_ID
}

fn discovery_target() -> QuotaTarget {
    QuotaTarget {
        kind: QuotaTargetKind::ExternalAgent,
        provider_or_agent_id: Arc::from(COLLECTOR_ID),
        upstream_provider_id: None,
        model_id: None,
        model_name: None,
    }
}

async fn load_auth(fs: &dyn Fs) -> Result<CodexAuth, QuotaError> {
    for path in auth_paths() {
        let Ok(contents) = fs.load(&path).await else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&contents) else {
            continue;
        };
        if let Some(auth) = parse_auth(&value) {
            return Ok(auth);
        }
    }
    Err(QuotaError::MissingCredentials)
}

fn auth_paths() -> Vec<PathBuf> {
    let fallback = util::paths::home_dir().join(".codex").join("auth.json");
    let Some(codex_home) = std::env::var_os("CODEX_HOME") else {
        return vec![fallback];
    };
    if codex_home.is_empty() {
        return vec![fallback];
    }
    let configured = PathBuf::from(codex_home).join("auth.json");
    if configured == fallback {
        vec![fallback]
    } else {
        vec![configured, fallback]
    }
}

fn parse_auth(value: &Value) -> Option<CodexAuth> {
    let tokens = value.get("tokens");
    let access_token = first_string([
        tokens.and_then(|tokens| tokens.get("access_token")),
        tokens.and_then(|tokens| tokens.get("accessToken")),
        value.get("access_token"),
        value.get("accessToken"),
        value.get("token"),
    ])?;
    let account_id = first_string([
        value.get("account_id"),
        value.get("accountId"),
        tokens.and_then(|tokens| tokens.get("account_id")),
        tokens.and_then(|tokens| tokens.get("accountId")),
    ]);
    Some(CodexAuth {
        access_token,
        account_id,
    })
}

fn first_string<const N: usize>(values: [Option<&Value>; N]) -> Option<String> {
    values.into_iter().find_map(|value| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cli_account_prefers_account_id_then_token_fingerprint() {
        let account_id = "account-id-secret";
        let token = "access-token-secret";
        let account = parse_auth(&json!({
            "tokens": { "access_token": token },
            "account_id": account_id,
        }))
        .expect("auth should parse")
        .account();
        let expected_account =
            QuotaAccount::from_stable_identity(COLLECTOR_ID, account_id.as_bytes(), None);
        assert_eq!(account, expected_account);
        let debug = format!("{account:?}");
        assert!(!debug.contains(account_id));
        assert!(!debug.contains(token));

        let token_account = parse_auth(&json!({
            "tokens": { "accessToken": token },
        }))
        .expect("auth should parse")
        .account();
        let expected_token_account =
            QuotaAccount::from_stable_identity(COLLECTOR_ID, token.as_bytes(), None);
        assert_eq!(token_account, expected_token_account);
        let debug = format!("{token_account:?}");
        assert!(!debug.contains(token));
    }

    #[test]
    fn both_collectors_declare_account_wide_discovery_targets() {
        let target = discovery_target();
        assert_eq!(target.kind, QuotaTargetKind::ExternalAgent);
        assert_eq!(target.provider_or_agent_id.as_ref(), COLLECTOR_ID);
        assert_eq!(target.model_id, None);
        assert_eq!(target.upstream_provider_id, None);
    }

    #[test]
    fn opencode_without_upstream_provider_is_not_supported() {
        let target = QuotaTarget {
            kind: QuotaTargetKind::ExternalAgent,
            provider_or_agent_id: Arc::from("opencode"),
            upstream_provider_id: None,
            model_id: Some(Arc::from("gpt-5.5")),
            model_name: Some("GPT-5.5".into()),
        };
        assert!(!supports_target(&target));
    }

    #[test]
    fn codex_collector_uses_shared_model_normalizer() {
        assert_eq!(
            ai_usage::normalize_codex_model_id("GPT-5.3-Codex-Spark").as_ref(),
            "gpt53codexspark"
        );
    }
}
