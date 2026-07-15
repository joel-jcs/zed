use crate::{
    QuotaAccountSummary, QuotaBucket, QuotaError, QuotaGroup, QuotaSnapshot, QuotaTarget,
    QuotaWindow,
};
use futures::AsyncReadExt;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, Method, Request, StatusCode};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const RESET_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";

#[derive(Clone)]
pub struct CodexQuotaSource {
    pub provider_id: Arc<str>,
    pub provider_name: SharedString,
    pub authentication_error: SharedString,
}

pub async fn fetch_codex_snapshot(
    http_client: &Arc<dyn HttpClient>,
    access_token: &str,
    account_id: Option<&str>,
    target: &QuotaTarget,
    source: CodexQuotaSource,
) -> Result<QuotaSnapshot, QuotaError> {
    let payload = send_json(
        http_client,
        USAGE_URL,
        access_token,
        account_id,
        false,
        &source,
    )
    .await?;
    let mut snapshot = parse_snapshot(&payload, target, &source)?;

    if let Ok(reset_payload) = send_json(
        http_client,
        RESET_CREDITS_URL,
        access_token,
        account_id,
        true,
        &source,
    )
    .await
        && let Some(available_resets) = parse_reset_summary(&reset_payload)
    {
        snapshot.available_resets = Some(available_resets);
    }

    Ok(snapshot)
}

async fn send_json(
    http_client: &Arc<dyn HttpClient>,
    url: &str,
    access_token: &str,
    account_id: Option<&str>,
    reset_request: bool,
    source: &CodexQuotaSource,
) -> Result<Value, QuotaError> {
    let account_header = if reset_request {
        "ChatGPT-Account-ID"
    } else {
        "ChatGPT-Account-Id"
    };
    let mut request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json");
    if let Some(account_id) = account_id {
        request = request.header(account_header, account_id);
    }
    if reset_request {
        request = request
            .header("OpenAI-Beta", "codex-1")
            .header("originator", "Codex Desktop");
    }
    let request = request
        .body(AsyncBody::empty())
        .map_err(|_| QuotaError::Provider("could not build Codex quota request".into()))?;
    let mut response = http_client
        .send(request)
        .await
        .map_err(|_| QuotaError::Provider("Codex quota request failed".into()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(status_error(status, source));
    }
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .map_err(|_| QuotaError::Provider("could not read Codex quota response".into()))?;
    serde_json::from_str(&body)
        .map_err(|_| QuotaError::Provider("Codex quota response was malformed".into()))
}

fn status_error(status: StatusCode, source: &CodexQuotaSource) -> QuotaError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            QuotaError::Authentication(source.authentication_error.clone())
        }
        StatusCode::TOO_MANY_REQUESTS => QuotaError::RateLimited {
            message: "Codex quota provider returned HTTP 429".into(),
            retry_after: Duration::from_secs(30),
        },
        status => QuotaError::Provider(
            format!("Codex quota provider returned HTTP {}", status.as_u16()).into(),
        ),
    }
}

fn parse_snapshot(
    payload: &Value,
    target: &QuotaTarget,
    source: &CodexQuotaSource,
) -> Result<QuotaSnapshot, QuotaError> {
    let rate_limit = payload
        .get("rate_limit")
        .or_else(|| payload.get("rateLimit"));
    let mut windows = rate_limit.map(parse_rate_limit_windows).unwrap_or_default();

    if let Some(credits) = payload
        .get("credits")
        .or_else(|| rate_limit.and_then(|value| value.get("credits")))
        && let Some(label) = credits_label(credits)
    {
        windows.push(QuotaWindow::value("Credits balance", label));
    }

    let active_model_id = target.model_id.as_deref().map(normalize_codex_model_id);
    let mut groups = parse_additional_groups(payload, rate_limit);
    if let Some(code_review_rate_limit) = payload
        .get("code_review_rate_limit")
        .or_else(|| payload.get("codeReviewRateLimit"))
        .filter(|value| !value.is_null())
    {
        let buckets = parse_rate_limit_buckets(code_review_rate_limit);
        if !buckets.is_empty() {
            groups.push(QuotaGroup {
                id: Arc::from("code_review"),
                display_name: "Code review".into(),
                description: None,
                applies_to_model_ids: Vec::new(),
                affects_severity: false,
                buckets,
            });
        }
    }

    if windows.is_empty() && groups.is_empty() {
        return Err(QuotaError::Provider(
            "Codex quota response contained no recognizable quota data".into(),
        ));
    }

    Ok(QuotaSnapshot {
        provider_id: source.provider_id.clone(),
        provider_name: source.provider_name.clone(),
        account: QuotaAccountSummary {
            fingerprint: Arc::from("unassigned"),
            safe_label: None,
        },
        plan: first_string(payload, rate_limit, &["plan_type", "planType"]).map(Into::into),
        fetched_at_unix_ms: 0,
        active_model_id,
        windows,
        model_windows: Default::default(),
        groups,
        available_resets: None,
    })
}

fn parse_rate_limit_windows(rate_limit: &Value) -> Vec<QuotaWindow> {
    [
        ("primary_window", "primary", 0),
        ("secondary_window", "secondary", 1),
    ]
    .into_iter()
    .filter_map(|(key, fallback_label, priority)| {
        rate_limit
            .get(key)
            .or_else(|| rate_limit.get(fallback_label))
            .and_then(|value| parse_window(value, fallback_label, priority))
    })
    .collect()
}

fn parse_rate_limit_buckets(rate_limit: &Value) -> Vec<QuotaBucket> {
    parse_rate_limit_windows(rate_limit)
        .into_iter()
        .map(|window| QuotaBucket {
            id: Arc::from(window.label.as_ref()),
            display_name: window.label.clone(),
            description: None,
            window,
        })
        .collect()
}

fn parse_additional_groups(payload: &Value, rate_limit: Option<&Value>) -> Vec<QuotaGroup> {
    let entries = payload
        .get("additional_rate_limits")
        .or_else(|| payload.get("additionalRateLimits"))
        .or_else(|| rate_limit.and_then(|value| value.get("additional_rate_limits")))
        .or_else(|| rate_limit.and_then(|value| value.get("additionalRateLimits")))
        .and_then(Value::as_array);
    let Some(entries) = entries else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|entry| {
            let rate_limit = entry
                .get("rate_limit")
                .or_else(|| entry.get("rateLimit"))
                .unwrap_or(entry);
            let buckets = parse_rate_limit_buckets(rate_limit);
            if buckets.is_empty() {
                return None;
            }
            let limit_name = string(entry, &["limit_name", "limitName"])
                .unwrap_or_else(|| "Additional quota".to_string());
            let metered_feature = string(entry, &["metered_feature", "meteredFeature"])
                .unwrap_or_else(|| limit_name.clone());
            Some(QuotaGroup {
                id: Arc::from(metered_feature),
                display_name: limit_name.clone().into(),
                description: None,
                applies_to_model_ids: vec![normalize_codex_model_id(&limit_name)],
                affects_severity: true,
                buckets,
            })
        })
        .collect()
}

fn parse_window(value: &Value, fallback_label: &str, priority: u16) -> Option<QuotaWindow> {
    let used_percent = number(value, &["used_percent", "usedPercent"])?;
    let window_seconds = number(
        value,
        &["limit_window_seconds", "window_seconds", "windowSeconds"],
    )
    .and_then(to_u64)
    .or_else(|| {
        number(value, &["window_duration_mins", "windowDurationMins"])
            .and_then(to_u64)
            .map(|minutes| minutes.saturating_mul(60))
    });
    let label = window_label(window_seconds, fallback_label);
    let remaining_percent = (100.0 - used_percent).clamp(0.0, 100.0);
    let mut window = QuotaWindow::percentage(label, remaining_percent)
        .with_compact_priority(priority_for_window(window_seconds, priority));
    if let Some(window_seconds) = window_seconds {
        window = window.with_window_seconds(window_seconds);
    }
    if let Some(reset_at) = first_value(value, &["reset_at", "resetAt", "resets_at", "resetsAt"])
        .and_then(timestamp_to_millis)
    {
        window = window.with_reset_at_unix_ms(reset_at);
    } else if let Some(reset_after_seconds) =
        number(value, &["reset_after_seconds", "resetAfterSeconds"]).and_then(to_u64)
    {
        window = window.with_reset_after_seconds(reset_after_seconds);
    }
    Some(window)
}

fn parse_reset_summary(payload: &Value) -> Option<crate::QuotaResetSummary> {
    let credits = payload
        .get("credits")
        .or_else(|| payload.get("resets"))
        .or_else(|| payload.get("reset_credits"))
        .and_then(Value::as_array);
    let available_resets = credits
        .into_iter()
        .flat_map(|credits| credits.iter())
        .filter(|credit| {
            string(credit, &["status"])
                .is_some_and(|status| status.eq_ignore_ascii_case("available"))
        })
        .map(|credit| crate::QuotaReset {
            title: string(credit, &["title"])
                .unwrap_or_else(|| "Reset".to_string())
                .into(),
            expires_at_unix_ms: first_value(
                credit,
                &["expires_at", "expiresAt", "expiry", "expiry_date"],
            )
            .and_then(timestamp_to_millis),
        })
        .collect::<Vec<_>>();
    let available_count = number(payload, &["available_count", "availableCount"])
        .and_then(to_u64)
        .unwrap_or(available_resets.len() as u64);
    if credits.is_none() && number(payload, &["available_count", "availableCount"]).is_none() {
        return None;
    }
    Some(crate::QuotaResetSummary {
        available_count,
        resets: available_resets,
    })
}

pub fn normalize_codex_model_id(value: &str) -> Arc<str> {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .into()
}

fn window_label(window_seconds: Option<u64>, fallback_label: &str) -> String {
    match window_seconds {
        Some(18_000) => "5h".to_string(),
        Some(604_800) => "weekly".to_string(),
        Some(seconds) => format_window_seconds(seconds),
        None => fallback_label.to_string(),
    }
}

fn format_window_seconds(seconds: u64) -> String {
    if seconds.is_multiple_of(86_400) {
        format!("{}d", seconds / 86_400)
    } else if seconds.is_multiple_of(3_600) {
        format!("{}h", seconds / 3_600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn priority_for_window(window_seconds: Option<u64>, fallback: u16) -> u16 {
    match window_seconds {
        Some(18_000) => 0,
        Some(604_800) => 1,
        _ => fallback.saturating_add(10),
    }
}

fn credits_label(credits: &Value) -> Option<String> {
    if credits.get("unlimited").and_then(Value::as_bool) == Some(true) {
        return Some("Unlimited".to_string());
    }
    number(credits, &["balance"]).map(|balance| format!("${balance:.2}"))
}

fn timestamp_to_millis(value: &Value) -> Option<i64> {
    if let Some(number) = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
    {
        if !number.is_finite() {
            return None;
        }
        return Some(if number.abs() < 1_000_000_000_000.0 {
            (number * 1_000.0) as i64
        } else {
            number as i64
        });
    }
    value
        .as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
}

fn to_u64(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0).then_some(value as u64)
}

fn number(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                .filter(|value| value.is_finite())
        })
    })
}

fn first_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| value.get(*key).filter(|value| !value.is_null()))
}

fn first_string(payload: &Value, fallback: Option<&Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload
            .get(*key)
            .or_else(|| fallback.and_then(|value| value.get(*key)))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{QuotaError, QuotaSeverity, QuotaTarget, QuotaTargetKind};
    use futures::executor::block_on;
    use gpui::SharedString;
    use http_client::{AsyncBody, FakeHttpClient, HttpClient, Inner, Method, Response};
    use serde_json::{Value, json};
    use std::{sync::Arc, time::Duration};

    fn target(model_id: Option<&str>) -> QuotaTarget {
        QuotaTarget {
            kind: QuotaTargetKind::ExternalAgent,
            provider_or_agent_id: Arc::from("codex"),
            upstream_provider_id: None,
            model_id: model_id.map(Arc::from),
            model_name: model_id.map(Into::into),
        }
    }

    fn source() -> CodexQuotaSource {
        CodexQuotaSource {
            provider_id: Arc::from("codex"),
            provider_name: SharedString::from("Codex"),
            authentication_error: SharedString::from("Codex authentication expired"),
        }
    }

    fn window(seconds: u64, used_percent: f64) -> Value {
        json!({
            "used_percent": used_percent,
            "limit_window_seconds": seconds,
            "reset_after_seconds": seconds,
            "reset_at": 1_784_654_520_i64,
        })
    }

    fn payload_with_rate_limit(primary: Value, secondary: Option<Value>) -> Value {
        json!({
            "rate_limit": {
                "primary_window": primary,
                "secondary_window": secondary,
            },
            "plan_type": "prolite",
        })
    }

    fn response(status: u16, body: &'static str) -> anyhow::Result<Response<AsyncBody>> {
        Ok(Response::builder()
            .status(status)
            .body(AsyncBody::from(body))?)
    }

    #[test]
    fn parses_prolite_with_only_weekly_window() {
        let payload: Value =
            serde_json::from_str(include_str!("../test_data/quota/codex/prolite-weekly.json"))
                .expect("redacted Codex fixture should be valid JSON");
        let snapshot = parse_snapshot(&payload, &target(Some("gpt-5.3-codex")), &source())
            .expect("fixture should parse");
        assert_eq!(snapshot.plan.as_deref(), Some("prolite"));
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].label.as_ref(), "weekly");
        assert_eq!(snapshot.windows[0].remaining_percent, Some(70.0));
        assert_eq!(snapshot.windows[1].value_label.as_deref(), Some("$0.00"));
    }

    #[test]
    fn parses_five_hour_and_weekly_by_duration() {
        let payload = payload_with_rate_limit(window(18_000, 20.0), Some(window(604_800, 60.0)));
        let snapshot = parse_snapshot(&payload, &target(Some("gpt-5.3-codex")), &source())
            .expect("payload should parse");
        assert_eq!(
            snapshot
                .windows
                .iter()
                .map(|window| window.label.as_ref())
                .collect::<Vec<_>>(),
            vec!["5h", "weekly"]
        );
    }

    #[test]
    fn preserves_main_and_spark_weekly_groups() {
        let mut payload = payload_with_rate_limit(window(604_800, 60.0), None);
        payload["additional_rate_limits"] = json!([
            {
                "limit_name": "GPT-5.3-Codex-Spark",
                "metered_feature": "codex_bengalfox",
                "rate_limit": { "primary_window": window(604_800, 99.0) }
            }
        ]);
        let snapshot = parse_snapshot(&payload, &target(Some("gpt-5.3-codex-spark")), &source())
            .expect("payload should parse");
        assert_eq!(snapshot.groups.len(), 1);
        assert_eq!(snapshot.groups[0].id.as_ref(), "codex_bengalfox");
        assert_eq!(
            snapshot.groups[0].display_name.as_ref(),
            "GPT-5.3-Codex-Spark"
        );
        assert_eq!(
            snapshot.groups[0].applies_to_model_ids,
            vec![Arc::from("gpt53codexspark")]
        );
        assert_eq!(snapshot.severity(), Some(QuotaSeverity::Critical));
    }

    #[test]
    fn inactive_spark_group_does_not_affect_severity() {
        let mut payload = payload_with_rate_limit(window(604_800, 60.0), None);
        payload["additional_rate_limits"] = json!([
            {
                "limit_name": "GPT-5.3-Codex-Spark",
                "metered_feature": "codex_bengalfox",
                "rate_limit": { "primary_window": window(604_800, 99.0) }
            }
        ]);
        let snapshot = parse_snapshot(&payload, &target(Some("gpt-5.3-codex")), &source())
            .expect("payload should parse");
        assert_eq!(snapshot.severity(), Some(QuotaSeverity::Warning));
    }

    #[test]
    fn parses_null_and_non_null_code_review() {
        let mut payload = payload_with_rate_limit(window(604_800, 60.0), None);
        payload["code_review_rate_limit"] = Value::Null;
        assert!(
            parse_snapshot(&payload, &target(None), &source())
                .expect("payload should parse")
                .groups
                .is_empty()
        );

        payload["code_review_rate_limit"] = json!({
            "primary_window": window(18_000, 10.0),
            "secondary_window": null,
        });
        let snapshot =
            parse_snapshot(&payload, &target(None), &source()).expect("payload should parse");
        assert_eq!(snapshot.groups.len(), 1);
        assert_eq!(snapshot.groups[0].display_name.as_ref(), "Code review");
        assert!(!snapshot.groups[0].affects_severity);
        assert_eq!(snapshot.severity(), Some(QuotaSeverity::Warning));
    }

    #[test]
    fn parses_balance_and_unlimited_credits_without_severity() {
        let mut payload = payload_with_rate_limit(window(604_800, 60.0), None);
        payload["credits"] = json!({ "balance": 0.0, "unlimited": false });
        let snapshot =
            parse_snapshot(&payload, &target(None), &source()).expect("payload should parse");
        assert_eq!(snapshot.windows[1].value_label.as_deref(), Some("$0.00"));
        assert_eq!(snapshot.severity(), Some(QuotaSeverity::Warning));

        payload["credits"] = json!({ "unlimited": true });
        let snapshot =
            parse_snapshot(&payload, &target(None), &source()).expect("payload should parse");
        assert_eq!(
            snapshot.windows[1].value_label.as_deref(),
            Some("Unlimited")
        );
        assert_eq!(snapshot.severity(), Some(QuotaSeverity::Warning));
    }

    #[test]
    fn converts_unix_second_resets_to_milliseconds() {
        let payload = payload_with_rate_limit(window(18_000, 20.0), None);
        let snapshot =
            parse_snapshot(&payload, &target(None), &source()).expect("payload should parse");
        assert_eq!(
            snapshot.windows[0].reset_at_unix_ms,
            Some(1_784_654_520_000)
        );
    }

    #[test]
    fn prefers_reset_at_and_falls_back_to_reset_after_seconds() {
        let mut payload = payload_with_rate_limit(window(18_000, 20.0), None);
        payload["rate_limit"]["primary_window"] = json!({
            "used_percent": 20,
            "limit_window_seconds": 18_000,
            "reset_at": "2026-07-15T12:00:00Z",
            "reset_after_seconds": 99,
        });
        let snapshot =
            parse_snapshot(&payload, &target(None), &source()).expect("payload should parse");
        assert_eq!(
            snapshot.windows[0].reset_at_unix_ms,
            Some(1_784_116_800_000)
        );
    }

    #[test]
    fn accepts_missing_secondary_and_partial_payloads() {
        let payload = json!({
            "rate_limit": { "primary_window": { "used_percent": 10, "limit_window_seconds": 18000 } }
        });
        let snapshot = parse_snapshot(&payload, &target(None), &source())
            .expect("partial payload should parse");
        assert_eq!(snapshot.windows.len(), 1);
    }

    #[test]
    fn maps_401_403_429_other_status_and_malformed_json_without_response_body() {
        assert!(matches!(
            status_error(http_client::StatusCode::UNAUTHORIZED, &source()),
            QuotaError::Authentication(message) if message == "Codex authentication expired"
        ));
        assert!(matches!(
            status_error(http_client::StatusCode::FORBIDDEN, &source()),
            QuotaError::Authentication(_)
        ));
        assert!(matches!(
            status_error(http_client::StatusCode::TOO_MANY_REQUESTS, &source()),
            QuotaError::RateLimited { message, retry_after }
                if message == "Codex quota provider returned HTTP 429" && retry_after == Duration::from_secs(30)
        ));
        assert!(matches!(
            status_error(http_client::StatusCode::INTERNAL_SERVER_ERROR, &source()),
            QuotaError::Provider(message) if message == "Codex quota provider returned HTTP 500"
        ));

        let client: Arc<dyn HttpClient> =
            FakeHttpClient::create(|_| async { response(200, "malformed response marker") });
        let result = block_on(send_json(
            &client,
            USAGE_URL,
            "sensitive-value",
            Some("account-value"),
            false,
            &source(),
        ));
        assert!(
            matches!(result, Err(QuotaError::Provider(ref message)) if message == "Codex quota response was malformed")
        );
        assert!(!format!("{result:?}").contains("malformed response marker"));
        assert!(!format!("{result:?}").contains("sensitive-value"));
        assert!(!format!("{result:?}").contains("account-value"));
    }

    #[test]
    fn request_has_get_headers_and_no_body() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            assert_eq!(request.method(), Method::GET);
            assert!(matches!(&request.body().0, Inner::Empty));
            if request.uri().path().ends_with("rate-limit-reset-credits") {
                assert_eq!(request.headers().len(), 5);
                assert_eq!(request.headers()["Authorization"], "Bearer sensitive-value");
                assert_eq!(request.headers()["ChatGPT-Account-ID"], "account-value");
                assert_eq!(request.headers()["OpenAI-Beta"], "codex-1");
                assert_eq!(request.headers()["originator"], "Codex Desktop");
                assert_eq!(request.headers()["Content-Type"], "application/json");
                return response(200, r#"{"available_count":0,"credits":[]}"#);
            }
            assert_eq!(request.headers().len(), 3);
            assert_eq!(request.headers()["Authorization"], "Bearer sensitive-value");
            assert_eq!(request.headers()["ChatGPT-Account-Id"], "account-value");
            assert_eq!(request.headers()["Content-Type"], "application/json");
            response(
                200,
                r#"{"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000}}}"#,
            )
        });
        let snapshot = block_on(fetch_codex_snapshot(
            &client,
            "sensitive-value",
            Some("account-value"),
            &target(None),
            source(),
        ))
        .expect("primary request should succeed");
        assert_eq!(snapshot.windows.len(), 1);
    }

    #[test]
    fn reset_enrichment_is_best_effort() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            if request.uri().path().ends_with("rate-limit-reset-credits") {
                return Err(anyhow::anyhow!("reset request failed"));
            }
            response(
                200,
                r#"{"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000}}}"#,
            )
        });
        let snapshot = block_on(fetch_codex_snapshot(
            &client,
            "sensitive-value",
            None,
            &target(None),
            source(),
        ))
        .expect("primary request should succeed");
        assert!(snapshot.available_resets.is_none());
    }

    #[test]
    fn normalizes_only_available_reset_titles_and_expiry() {
        let payload: Value = serde_json::from_str(include_str!(
            "../test_data/quota/codex/available-resets.json"
        ))
        .expect("redacted reset fixture should be valid JSON");
        let summary = parse_reset_summary(&payload).expect("reset fixture should parse");
        assert_eq!(summary.available_count, 2);
        assert_eq!(summary.resets.len(), 2);
        assert_eq!(summary.resets[0].title.as_ref(), "Weekly bonus");
        assert_eq!(
            summary.resets[0].expires_at_unix_ms,
            Some(1_784_654_520_000)
        );
        assert_eq!(summary.resets[1].title.as_ref(), "Reset");
        assert_eq!(
            summary.resets[1].expires_at_unix_ms,
            Some(1_784_116_800_000)
        );
        let mut without_count = payload;
        without_count
            .as_object_mut()
            .expect("fixture object")
            .remove("available_count");
        assert_eq!(
            parse_reset_summary(&without_count)
                .expect("count fallback")
                .available_count,
            2
        );
        let debug = format!("{summary:?}");
        for marker in ["id", "account", "email", "token", "redeemed", "expired"] {
            assert!(!debug.to_ascii_lowercase().contains(marker));
        }
    }

    #[test]
    fn errors_and_debug_output_contain_no_secret_markers() {
        let error = QuotaError::Authentication(source().authentication_error);
        let debug = format!("{error:?}");
        assert!(!debug.contains("sensitive-value"));
        assert!(!debug.contains("account-value"));
        assert!(!debug.contains("response marker"));
    }

    #[test]
    fn number_rejects_non_finite_values() {
        for value in [json!("NaN"), json!("inf"), json!("-inf")] {
            assert_eq!(number(&json!({"value": value}), &["value"]), None);
        }
        assert_eq!(
            number(
                &json!({"first": "NaN", "second": "4.5"}),
                &["first", "second"]
            ),
            Some(4.5)
        );
    }
}
