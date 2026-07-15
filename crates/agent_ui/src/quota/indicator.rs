use std::sync::Arc;

use acp_thread::{SessionCost, TokenUsage};
use agent_settings::{AgentSettings, QuotaRingVisibility};
use ai_usage::{QuotaSnapshot, QuotaView, QuotaWindow};
use fs::Fs;
use gpui::{App, Context, Div, Entity, IntoElement, Render, Subscription, Window, div, px};
use project::ProjectEntryId;
use settings::{Settings, SettingsStore};
use ui::{ButtonLike, CircularProgress, Icon, PopoverMenu, PopoverMenuHandle, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use super::{
    ActiveQuotaTarget,
    button::{current_unix_ms, relative_age},
    popover::QuotaPopover,
};

const FIVE_HOUR_SECONDS: u64 = 18_000;
const WEEKLY_SECONDS: u64 = 604_800;

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
                self.render_rings(&rings, snapshot, display_mode, cx)
            })
            .hoverable_tooltip({
                let context_usage = self.context_usage.clone();
                let quota_lines = snapshot.map(|snapshot| quota_lines(snapshot, &rings));
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

    fn render_context_only(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(context_usage) = self.context_usage.as_ref() else {
            return div().size(px(16.)).into_any_element();
        };
        let usage = &context_usage.token_usage;
        let used_ratio = context_used_ratio(usage);
        let display_mode = AgentSettings::get_global(cx).quota.display_mode;
        let value = context_fill(used_ratio, display_mode);
        let tone = context_tone(used_ratio);
        let max_output_tokens = usage.max_output_tokens.unwrap_or(0);
        let input_max_tokens = usage.max_tokens.saturating_sub(max_output_tokens);
        let layout = context_ring_layout(context_usage.show_split, &[RingKind::Context]);
        let ring = |value, tone| {
            CircularProgress::new(value, 100.0, px(16.), cx)
                .stroke_width(px(2.))
                .radius(px(6.))
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
            ring(value, tone).into_any_element()
        }
    }

    fn render_rings(
        &self,
        rings: &[RingKind],
        snapshot: Option<&QuotaSnapshot>,
        display_mode: settings::QuotaDisplayMode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let size = px(16. + (rings.len().saturating_sub(1) * 4) as f32);
        let mut container = div().relative().size(size);
        for (index, kind) in rings.iter().enumerate() {
            let radius = px(6. + ((rings.len() - index - 1) * 2) as f32);
            let progress = match kind {
                RingKind::Context => self.context_usage.as_ref().map(|context| {
                    context_fill(context_used_ratio(&context.token_usage), display_mode)
                }),
                RingKind::Weekly | RingKind::FiveHour => snapshot
                    .and_then(|snapshot| applicable_quota_window(snapshot, *kind))
                    .and_then(|window| quota_fill(window, display_mode)),
            };
            let Some(progress) = progress else {
                continue;
            };
            let tone = match kind {
                RingKind::Context => self
                    .context_usage
                    .as_ref()
                    .map(|context| context_tone(context_used_ratio(&context.token_usage)))
                    .unwrap_or(RingTone::Normal),
                RingKind::Weekly | RingKind::FiveHour => snapshot
                    .and_then(|snapshot| applicable_quota_window(snapshot, *kind))
                    .and_then(|window| window.remaining_percent)
                    .map(quota_tone)
                    .unwrap_or(RingTone::Normal),
            };
            container = container.child(
                div().absolute().inset_0().child(
                    CircularProgress::new(progress, 100., size, cx)
                        .stroke_width(px(2.))
                        .radius(radius)
                        .progress_color(tone_color(tone, cx)),
                ),
            );
        }
        container.into_any_element()
    }
}

impl Render for ContextQuotaIndicator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = AgentSettings::get_global(cx).quota.visible_rings;
        self.render_indicator(self.view(cx).as_ref(), settings, window, cx)
    }
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

fn quota_lines(snapshot: &QuotaSnapshot, rings: &[RingKind]) -> Vec<String> {
    let mut lines = rings
        .iter()
        .filter_map(|kind| {
            let window = applicable_quota_window(snapshot, *kind)?;
            let remaining = window.remaining_percent?;
            let used = window.used_percent.unwrap_or_else(|| 100. - remaining);
            Some(format!(
                "{}: {:.0}% remaining · {:.0}% used",
                window.label, remaining, used
            ))
        })
        .collect::<Vec<_>>();
    lines.push(format!(
        "Last quota success: {} ago",
        relative_age(snapshot.fetched_at_unix_ms, current_unix_ms())
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

fn applicable_quota_window<'a>(
    snapshot: &'a QuotaSnapshot,
    kind: RingKind,
) -> Option<&'a QuotaWindow> {
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
                fingerprint: Arc::from("account"),
                safe_label: None,
            },
            plan: None,
            fetched_at_unix_ms: 0,
            active_model_id: Some(Arc::from("active")),
            windows,
            model_windows: HashMap::default(),
            groups: Vec::new(),
            available_resets: None,
        }
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
    fn rings_order_weekly_five_hour_context() {
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
}
