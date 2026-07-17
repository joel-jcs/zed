use collections::HashMap;
use gpui::SharedString;
use sha2::{Digest, Sha256};
use std::{hash::Hash, sync::Arc, time::Duration};

pub const MINIMUM_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaRefreshPolicy {
    pub auto_refresh: bool,
    pub interval: Duration,
}

impl Default for QuotaRefreshPolicy {
    fn default() -> Self {
        Self {
            auto_refresh: false,
            interval: Duration::from_secs(30),
        }
    }
}

impl QuotaRefreshPolicy {
    pub fn normalized(self) -> Self {
        Self {
            auto_refresh: self.auto_refresh,
            interval: self.interval.max(MINIMUM_REFRESH_INTERVAL),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum QuotaTargetKind {
    NativeLanguageModel,
    ExternalAgent,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct QuotaTarget {
    pub kind: QuotaTargetKind,
    pub provider_or_agent_id: Arc<str>,
    pub upstream_provider_id: Option<Arc<str>>,
    pub model_id: Option<Arc<str>>,
    pub model_name: Option<SharedString>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct QuotaAccount {
    pub credential_source: Arc<str>,
    pub fingerprint: Arc<str>,
    pub safe_label: Option<SharedString>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct QuotaAccountSummary {
    pub fingerprint: Arc<str>,
    pub safe_label: Option<SharedString>,
}

impl QuotaAccount {
    pub fn from_stable_identity(
        credential_source: impl Into<Arc<str>>,
        stable_identity: &[u8],
        safe_label: Option<SharedString>,
    ) -> Self {
        let digest = Sha256::digest(stable_identity);
        let fingerprint = format!("{:x}", digest);
        Self {
            credential_source: credential_source.into(),
            fingerprint: Arc::from(&fingerprint[..16]),
            safe_label,
        }
    }

    pub fn summary(&self) -> QuotaAccountSummary {
        QuotaAccountSummary {
            fingerprint: self.fingerprint.clone(),
            safe_label: self.safe_label.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum QuotaCacheScope {
    Account,
    Subscription(Arc<str>),
    ModelFamily(Arc<str>),
    Model(Arc<str>),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct QuotaCacheKey {
    pub collector_id: Arc<str>,
    pub credential_source: Arc<str>,
    pub account_fingerprint: Arc<str>,
    pub provider_or_agent_id: Arc<str>,
    pub scope: QuotaCacheScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaSeverity {
    Normal,
    Warning,
    Critical,
}

impl QuotaSeverity {
    pub fn from_remaining_percent(value: f64) -> Self {
        if value < 20.0 {
            Self::Critical
        } else if value <= 50.0 {
            Self::Warning
        } else {
            Self::Normal
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaWindow {
    pub label: SharedString,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub window_seconds: Option<u64>,
    pub reset_after_seconds: Option<u64>,
    pub reset_at_unix_ms: Option<i64>,
    pub value_label: Option<SharedString>,
    pub compact_priority: Option<u16>,
}

impl QuotaWindow {
    pub fn percentage(label: impl Into<SharedString>, remaining_percent: f64) -> Self {
        let remaining_percent = remaining_percent.clamp(0.0, 100.0);
        Self {
            label: label.into(),
            used_percent: Some(100.0 - remaining_percent),
            remaining_percent: Some(remaining_percent),
            window_seconds: None,
            reset_after_seconds: None,
            reset_at_unix_ms: None,
            value_label: None,
            compact_priority: None,
        }
    }

    pub fn value(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            used_percent: None,
            remaining_percent: None,
            window_seconds: None,
            reset_after_seconds: None,
            reset_at_unix_ms: None,
            value_label: Some(value.into()),
            compact_priority: None,
        }
    }

    pub fn with_window_seconds(mut self, value: u64) -> Self {
        self.window_seconds = Some(value);
        self
    }

    pub fn with_reset_after_seconds(mut self, value: u64) -> Self {
        self.reset_after_seconds = Some(value);
        self
    }

    pub fn with_reset_at_unix_ms(mut self, value: i64) -> Self {
        self.reset_at_unix_ms = Some(value);
        self
    }

    pub fn with_value_label(mut self, value: impl Into<SharedString>) -> Self {
        self.value_label = Some(value.into());
        self
    }

    pub fn with_compact_priority(mut self, value: u16) -> Self {
        self.compact_priority = Some(value);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaReset {
    pub title: SharedString,
    pub expires_at_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaResetSummary {
    pub available_count: u64,
    pub resets: Vec<QuotaReset>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaBucket {
    pub id: Arc<str>,
    pub display_name: SharedString,
    pub description: Option<SharedString>,
    pub window: QuotaWindow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaGroup {
    pub id: Arc<str>,
    pub display_name: SharedString,
    pub description: Option<SharedString>,
    /// Empty means provider/account-wide. Otherwise this group contributes to
    /// active severity only when the active model ID is present.
    pub applies_to_model_ids: Vec<Arc<str>>,
    pub affects_severity: bool,
    pub buckets: Vec<QuotaBucket>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaSnapshot {
    pub provider_id: Arc<str>,
    pub provider_name: SharedString,
    pub account: QuotaAccountSummary,
    pub plan: Option<SharedString>,
    pub fetched_at_unix_ms: i64,
    pub active_model_id: Option<Arc<str>>,
    pub windows: Vec<QuotaWindow>,
    pub model_windows: HashMap<Arc<str>, Vec<QuotaWindow>>,
    pub groups: Vec<QuotaGroup>,
    pub available_resets: Option<QuotaResetSummary>,
}

impl QuotaSnapshot {
    pub fn applicable_windows(&self) -> Vec<&QuotaWindow> {
        let mut windows = self.windows.iter().collect::<Vec<_>>();

        if let Some(active_model_id) = self.active_model_id.as_ref()
            && let Some(model_windows) = self.model_windows.get(active_model_id)
        {
            windows.extend(model_windows.iter());
        }

        for group in self.groups.iter().filter(|group| {
            group.affects_severity
                && (group.applies_to_model_ids.is_empty()
                    || self
                        .active_model_id
                        .as_ref()
                        .is_some_and(|active| group.applies_to_model_ids.contains(active)))
        }) {
            windows.extend(group.buckets.iter().map(|bucket| &bucket.window));
        }

        windows
    }

    pub fn severity(&self) -> Option<QuotaSeverity> {
        self.applicable_windows()
            .iter()
            .filter_map(|window| window.remaining_percent)
            .reduce(f64::min)
            .map(QuotaSeverity::from_remaining_percent)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum QuotaError {
    #[error("quota credentials are not configured")]
    MissingCredentials,
    #[error("the active quota account could not be identified")]
    AmbiguousAccount,
    #[error("quota refresh is cooling down")]
    Cooldown,
    #[error("{0}")]
    Authentication(SharedString),
    #[error("{message}")]
    RateLimited {
        message: SharedString,
        retry_after: Duration,
    },
    #[error("{0}")]
    Provider(SharedString),
}

impl QuotaError {
    pub fn disconnects_provider(&self) -> bool {
        matches!(
            self,
            Self::MissingCredentials | Self::AmbiguousAccount | Self::Authentication(_)
        )
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => Some(*retry_after),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaView {
    pub provider_id: Arc<str>,
    pub quota_family_id: Arc<str>,
    pub provider_name: SharedString,
    pub account: Option<QuotaAccountSummary>,
    pub snapshot: Option<QuotaSnapshot>,
    pub is_fetching: bool,
    pub error: Option<QuotaError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use collections::HashMap;
    use std::sync::Arc;

    fn window(label: &str, remaining: f64, priority: u16) -> QuotaWindow {
        QuotaWindow::percentage(label, remaining)
            .with_window_seconds(18_000)
            .with_compact_priority(priority)
    }

    fn base_snapshot(active_model_id: Option<&str>) -> QuotaSnapshot {
        QuotaSnapshot {
            provider_id: Arc::from("test"),
            provider_name: "Test".into(),
            account: QuotaAccountSummary {
                fingerprint: Arc::from("account-1"),
                safe_label: Some("user@example.test".into()),
            },
            plan: None,
            fetched_at_unix_ms: 1_700_000_000_000,
            active_model_id: active_model_id.map(Arc::from),
            windows: Vec::new(),
            model_windows: HashMap::default(),
            groups: Vec::new(),
            available_resets: None,
        }
    }

    #[test]
    fn applicable_windows_exclude_non_severity_groups_and_inactive_models() {
        let mut snapshot = base_snapshot(Some("active"));
        snapshot.windows = vec![window("main-window", 80.0, 0)];
        snapshot
            .model_windows
            .insert(Arc::from("active"), vec![window("active-model", 70.0, 1)]);
        snapshot.model_windows.insert(
            Arc::from("inactive"),
            vec![window("inactive-model", 5.0, 2)],
        );
        snapshot.groups = vec![
            QuotaGroup {
                id: Arc::from("main"),
                display_name: "Main".into(),
                description: None,
                applies_to_model_ids: Vec::new(),
                affects_severity: true,
                buckets: vec![QuotaBucket {
                    id: Arc::from("main-window"),
                    display_name: "Main".into(),
                    description: None,
                    window: window("main-group", 60.0, 3),
                }],
            },
            QuotaGroup {
                id: Arc::from("active"),
                display_name: "Active".into(),
                description: None,
                applies_to_model_ids: vec![Arc::from("active")],
                affects_severity: true,
                buckets: vec![QuotaBucket {
                    id: Arc::from("active-window"),
                    display_name: "Active".into(),
                    description: None,
                    window: window("active-group", 50.0, 4),
                }],
            },
            QuotaGroup {
                id: Arc::from("inactive"),
                display_name: "Inactive".into(),
                description: None,
                applies_to_model_ids: vec![Arc::from("inactive")],
                affects_severity: true,
                buckets: vec![QuotaBucket {
                    id: Arc::from("inactive-window"),
                    display_name: "Inactive".into(),
                    description: None,
                    window: window("inactive-group", 1.0, 5),
                }],
            },
            QuotaGroup {
                id: Arc::from("code-review"),
                display_name: "Code review".into(),
                description: None,
                applies_to_model_ids: Vec::new(),
                affects_severity: false,
                buckets: vec![QuotaBucket {
                    id: Arc::from("code-review-window"),
                    display_name: "Code review".into(),
                    description: None,
                    window: window("code-review-group", 1.0, 6),
                }],
            },
        ];

        let applicable_windows: Vec<&QuotaWindow> = snapshot.applicable_windows();
        let applicable_labels = applicable_windows
            .into_iter()
            .map(|window| window.label.to_string())
            .collect::<Vec<_>>();
        assert!(applicable_labels.contains(&"main-window".to_string()));
        assert!(applicable_labels.contains(&"active-model".to_string()));
        assert!(applicable_labels.contains(&"main-group".to_string()));
        assert!(applicable_labels.contains(&"active-group".to_string()));
        assert!(!applicable_labels.contains(&"inactive-model".to_string()));
        assert!(!applicable_labels.contains(&"inactive-group".to_string()));
        assert!(!applicable_labels.contains(&"code-review-group".to_string()));
        assert_eq!(snapshot.groups.len(), 4);
    }

    #[test]
    fn reset_summary_contains_only_safe_display_fields() {
        let summary = QuotaResetSummary {
            available_count: 3,
            resets: vec![QuotaReset {
                title: "Primary quota".into(),
                expires_at_unix_ms: Some(1_700_000_000_000),
            }],
        };

        let normalized = format!("{summary:?}");
        assert!(normalized.contains("available_count: 3"));
        assert!(normalized.contains("Primary quota"));
        assert!(normalized.contains("1700000000000"));
        assert!(!normalized.contains("provider_id"));
    }

    #[test]
    fn rate_limit_error_exposes_cooldown_without_provider_payload() {
        let error = QuotaError::RateLimited {
            message: "Quota refresh is temporarily rate limited".into(),
            retry_after: Duration::from_secs(30),
        };

        assert_eq!(error.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(
            error.to_string(),
            "Quota refresh is temporarily rate limited"
        );
        assert!(!error.to_string().contains("fake provider body"));
    }

    #[test]
    fn severity_uses_remaining_percentage_thresholds() {
        assert_eq!(
            QuotaSeverity::from_remaining_percent(51.0),
            QuotaSeverity::Normal
        );
        assert_eq!(
            QuotaSeverity::from_remaining_percent(50.0),
            QuotaSeverity::Warning
        );
        assert_eq!(
            QuotaSeverity::from_remaining_percent(20.0),
            QuotaSeverity::Warning
        );
        assert_eq!(
            QuotaSeverity::from_remaining_percent(19.99),
            QuotaSeverity::Critical
        );
    }

    #[test]
    fn inactive_model_does_not_change_active_severity() {
        let mut snapshot = base_snapshot(Some("active"));
        snapshot
            .model_windows
            .insert(Arc::from("active"), vec![window("5h", 70.0, 0)]);
        snapshot
            .model_windows
            .insert(Arc::from("inactive"), vec![window("5h", 2.0, 0)]);

        assert_eq!(snapshot.severity(), Some(QuotaSeverity::Normal));
    }

    #[test]
    fn only_applicable_groups_change_active_severity() {
        let mut snapshot = base_snapshot(Some("gemini-pro"));
        snapshot.groups = vec![
            QuotaGroup {
                id: Arc::from("gemini"),
                display_name: "Gemini Models".into(),
                description: None,
                applies_to_model_ids: vec![Arc::from("gemini-pro")],
                affects_severity: true,
                buckets: vec![QuotaBucket {
                    id: Arc::from("gemini-weekly"),
                    display_name: "Weekly".into(),
                    description: None,
                    window: window("weekly", 35.0, 1),
                }],
            },
            QuotaGroup {
                id: Arc::from("claude-chatgpt"),
                display_name: "Claude & ChatGPT Models".into(),
                description: None,
                applies_to_model_ids: vec![Arc::from("claude-opus")],
                affects_severity: true,
                buckets: vec![QuotaBucket {
                    id: Arc::from("other-weekly"),
                    display_name: "Weekly".into(),
                    description: None,
                    window: window("weekly", 1.0, 1),
                }],
            },
        ];

        assert_eq!(snapshot.severity(), Some(QuotaSeverity::Warning));
    }

    #[test]
    fn window_preserves_duration_reset_and_value_semantics() {
        let window = QuotaWindow::percentage("5h", 72.0)
            .with_window_seconds(18_000)
            .with_reset_after_seconds(7_200)
            .with_reset_at_unix_ms(1_700_007_200_000)
            .with_value_label("72 requests left");

        assert_eq!(window.used_percent, Some(28.0));
        assert_eq!(window.remaining_percent, Some(72.0));
        assert_eq!(window.window_seconds, Some(18_000));
        assert_eq!(window.reset_after_seconds, Some(7_200));
        assert_eq!(window.reset_at_unix_ms, Some(1_700_007_200_000));
        assert_eq!(window.value_label.as_deref(), Some("72 requests left"));
    }
}
