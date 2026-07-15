mod codex;

use ai_usage::{QuotaTarget, QuotaTargetKind};
use fs::Fs;
use gpui::{App, SharedString};
use project::AgentId;
use std::sync::Arc;

pub fn canonical_runtime(agent_id: &str) -> Option<&'static str> {
    match agent_id {
        crate::CLAUDE_AGENT_ID => Some("claude-code"),
        crate::CODEX_ID => Some("codex"),
        crate::GEMINI_ID => Some("gemini-cli"),
        crate::CURSOR_ID => Some("cursor"),
        "opencode" => Some("opencode"),
        "antigravity" | "antigravity-cli" => Some("antigravity-cli"),
        _ => None,
    }
}

pub fn quota_target_for_agent(
    agent_id: &AgentId,
    upstream_provider_id: Option<&str>,
    model_id: Option<&str>,
    model_name: Option<SharedString>,
) -> QuotaTarget {
    QuotaTarget {
        kind: QuotaTargetKind::ExternalAgent,
        provider_or_agent_id: Arc::from(agent_id.as_ref()),
        upstream_provider_id: upstream_provider_id.map(Arc::from),
        model_id: model_id.map(Arc::from),
        model_name,
    }
}

pub(crate) fn register_collectors(fs: Arc<dyn Fs>, cx: &mut App) {
    codex::register(fs, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use project::AgentId;

    #[test]
    fn canonicalizes_known_agent_ids() {
        assert_eq!(canonical_runtime("claude-acp"), Some("claude-code"));
        assert_eq!(canonical_runtime("codex-acp"), Some("codex"));
        assert_eq!(canonical_runtime("gemini"), Some("gemini-cli"));
        assert_eq!(canonical_runtime("cursor"), Some("cursor"));
        assert_eq!(canonical_runtime("opencode"), Some("opencode"));
        assert_eq!(canonical_runtime("antigravity"), Some("antigravity-cli"));
        assert_eq!(canonical_runtime("unknown"), None);
    }

    #[test]
    fn external_target_preserves_explicit_upstream_provider() {
        let target = quota_target_for_agent(
            &AgentId::new("opencode"),
            Some("anthropic"),
            Some("claude-sonnet-4"),
            Some("Claude Sonnet 4".into()),
        );

        assert_eq!(target.upstream_provider_id.as_deref(), Some("anthropic"));
    }
}
