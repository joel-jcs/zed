use acp_thread::{SessionCost, TokenUsage};
use ai_usage::{QuotaSnapshot, QuotaView, QuotaWindow};
use chrono::{DateTime, Utc};
use gpui::{App, Context, Div, Entity, IntoElement, Render, Subscription, Window, div, px};
use project::ProjectEntryId;
use ui::{ButtonLike, CircularProgress, Icon, PopoverMenu, PopoverMenuHandle, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use super::{ActiveQuotaTarget, popover::QuotaPopover};

const FIVE_HOUR_SECONDS: u64 = 18_000;
const WEEKLY_SECONDS: u64 = 604_800;

const CONTEXT_RING_SIZE: f32 = 16.0;
const CONTEXT_RING_STROKE_WIDTH: f32 = 2.0;
const CONTEXT_RING_RADIUS: f32 = 6.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RingTone {
    Normal,
    Warning,
}

#[derive(Clone)]
pub(crate) struct ContextUsageData {
    pub(crate) token_usage: TokenUsage,
    pub(crate) show_split: bool,
    pub(crate) cost: Option<SessionCost>,
    pub(crate) global_agents_md_loaded: bool,
    pub(crate) project_rules_count: usize,
    pub(crate) project_entry_ids: Vec<ProjectEntryId>,
    pub(crate) workspace: gpui::WeakEntity<Workspace>,
}

pub(crate) struct ContextQuotaIndicator {
    target: ActiveQuotaTarget,
    store: Entity<ai_usage::QuotaStore>,
    menu_handle: PopoverMenuHandle<QuotaPopover>,
    context_usage: Option<ContextUsageData>,
    _subscriptions: Vec<Subscription>,
}

impl ContextQuotaIndicator {
    pub(crate) fn new(
        target: ActiveQuotaTarget,
        store: Entity<ai_usage::QuotaStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        store.update(cx, |store, cx| store.activate(target.0.clone(), cx));
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            target,
            store,
            menu_handle: PopoverMenuHandle::default(),
            context_usage: None,
            _subscriptions: vec![store_subscription],
        }
    }

    pub(crate) fn set_target(&mut self, target: ActiveQuotaTarget, cx: &mut Context<Self>) {
        if self.target == target {
            return;
        }
        self.store.update(cx, |store, cx| {
            store.deactivate(&self.target.0, cx);
            store.activate(target.0.clone(), cx);
        });
        self.target = target;
        cx.notify();
    }

    pub(crate) fn release(&mut self, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.deactivate(&self.target.0, cx));
    }

    pub(crate) fn set_context_usage(
        &mut self,
        context_usage: Option<ContextUsageData>,
        cx: &mut Context<Self>,
    ) {
        self.context_usage = context_usage;
        cx.notify();
    }

    pub(crate) fn context_usage(&self) -> Option<ContextUsageData> {
        self.context_usage.clone()
    }

    pub(crate) fn active_target(&self) -> ActiveQuotaTarget {
        self.target.clone()
    }

    fn view(&self, cx: &mut Context<Self>) -> Option<QuotaView> {
        self.store
            .update(cx, |store, _cx| store.view(&self.target.0))
    }

    fn render_indicator(
        &self,
        view: Option<&QuotaView>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let snapshot = view.and_then(|view| view.snapshot.as_ref());

        let quota_fallback = quota_fallback_window(self.context_usage.is_some(), snapshot);

        let trigger_content = if self.context_usage.is_some() {
            self.render_context_only(cx)
        } else if let Some(window) = quota_fallback {
            self.render_quota_fallback_ring(window, cx)
        } else {
            return div().into_any_element();
        };

        let is_open = self.menu_handle.is_deployed();

        let aria_label: SharedString = if let Some(window) = quota_fallback {
            format!(
                "Show quota: {}, {}",
                window_display_label(window),
                window_value(window),
            )
            .into()
        } else {
            "Show context and quota".into()
        };

        let trigger = ButtonLike::new("context-quota-indicator")
            .style(ButtonStyle::Transparent)
            .size(ButtonSize::None)
            .aria_label(aria_label)
            .child(trigger_content)
            .when(!is_open, |trigger| {
                trigger.hoverable_tooltip({
                    let context_usage = self.context_usage.clone();
                    let quota_lines = snapshot
                        .map(|s| quota_lines(s, current_unix_ms()))
                        .unwrap_or_default();
                    move |_window, cx| {
                        cx.new(|_cx| ContextQuotaTooltip {
                            context_usage: context_usage.clone(),
                            quota_lines: quota_lines.clone(),
                        })
                        .into()
                    }
                })
            });

        let store = self.store.clone();
        let active_target = self.target.clone();
        let context_source = cx.entity().downgrade();
        PopoverMenu::new("context-quota-popover")
            .trigger(trigger)
            .menu(move |window, cx| {
                Some(cx.new(|cx| {
                    QuotaPopover::new_with_context_source(
                        Some(active_target.clone()),
                        None,
                        store.clone(),
                        Some(context_source.clone()),
                        window,
                        cx,
                    )
                }))
            })
            .with_handle(self.menu_handle.clone())
            .anchor(gpui::Anchor::BottomLeft)
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            })
            .into_any_element()
    }

    fn render_quota_fallback_ring(
        &self,
        window: &QuotaWindow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let remaining_percent = window.remaining_percent.unwrap_or_default();

        h_flex()
            .id("quota_fallback_usage")
            .mt_px()
            .mr_1()
            .child(
                CircularProgress::new(remaining_percent as f32, 100.0, px(CONTEXT_RING_SIZE), cx)
                    .stroke_width(px(CONTEXT_RING_STROKE_WIDTH))
                    .radius(px(CONTEXT_RING_RADIUS))
                    .progress_color(quota_progress_color(remaining_percent, cx)),
            )
            .into_any_element()
    }

    fn render_context_only(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(context_usage) = self.context_usage.as_ref() else {
            return div().size(px(CONTEXT_RING_SIZE)).into_any_element();
        };
        let usage = &context_usage.token_usage;
        let max_output_tokens = usage.max_output_tokens.unwrap_or(0);
        let input_max_tokens = usage.max_tokens.saturating_sub(max_output_tokens);

        if context_usage.show_split {
            let input_tone = split_context_tone(usage.input_tokens, input_max_tokens);
            let output_tone = split_context_tone(usage.output_tokens, max_output_tokens);
            h_flex()
                .id("split_token_usage")
                .flex_shrink_0()
                .gap_1p5()
                .mr_1()
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Icon::new(IconName::ArrowUp)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            CircularProgress::new(
                                usage.input_tokens as f32,
                                input_max_tokens as f32,
                                px(CONTEXT_RING_SIZE),
                                cx,
                            )
                            .stroke_width(px(CONTEXT_RING_STROKE_WIDTH))
                            .radius(px(CONTEXT_RING_RADIUS))
                            .progress_color(tone_color(input_tone, cx)),
                        ),
                )
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Icon::new(IconName::ArrowDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            CircularProgress::new(
                                usage.output_tokens as f32,
                                max_output_tokens as f32,
                                px(CONTEXT_RING_SIZE),
                                cx,
                            )
                            .stroke_width(px(CONTEXT_RING_STROKE_WIDTH))
                            .radius(px(CONTEXT_RING_RADIUS))
                            .progress_color(tone_color(output_tone, cx)),
                        ),
                )
                .into_any_element()
        } else {
            let used_ratio = context_used_ratio(usage);
            h_flex()
                .id("circular_progress_tokens")
                .mt_px()
                .mr_1()
                .child(
                    CircularProgress::new(
                        usage.used_tokens as f32,
                        usage.max_tokens as f32,
                        px(CONTEXT_RING_SIZE),
                        cx,
                    )
                    .stroke_width(px(CONTEXT_RING_STROKE_WIDTH))
                    .radius(px(CONTEXT_RING_RADIUS))
                    .progress_color(tone_color(context_tone(used_ratio), cx)),
                )
                .into_any_element()
        }
    }
}

impl Render for ContextQuotaIndicator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_indicator(self.view(cx).as_ref(), window, cx)
    }
}

pub(crate) fn all_items(snapshot: &QuotaSnapshot) -> Vec<QuotaWindow> {
    let mut windows = snapshot.windows.clone();

    if let Some(active_model_id) = snapshot.active_model_id.as_ref()
        && let Some(model_windows) = snapshot.model_windows.get(active_model_id)
    {
        windows.extend(model_windows.iter().cloned());
    }

    for group in &snapshot.groups {
        let applies = group.applies_to_model_ids.is_empty()
            || snapshot
                .active_model_id
                .as_ref()
                .is_some_and(|active| group.applies_to_model_ids.contains(active));

        if applies {
            windows.extend(group.buckets.iter().map(|bucket| bucket.window.clone()));
        }
    }

    windows
}

pub(crate) fn window_value(window: &QuotaWindow) -> String {
    if let Some(remaining) = window.remaining_percent {
        format!("{remaining:.0}% left")
    } else if let Some(value) = &window.value_label {
        value.to_string()
    } else {
        "Unavailable".to_string()
    }
}

pub(crate) fn reset_value(window: &QuotaWindow, now_unix_ms: i64) -> Option<String> {
    if let Some(reset_at) = window.reset_at_unix_ms {
        return Some(format_duration(
            (reset_at - now_unix_ms).max(0) as u64 / 1000,
        ));
    }
    window.reset_after_seconds.map(format_duration)
}

fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

pub(crate) fn relative_age(fetched_at_unix_ms: i64, now_unix_ms: i64) -> String {
    format_duration((now_unix_ms - fetched_at_unix_ms).max(0) as u64 / 1000)
}

pub(crate) fn absolute_time(unix_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(unix_ms)
        .map(|time| time.format("%-I:%M %p").to_string())
        .unwrap_or_else(|| "Unknown time".to_string())
}

#[cfg(test)]
pub(crate) fn render_tooltip_text(snapshot: &QuotaSnapshot, now_unix_ms: i64) -> String {
    quota_lines(snapshot, now_unix_ms).join("\n")
}

pub(crate) fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn context_used_ratio(usage: &TokenUsage) -> f32 {
    if usage.max_tokens == 0 {
        0.
    } else {
        (usage.used_tokens as f32 / usage.max_tokens as f32).clamp(0., 1.)
    }
}

fn split_context_used_ratio(used: u64, max: u64) -> f32 {
    if max == 0 {
        0.
    } else {
        (used as f32 / max as f32).clamp(0., 1.)
    }
}

fn split_context_tone(used: u64, max: u64) -> RingTone {
    context_tone(split_context_used_ratio(used, max))
}

fn tone_color(tone: RingTone, cx: &App) -> gpui::Hsla {
    match tone {
        RingTone::Normal => cx.theme().colors().text_muted,
        RingTone::Warning => cx.theme().status().warning,
    }
}

fn quota_lines(snapshot: &QuotaSnapshot, now_unix_ms: i64) -> Vec<String> {
    let mut lines = vec![snapshot.provider_name.to_string()];
    for window in all_items(snapshot) {
        let label = window_display_label(&window);
        let mut line = format!("{}: {}", label, window_value(&window));

        if let Some(reset) = reset_value(&window, now_unix_ms) {
            line.push_str(&format!(" · Resets in {reset}"));
        }

        lines.push(line);
    }
    lines.push(format!(
        "Updated {} ago",
        relative_age(snapshot.fetched_at_unix_ms, now_unix_ms)
    ));
    lines
}

struct ContextQuotaTooltip {
    context_usage: Option<ContextUsageData>,
    quota_lines: Vec<String>,
}

impl Render for ContextQuotaTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(context_usage) = self.context_usage.clone() else {
            return ui::tooltip_container(cx, |container, _cx| {
                container
                    .min_w_40()
                    .child(
                        Label::new("Quota")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .children(self.quota_lines.iter().map(|line| Label::new(line.clone())))
            })
            .into_any_element();
        };

        let usage = context_usage.token_usage;
        let used = crate::humanize_token_count(usage.used_tokens);
        let max = crate::humanize_token_count(usage.max_tokens);
        let percentage = format!("{}%", (context_used_ratio(&usage) * 100.).round() as u32);
        let input_tokens = crate::humanize_token_count(usage.input_tokens);
        let output_tokens = crate::humanize_token_count(usage.output_tokens);
        let max_output_tokens = usage.max_output_tokens.unwrap_or(0);
        let input_max =
            crate::humanize_token_count(usage.max_tokens.saturating_sub(max_output_tokens));
        let output_max = crate::humanize_token_count(max_output_tokens);
        let separator_color = Color::Custom(cx.theme().colors().text_disabled.opacity(0.6));
        let quota_lines = self.quota_lines.clone();
        let workspace = context_usage.workspace.clone();
        let project_entry_ids = context_usage.project_entry_ids.clone();
        let global_agents_md_loaded = context_usage.global_agents_md_loaded;
        let project_rules_count = context_usage.project_rules_count;
        let cost_label = context_usage.cost.map(|cost| {
            let precision = if cost.amount > 0.0 && cost.amount < 0.01 {
                4
            } else {
                2
            };
            format!("{:.prec$} {}", cost.amount, cost.currency, prec = precision)
        });
        let show_split = context_usage.show_split;

        ui::tooltip_container(cx, move |mut container: Div, cx: &mut Context<Self>| {
            container = container.min_w_40().child(
                Label::new("Context")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            );
            if !show_split {
                container = container.child(
                    h_flex()
                        .gap_0p5()
                        .child(Label::new(percentage))
                        .child(Label::new("\u{2022}").color(separator_color).mx_1())
                        .child(Label::new(used))
                        .child(Label::new("/").color(separator_color))
                        .child(Label::new(max).color(Color::Muted)),
                );
            } else {
                container = container.child(
                    v_flex()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_0p5()
                                .child(Label::new("Input:").color(Color::Muted).mr_0p5())
                                .child(Label::new(input_tokens))
                                .child(Label::new("/").color(separator_color))
                                .child(Label::new(input_max).color(Color::Muted)),
                        )
                        .child(
                            h_flex()
                                .gap_0p5()
                                .child(Label::new("Output:").color(Color::Muted).mr_0p5())
                                .child(Label::new(output_tokens))
                                .child(Label::new("/").color(separator_color))
                                .child(Label::new(output_max).color(Color::Muted)),
                        ),
                );
            }
            if !quota_lines.is_empty() {
                container = container.child(
                    v_flex()
                        .mt_1p5()
                        .pt_1p5()
                        .gap_0p5()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Label::new("Quota")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .children(quota_lines.into_iter().map(Label::new)),
                );
            }
            if let Some(cost_label) = cost_label {
                container = container.child(
                    v_flex()
                        .mt_1p5()
                        .pt_1p5()
                        .gap_0p5()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Label::new("Cost")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .child(Label::new(cost_label)),
                );
            }
            if global_agents_md_loaded || project_rules_count > 0 {
                let mut rules = v_flex()
                    .mt_1p5()
                    .pt_1p5()
                    .pb_0p5()
                    .gap_0p5()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new("Rules")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(v_flex().mx_neg_1());
                if global_agents_md_loaded {
                    let workspace = workspace.clone();
                    rules = rules.child(
                        Button::new("open-global-agents-md", "1 global rule")
                            .end_icon(
                                Icon::new(IconName::ArrowUpRight)
                                    .color(Color::Muted)
                                    .size(IconSize::XSmall),
                            )
                            .on_click(move |_, window, cx| {
                                workspace
                                    .update(cx, |workspace, cx| {
                                        workspace
                                            .open_abs_path(
                                                paths::agents_file().clone(),
                                                workspace::OpenOptions {
                                                    focus: Some(true),
                                                    ..Default::default()
                                                },
                                                window,
                                                cx,
                                            )
                                            .detach_and_log_err(cx);
                                    })
                                    .log_err();
                            }),
                    );
                }
                if project_rules_count > 0 {
                    let workspace = workspace.clone();
                    let project_entry_ids = project_entry_ids.clone();
                    rules = rules.child(
                        Button::new(
                            "open-project-rules",
                            format!("{} project rules", project_rules_count),
                        )
                        .end_icon(
                            Icon::new(IconName::ArrowUpRight)
                                .color(Color::Muted)
                                .size(IconSize::XSmall),
                        )
                        .on_click(move |_, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    let project = workspace.project().read(cx);
                                    let paths = project_entry_ids
                                        .iter()
                                        .filter_map(|id| project.path_for_entry(*id, cx))
                                        .collect::<Vec<_>>();
                                    for path in paths {
                                        workspace
                                            .open_path(path, None, true, window, cx)
                                            .detach_and_log_err(cx);
                                    }
                                })
                                .log_err();
                        }),
                    );
                }
                container = container.child(rules);
            }
            container
        })
        .into_any_element()
    }
}

pub(crate) fn quota_progress_color(remaining_percent: f64, cx: &App) -> gpui::Hsla {
    use ai_usage::QuotaSeverity;
    match QuotaSeverity::from_remaining_percent(remaining_percent) {
        QuotaSeverity::Normal => cx.theme().colors().text_muted,
        QuotaSeverity::Warning => cx.theme().status().warning,
        QuotaSeverity::Critical => cx.theme().status().error,
    }
}

fn context_tone(used_ratio: f32) -> RingTone {
    if used_ratio >= 0.85 {
        RingTone::Warning
    } else {
        RingTone::Normal
    }
}

fn shortest_quota_window(snapshot: &QuotaSnapshot) -> Option<&QuotaWindow> {
    snapshot
        .applicable_windows()
        .into_iter()
        .filter(|window| window.remaining_percent.is_some())
        .min_by(
            |left, right| match (left.window_seconds, right.window_seconds) {
                (Some(left_seconds), Some(right_seconds)) => left_seconds
                    .cmp(&right_seconds)
                    .then_with(|| {
                        left.remaining_percent
                            .unwrap_or(f64::INFINITY)
                            .total_cmp(&right.remaining_percent.unwrap_or(f64::INFINITY))
                    })
                    .then_with(|| {
                        left.compact_priority
                            .unwrap_or(u16::MAX)
                            .cmp(&right.compact_priority.unwrap_or(u16::MAX))
                    }),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => left
                    .compact_priority
                    .unwrap_or(u16::MAX)
                    .cmp(&right.compact_priority.unwrap_or(u16::MAX))
                    .then_with(|| {
                        left.remaining_percent
                            .unwrap_or(f64::INFINITY)
                            .total_cmp(&right.remaining_percent.unwrap_or(f64::INFINITY))
                    }),
            },
        )
}

fn quota_fallback_window(
    context_available: bool,
    snapshot: Option<&QuotaSnapshot>,
) -> Option<&QuotaWindow> {
    if context_available {
        None
    } else {
        snapshot.and_then(shortest_quota_window)
    }
}

pub(crate) fn window_display_label(window: &QuotaWindow) -> SharedString {
    match window.window_seconds {
        Some(FIVE_HOUR_SECONDS) => "5-hour".into(),
        Some(WEEKLY_SECONDS) => "Weekly".into(),
        _ => window.label.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortest_quota_window_known_versus_unknown_durations() {
        let snapshot = snapshot(vec![
            {
                let mut q = quota("Unknown", 0, 50.0);
                q.window_seconds = None;
                q
            },
            quota("Known", 3600, 40.0),
        ]);
        // Known duration should be preferred over unknown duration
        assert_eq!(
            shortest_quota_window(&snapshot).and_then(|w| w.window_seconds),
            Some(3600)
        );
    }

    #[test]
    fn non_percentage_rows_do_not_create_fallback() {
        let snapshot = snapshot(vec![QuotaWindow::value("Credits balance", "$10.00")]);

        assert!(quota_fallback_window(false, Some(&snapshot)).is_none());
    }

    #[test]
    fn context_suppresses_quota_fallback() {
        let snapshot = snapshot(vec![quota("Weekly", WEEKLY_SECONDS, 80.0)]);

        assert!(quota_fallback_window(true, Some(&snapshot)).is_none());
        assert!(quota_fallback_window(false, Some(&snapshot)).is_some());
    }

    use ai_usage::QuotaAccountSummary;
    use collections::HashMap;
    use std::sync::Arc;

    fn snapshot(windows: Vec<QuotaWindow>) -> QuotaSnapshot {
        QuotaSnapshot {
            provider_id: Arc::from("provider"),
            provider_name: "Provider".into(),
            account: QuotaAccountSummary {
                fingerprint: Arc::from("account-fingerprint"),
                safe_label: Some("account".into()),
            },
            plan: None,
            fetched_at_unix_ms: 1_700_000_000_000,
            active_model_id: None,
            windows,
            model_windows: HashMap::default(),
            groups: Vec::new(),
            available_resets: None,
        }
    }

    fn snapshot_fetched_at(unix_ms: i64) -> QuotaSnapshot {
        let mut s = snapshot(Vec::new());
        s.fetched_at_unix_ms = unix_ms;
        s
    }

    fn quota(label: &str, seconds: u64, remaining: f64) -> QuotaWindow {
        QuotaWindow::percentage(label, remaining).with_window_seconds(seconds)
    }

    #[test]
    fn shortest_quota_window_fallback_coverage() {
        // Weekly fallback
        let snapshot1 = snapshot(vec![
            quota("Monthly", 2_592_000, 90.0),
            quota("Weekly", WEEKLY_SECONDS, 80.0),
        ]);
        assert_eq!(
            shortest_quota_window(&snapshot1).and_then(|w| w.window_seconds),
            Some(WEEKLY_SECONDS)
        );

        // Monthly-only fallback
        let snapshot2 = snapshot(vec![quota("Monthly", 2_592_000, 90.0)]);
        assert_eq!(
            shortest_quota_window(&snapshot2).and_then(|w| w.window_seconds),
            Some(2_592_000)
        );

        // Equal duration tie-breaking
        let mut q_a = quota("5-hour-A", 18_000, 70.0);
        q_a.compact_priority = Some(10);
        let mut q_b = quota("5-hour-B", 18_000, 20.0);
        q_b.compact_priority = Some(5);
        let snapshot3 = snapshot(vec![q_a, q_b]);
        assert_eq!(
            shortest_quota_window(&snapshot3).map(|w| w.label.as_ref()),
            Some("5-hour-B")
        );

        // Unknown durations
        let mut q_unknown1 = quota("Unknown1", 0, 80.0);
        q_unknown1.window_seconds = None;
        let mut q_unknown2 = quota("Unknown2", 0, 30.0);
        q_unknown2.window_seconds = None;
        let snapshot4 = snapshot(vec![q_unknown1, q_unknown2]);
        assert_eq!(
            shortest_quota_window(&snapshot4).and_then(|w| w.remaining_percent),
            Some(30.0)
        );
    }
    #[test]
    fn shortest_quota_window_prefers_shortest_duration() {
        let snapshot = snapshot(vec![
            quota("Monthly", 2_592_000, 90.0),
            quota("Weekly", 604_800, 80.0),
            quota("5-hour", 18_000, 70.0),
        ]);

        assert_eq!(
            shortest_quota_window(&snapshot).and_then(|window| window.window_seconds),
            Some(18_000),
        );
    }

    #[test]
    fn context_tone_keeps_existing_eighty_five_percent_warning() {
        assert_eq!(context_tone(0.849), RingTone::Normal);
        assert_eq!(context_tone(0.85), RingTone::Warning);
        assert_eq!(context_tone(1.0), RingTone::Warning);
    }

    #[test]
    fn tooltip_title_omits_account_fingerprint_and_model() {
        let mut snapshot = snapshot_fetched_at(0);
        snapshot.account.safe_label = Some("user@example.test".into());
        snapshot.account.fingerprint = Arc::from("user_fingerprint");
        snapshot.active_model_id = Some(Arc::from("model-id"));
        let lines = quota_lines(&snapshot, 0);
        assert_eq!(lines[0], "Provider");
        assert!(!lines[0].contains("user@example.test"));
        assert!(!lines[0].contains("user_fingerprint"));
        assert!(!lines[0].contains("Display Model"));
    }
}
