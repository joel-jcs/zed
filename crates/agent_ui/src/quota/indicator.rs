use std::sync::Arc;

use acp_thread::{SessionCost, TokenUsage};
use agent_settings::{AgentSettings, QuotaRingVisibility};
use ai_usage::{QuotaSnapshot, QuotaView, QuotaWindow};
use chrono::{DateTime, Utc};
use fs::Fs;
use gpui::{App, Context, Div, Entity, IntoElement, Render, Subscription, Window, div, px};
use project::ProjectEntryId;
use settings::{Settings, SettingsStore};
use ui::{ButtonLike, CircularProgress, Icon, PopoverMenu, PopoverMenuHandle, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use super::{ActiveQuotaTarget, popover::QuotaPopover};

const FIVE_HOUR_SECONDS: u64 = 18_000;
const WEEKLY_SECONDS: u64 = 604_800;

const CONTEXT_RING_SIZE: f32 = 16.0;
const CONTEXT_RING_STROKE_WIDTH: f32 = 2.0;
const CONTEXT_RING_RADIUS: f32 = 6.0;
const QUOTA_BAR_STACK_WIDTH: f32 = 12.0;
const QUOTA_BAR_HEIGHT: f32 = 3.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RingKind {
    Weekly,
    FiveHour,
    Context,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RingTone {
    Normal,
    Warning,
    Critical,
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
    fs: Arc<dyn Fs>,
    menu_handle: PopoverMenuHandle<QuotaPopover>,
    context_usage: Option<ContextUsageData>,
    _subscriptions: Vec<Subscription>,
}

impl ContextQuotaIndicator {
    pub(crate) fn new(
        target: ActiveQuotaTarget,
        store: Entity<ai_usage::QuotaStore>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        store.update(cx, |store, cx| store.activate(target.0.clone(), cx));
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());
        let settings_subscription = cx.observe_global::<SettingsStore>(|_, cx| cx.notify());
        Self {
            target,
            store,
            fs,
            menu_handle: PopoverMenuHandle::default(),
            context_usage: None,
            _subscriptions: vec![store_subscription, settings_subscription],
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
        settings: QuotaRingVisibility,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let snapshot = view.and_then(|view| view.snapshot.as_ref());
        if !has_visible_content(
            snapshot,
            RingVisibility {
                weekly: settings.weekly,
                five_hour: settings.five_hour,
                context: settings.context,
            },
            self.context_usage.is_some(),
        ) {
            return div().into_any_element();
        }
        let rings = snapshot
            .map(|snapshot| {
                compact_rings(
                    snapshot,
                    RingVisibility {
                        weekly: settings.weekly,
                        five_hour: settings.five_hour,
                        context: settings.context,
                    },
                    self.context_usage.is_some(),
                )
            })
            .unwrap_or_else(|| {
                effective_visibility(
                    RingVisibility {
                        weekly: settings.weekly,
                        five_hour: settings.five_hour,
                        context: settings.context,
                    },
                    RingAvailability {
                        weekly: false,
                        five_hour: false,
                        context: self.context_usage.is_some(),
                    },
                )
            });

        let display_mode = AgentSettings::get_global(cx).quota.display_mode;
        let context_only = rings == [RingKind::Context];
        let trigger = ButtonLike::new("context-quota-indicator")
            .aria_label("Show context and quota")
            .child(if context_only {
                self.render_context_only(cx)
            } else {
                self.render_compact_indicator(&rings, snapshot, display_mode, cx)
            })
            .hoverable_tooltip({
                let context_usage = self.context_usage.clone();
                let active_target = self.target.0.clone();
                let quota_lines =
                    snapshot.map(|s| quota_lines(s, Some(&active_target), current_unix_ms()));
                move |_window, cx| {
                    cx.new(|_cx| ContextQuotaTooltip {
                        context_usage: context_usage.clone(),
                        quota_lines: quota_lines.clone().unwrap_or_default(),
                    })
                    .into()
                }
            });

        let store = self.store.clone();
        let active_target = self.target.clone();
        let context_source = cx.entity().downgrade();
        let fs = self.fs.clone();
        PopoverMenu::new("context-quota-popover")
            .trigger(trigger)
            .menu(move |_window, cx| {
                Some(cx.new(|cx| {
                    QuotaPopover::new_with_context_source(
                        Some(active_target.clone()),
                        None,
                        store.clone(),
                        Some(fs.clone()),
                        Some(context_source.clone()),
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

    fn render_single_context_ring(
        &self,
        display_mode: settings::QuotaDisplayMode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(context_usage) = self.context_usage.as_ref() else {
            return div().size(px(CONTEXT_RING_SIZE)).into_any_element();
        };

        let used_ratio = context_used_ratio(&context_usage.token_usage);

        CircularProgress::new(
            context_fill(used_ratio, display_mode),
            100.0,
            px(CONTEXT_RING_SIZE),
            cx,
        )
        .stroke_width(px(CONTEXT_RING_STROKE_WIDTH))
        .radius(px(CONTEXT_RING_RADIUS))
        .progress_color(tone_color(context_tone(used_ratio), cx))
        .into_any_element()
    }

    fn render_compact_indicator(
        &self,
        rings: &[RingKind],
        snapshot: Option<&QuotaSnapshot>,
        display_mode: settings::QuotaDisplayMode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let show_context = rings.contains(&RingKind::Context) && self.context_usage.is_some();
        let quota_bars = quota_bar_kinds(rings);

        let mut content = h_flex().items_center().gap_1();

        if show_context {
            content = content.child(self.render_single_context_ring(display_mode, cx));
        }

        if !quota_bars.is_empty() {
            content = content.child(render_quota_bar_stack(
                &quota_bars,
                snapshot,
                display_mode,
                cx,
            ));
        }

        content.into_any_element()
    }

    fn render_context_only(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(context_usage) = self.context_usage.as_ref() else {
            return div().size(px(CONTEXT_RING_SIZE)).into_any_element();
        };
        let usage = &context_usage.token_usage;
        let display_mode = AgentSettings::get_global(cx).quota.display_mode;
        let max_output_tokens = usage.max_output_tokens.unwrap_or(0);
        let input_max_tokens = usage.max_tokens.saturating_sub(max_output_tokens);
        let layout = context_ring_layout(context_usage.show_split, &[RingKind::Context]);
        let ring = |value, tone| {
            CircularProgress::new(value, 100.0, px(CONTEXT_RING_SIZE), cx)
                .stroke_width(px(CONTEXT_RING_STROKE_WIDTH))
                .radius(px(CONTEXT_RING_RADIUS))
                .progress_color(tone_color(tone, cx))
        };
        if layout == ContextRingLayout::SplitSideBySide {
            let input_tone = split_context_tone(usage.input_tokens, input_max_tokens);
            let output_tone = split_context_tone(usage.output_tokens, max_output_tokens);
            h_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Icon::new(IconName::ArrowUp)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(ring(
                            split_context_fill(usage.input_tokens, input_max_tokens, display_mode),
                            input_tone,
                        )),
                )
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Icon::new(IconName::ArrowDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(ring(
                            split_context_fill(
                                usage.output_tokens,
                                max_output_tokens,
                                display_mode,
                            ),
                            output_tone,
                        )),
                )
                .into_any_element()
        } else {
            self.render_single_context_ring(display_mode, cx)
        }
    }
}

impl Render for ContextQuotaIndicator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = AgentSettings::get_global(cx).quota.visible_rings;
        self.render_indicator(self.view(cx).as_ref(), settings, window, cx)
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
pub(crate) fn render_tooltip_text(
    snapshot: &QuotaSnapshot,
    target: Option<&ai_usage::QuotaTarget>,
    now_unix_ms: i64,
) -> String {
    quota_lines(snapshot, target, now_unix_ms).join("\n")
}

pub(crate) fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn quota_fill(window: &QuotaWindow, display_mode: settings::QuotaDisplayMode) -> Option<f32> {
    match display_mode {
        settings::QuotaDisplayMode::Remaining => window.remaining_percent.map(|value| value as f32),
        settings::QuotaDisplayMode::Used => window
            .used_percent
            .or_else(|| window.remaining_percent.map(|value| 100. - value))
            .map(|value| value as f32),
    }
}

fn context_used_ratio(usage: &TokenUsage) -> f32 {
    if usage.max_tokens == 0 {
        0.
    } else {
        (usage.used_tokens as f32 / usage.max_tokens as f32).clamp(0., 1.)
    }
}

fn context_fill(used_ratio: f32, display_mode: settings::QuotaDisplayMode) -> f32 {
    match display_mode {
        settings::QuotaDisplayMode::Remaining => (1. - used_ratio) * 100.,
        settings::QuotaDisplayMode::Used => used_ratio * 100.,
    }
}

fn split_context_fill(used: u64, max: u64, display_mode: settings::QuotaDisplayMode) -> f32 {
    let used_ratio = split_context_used_ratio(used, max);
    match display_mode {
        settings::QuotaDisplayMode::Remaining => (1. - used_ratio) * 100.,
        settings::QuotaDisplayMode::Used => used_ratio * 100.,
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
        RingTone::Critical => cx.theme().status().error,
    }
}

fn render_quota_bar(fill_percent: f32, tone: RingTone, cx: &App) -> AnyElement {
    div()
        .w(px(QUOTA_BAR_STACK_WIDTH))
        .h(px(QUOTA_BAR_HEIGHT))
        .rounded_full()
        .overflow_hidden()
        .bg(cx.theme().colors().border_variant)
        .child(
            div()
                .h_full()
                .w(relative((fill_percent / 100.0).clamp(0.0, 1.0)))
                .rounded_full()
                .bg(tone_color(tone, cx)),
        )
        .into_any_element()
}

fn render_quota_bar_stack(
    quota_bars: &[RingKind],
    snapshot: Option<&QuotaSnapshot>,
    display_mode: settings::QuotaDisplayMode,
    cx: &App,
) -> AnyElement {
    let mut stack = v_flex()
        .w(px(QUOTA_BAR_STACK_WIDTH))
        .h(px(CONTEXT_RING_SIZE))
        .justify_center()
        .gap_0p5();

    for kind in quota_bars.iter().copied() {
        let Some(window) = snapshot.and_then(|snapshot| applicable_quota_window(snapshot, kind))
        else {
            continue;
        };
        let Some(fill_percent) = quota_fill(window, display_mode) else {
            continue;
        };
        let Some(remaining_percent) = window.remaining_percent else {
            continue;
        };

        stack = stack.child(render_quota_bar(
            fill_percent,
            quota_tone(remaining_percent),
            cx,
        ));
    }

    stack.into_any_element()
}

fn quota_lines(
    snapshot: &QuotaSnapshot,
    target: Option<&ai_usage::QuotaTarget>,
    now_unix_ms: i64,
) -> Vec<String> {
    let mut lines = vec![snapshot.provider_name.to_string()];
    let account = snapshot
        .account
        .safe_label
        .as_deref()
        .unwrap_or(snapshot.account.fingerprint.as_ref());
    lines[0].push_str(&format!(" · {account}"));

    let model = target
        .and_then(|target| target.model_name.as_deref())
        .or_else(|| snapshot.active_model_id.as_deref());
    if let Some(model) = model {
        lines[0].push_str(&format!(" · {model}"));
    }
    for window in all_items(snapshot) {
        let mut line = format!("{}: {}", window.label, window_value(&window));

        if let Some(reset) = reset_value(&window, now_unix_ms) {
            line.push_str(&format!(" · resets in {reset}"));
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RingVisibility {
    weekly: bool,
    five_hour: bool,
    context: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RingAvailability {
    weekly: bool,
    five_hour: bool,
    context: bool,
}

fn quota_tone(remaining_percent: f64) -> RingTone {
    if remaining_percent < 20.0 {
        RingTone::Critical
    } else if remaining_percent <= 50.0 {
        RingTone::Warning
    } else {
        RingTone::Normal
    }
}

fn context_tone(used_ratio: f32) -> RingTone {
    if used_ratio >= 0.85 {
        RingTone::Warning
    } else {
        RingTone::Normal
    }
}

fn window_kind(window: &QuotaWindow) -> Option<RingKind> {
    match window.window_seconds {
        Some(WEEKLY_SECONDS) => Some(RingKind::Weekly),
        Some(FIVE_HOUR_SECONDS) => Some(RingKind::FiveHour),
        _ => None,
    }
}

fn applicable_quota_window(snapshot: &QuotaSnapshot, kind: RingKind) -> Option<&QuotaWindow> {
    snapshot
        .applicable_windows()
        .into_iter()
        .filter(|window| window_kind(window) == Some(kind))
        .filter(|window| window.remaining_percent.is_some())
        .min_by(|left, right| {
            left.remaining_percent
                .unwrap_or(f64::INFINITY)
                .total_cmp(&right.remaining_percent.unwrap_or(f64::INFINITY))
        })
}

fn available_rings(snapshot: &QuotaSnapshot, context_available: bool) -> RingAvailability {
    RingAvailability {
        weekly: applicable_quota_window(snapshot, RingKind::Weekly).is_some(),
        five_hour: applicable_quota_window(snapshot, RingKind::FiveHour).is_some(),
        context: context_available,
    }
}

fn effective_visibility(
    visibility: RingVisibility,
    availability: RingAvailability,
) -> Vec<RingKind> {
    let mut rings = [
        (RingKind::Weekly, visibility.weekly, availability.weekly),
        (
            RingKind::FiveHour,
            visibility.five_hour,
            availability.five_hour,
        ),
        (RingKind::Context, visibility.context, availability.context),
    ]
    .into_iter()
    .filter_map(|(kind, visible, available)| visible.then_some(kind).filter(|_| available))
    .collect::<Vec<_>>();

    if rings.is_empty() {
        rings = [
            (RingKind::Context, availability.context),
            (RingKind::FiveHour, availability.five_hour),
            (RingKind::Weekly, availability.weekly),
        ]
        .into_iter()
        .find_map(|(kind, available)| available.then_some(vec![kind]))
        .unwrap_or_default();
    }

    rings
}

fn compact_rings(
    snapshot: &QuotaSnapshot,
    visibility: RingVisibility,
    context_available: bool,
) -> Vec<RingKind> {
    effective_visibility(visibility, available_rings(snapshot, context_available))
}

fn quota_bar_kinds(rings: &[RingKind]) -> Vec<RingKind> {
    [RingKind::FiveHour, RingKind::Weekly]
        .into_iter()
        .filter(|kind| rings.contains(kind))
        .collect()
}

fn has_visible_content(
    snapshot: Option<&QuotaSnapshot>,
    visibility: RingVisibility,
    context_available: bool,
) -> bool {
    context_available
        || snapshot.is_some_and(|snapshot| {
            !compact_rings(snapshot, visibility, context_available).is_empty()
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContextRingLayout {
    Single,
    SplitSideBySide,
}

fn context_ring_layout(show_split: bool, rings: &[RingKind]) -> ContextRingLayout {
    if rings == [RingKind::Context] && show_split {
        ContextRingLayout::SplitSideBySide
    } else {
        ContextRingLayout::Single
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_usage::{QuotaAccountSummary, QuotaBucket, QuotaGroup};
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

    fn all_visible() -> RingVisibility {
        RingVisibility {
            weekly: true,
            five_hour: true,
            context: true,
        }
    }

    #[test]
    fn quota_tones_match_remaining_and_used_boundaries() {
        assert_eq!(quota_tone(51.0), RingTone::Normal);
        assert_eq!(quota_tone(50.0), RingTone::Warning);
        assert_eq!(quota_tone(20.0), RingTone::Warning);
        assert_eq!(quota_tone(19.99), RingTone::Critical);
        assert_eq!(quota_tone(100.0 - 49.0), RingTone::Normal);
        assert_eq!(quota_tone(100.0 - 50.0), RingTone::Warning);
        assert_eq!(quota_tone(100.0 - 80.0), RingTone::Warning);
        assert_eq!(quota_tone(100.0 - 81.0), RingTone::Critical);
    }

    #[test]
    fn context_tone_keeps_existing_eighty_five_percent_warning() {
        assert_eq!(context_tone(0.849), RingTone::Normal);
        assert_eq!(context_tone(0.85), RingTone::Warning);
        assert_eq!(context_tone(1.0), RingTone::Warning);
    }

    #[test]
    fn compact_selection_tracks_weekly_five_hour_context() {
        let snapshot = snapshot(vec![
            quota("5h", FIVE_HOUR_SECONDS, 70.0),
            quota("weekly", WEEKLY_SECONDS, 80.0),
        ]);

        assert_eq!(
            compact_rings(&snapshot, all_visible(), true),
            vec![RingKind::Weekly, RingKind::FiveHour, RingKind::Context]
        );
    }

    #[test]
    fn missing_five_hour_creates_no_placeholder() {
        let snapshot = snapshot(vec![quota("weekly", WEEKLY_SECONDS, 80.0)]);

        assert_eq!(
            compact_rings(&snapshot, all_visible(), true),
            vec![RingKind::Weekly, RingKind::Context]
        );
    }

    #[test]
    fn duplicate_applicable_weekly_uses_lowest_remaining() {
        let snapshot = snapshot(vec![
            quota("weekly-high", WEEKLY_SECONDS, 80.0),
            quota("weekly-low", WEEKLY_SECONDS, 30.0),
        ]);

        assert_eq!(
            applicable_quota_window(&snapshot, RingKind::Weekly)
                .map(|window| window.label.as_ref()),
            Some("weekly-low")
        );
    }

    #[test]
    fn inactive_named_group_never_changes_ring() {
        let mut snapshot = snapshot(vec![quota("weekly", WEEKLY_SECONDS, 80.0)]);
        snapshot.groups.push(QuotaGroup {
            id: Arc::from("inactive"),
            display_name: "Inactive".into(),
            description: None,
            applies_to_model_ids: vec![Arc::from("inactive-model")],
            affects_severity: true,
            buckets: vec![QuotaBucket {
                id: Arc::from("inactive-weekly"),
                display_name: "Inactive weekly".into(),
                description: None,
                window: quota("inactive-weekly", WEEKLY_SECONDS, 1.0),
            }],
        });

        assert_eq!(
            applicable_quota_window(&snapshot, RingKind::Weekly)
                .and_then(|window| window.remaining_percent),
            Some(80.0)
        );
    }

    #[test]
    fn hiding_ring_changes_only_compact_selection() {
        let snapshot = snapshot(vec![
            quota("5h", FIVE_HOUR_SECONDS, 70.0),
            quota("weekly", WEEKLY_SECONDS, 80.0),
        ]);
        let mut visibility = all_visible();
        visibility.weekly = false;

        assert_eq!(
            compact_rings(&snapshot, visibility, true),
            vec![RingKind::FiveHour, RingKind::Context]
        );
        assert_eq!(
            available_rings(&snapshot, true),
            RingAvailability {
                weekly: true,
                five_hour: true,
                context: true,
            }
        );
    }

    #[test]
    fn no_context_or_quota_snapshot_has_no_indicator_content() {
        let snapshot = snapshot(vec![quota("weekly", WEEKLY_SECONDS, 80.0)]);

        assert!(!has_visible_content(None, all_visible(), false));
        assert!(has_visible_content(Some(&snapshot), all_visible(), false));
        assert!(has_visible_content(None, all_visible(), true));
    }

    #[test]
    fn hover_lines_include_quota_windows_hidden_from_compact_rings() {
        let snapshot = snapshot(vec![
            quota("5h", FIVE_HOUR_SECONDS, 70.0),
            quota("weekly", WEEKLY_SECONDS, 80.0),
        ]);

        let lines = quota_lines(&snapshot, None, current_unix_ms());

        assert!(lines.iter().any(|line| line.starts_with("5h:")));
        assert!(lines.iter().any(|line| line.starts_with("weekly:")));
    }

    #[test]
    fn hover_uses_safe_account_fingerprint_fallback() {
        let mut snapshot = snapshot_fetched_at(0);
        snapshot.account.safe_label = None;
        snapshot.account.fingerprint = Arc::from("user_fingerprint");
        let lines = quota_lines(&snapshot, None, 0);
        assert!(lines[0].contains("user_fingerprint"));
    }

    #[test]
    fn hover_uses_target_model_display_name() {
        let mut snapshot = snapshot_fetched_at(0);
        snapshot.active_model_id = Some(Arc::from("normalized-id"));
        let target = ai_usage::QuotaTarget {
            kind: ai_usage::QuotaTargetKind::ExternalAgent,
            provider_or_agent_id: Arc::from("provider"),
            upstream_provider_id: None,
            model_id: None,
            model_name: Some("Display Model".into()),
        };
        let lines = quota_lines(&snapshot, Some(&target), 0);
        assert!(lines[0].contains("Display Model"));
        assert!(!lines[0].contains("normalized-id"));
    }

    #[test]
    fn hover_retains_rows_beyond_compact_ring_limit() {
        let mut snapshot = snapshot_fetched_at(0);
        for i in 0..5 {
            snapshot
                .windows
                .push(quota(&format!("window {i}"), WEEKLY_SECONDS, 80.0));
        }
        let lines = quota_lines(&snapshot, None, 0);
        assert!(lines.iter().any(|l| l.starts_with("window 4:")));
    }

    #[test]
    fn effective_visibility_always_selects_one_available_ring() {
        let visibility = RingVisibility {
            weekly: false,
            five_hour: false,
            context: false,
        };

        assert_eq!(
            effective_visibility(
                visibility,
                RingAvailability {
                    weekly: true,
                    five_hour: true,
                    context: true,
                },
            ),
            vec![RingKind::Context]
        );
        assert_eq!(
            effective_visibility(
                visibility,
                RingAvailability {
                    weekly: true,
                    five_hour: false,
                    context: false,
                },
            ),
            vec![RingKind::Weekly]
        );
    }

    #[test]
    fn context_only_split_mode_remains_two_side_by_side_rings() {
        assert_eq!(
            context_ring_layout(true, &[RingKind::Context]),
            ContextRingLayout::SplitSideBySide
        );
        assert_eq!(split_context_tone(85, 100), RingTone::Warning);
        assert_eq!(split_context_tone(50, 100), RingTone::Normal);
        assert_eq!(
            context_ring_layout(true, &[RingKind::Weekly, RingKind::Context]),
            ContextRingLayout::Single
        );
    }

    #[test]
    fn quota_bars_render_five_hour_above_weekly() {
        let rings = vec![RingKind::Weekly, RingKind::FiveHour, RingKind::Context];

        assert_eq!(
            quota_bar_kinds(&rings),
            vec![RingKind::FiveHour, RingKind::Weekly]
        );
    }

    #[test]
    fn missing_quota_bars_are_omitted_without_placeholders() {
        assert_eq!(
            quota_bar_kinds(&[RingKind::Weekly, RingKind::Context]),
            vec![RingKind::Weekly]
        );
        assert_eq!(
            quota_bar_kinds(&[RingKind::FiveHour, RingKind::Context]),
            vec![RingKind::FiveHour]
        );
        assert!(quota_bar_kinds(&[RingKind::Context]).is_empty());
    }

    #[test]
    fn quota_bar_fill_follows_remaining_and_used_modes() {
        let window = quota("weekly", WEEKLY_SECONDS, 75.0);

        assert_eq!(
            quota_fill(&window, settings::QuotaDisplayMode::Remaining),
            Some(75.0)
        );
        assert_eq!(
            quota_fill(&window, settings::QuotaDisplayMode::Used),
            Some(25.0)
        );
    }
}
