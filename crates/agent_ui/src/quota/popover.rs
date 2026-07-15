use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::Arc,
};

use agent_settings::{AgentSettings, QuotaRingVisibility};
use ai_usage::{QuotaError, QuotaResetSummary, QuotaSnapshot, QuotaTarget, QuotaView, QuotaWindow};
use chrono::{DateTime, Local, Utc};
use collections::HashMap;
use fs::Fs;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Render, Subscription, WeakEntity, Window, prelude::*,
};
use settings::{Settings as _, SettingsStore, update_settings_file};
use ui::{
    ButtonLike, Checkbox, CommonAnimationExt, Disclosure, IconButton, ToggleButtonGroup,
    ToggleButtonSimple, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    ActiveQuotaTarget,
    indicator::{
        ContextQuotaIndicator, ContextUsageData, absolute_time, current_unix_ms, relative_age,
        reset_value, window_value,
    },
};

const FIVE_HOUR_SECONDS: u64 = 18_000;
const WEEKLY_SECONDS: u64 = 604_800;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmptyPopoverContent {
    Unavailable,
    Context,
}

fn empty_popover_content(has_context: bool) -> EmptyPopoverContent {
    if has_context {
        EmptyPopoverContent::Context
    } else {
        EmptyPopoverContent::Unavailable
    }
}

fn active_target_changed(current: Option<&QuotaTarget>, next: &QuotaTarget) -> bool {
    current != Some(next)
}

pub(crate) struct QuotaPopover {
    active_target: Option<QuotaTarget>,
    targets: Vec<QuotaTarget>,
    context_usage: Option<ContextUsageData>,
    context_usage_source: Option<WeakEntity<ContextQuotaIndicator>>,
    store: Entity<ai_usage::QuotaStore>,
    fs: Option<Arc<dyn Fs>>,
    expanded: HashSet<String>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl QuotaPopover {
    pub(crate) fn new_with_context_source(
        active_target: Option<ActiveQuotaTarget>,
        context_usage: Option<ContextUsageData>,
        store: Entity<ai_usage::QuotaStore>,
        fs: Option<Arc<dyn Fs>>,
        context_usage_source: Option<WeakEntity<ContextQuotaIndicator>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_target = active_target.map(|target| target.0);
        let targets = store.update(cx, |store, cx| {
            let targets = store.targets_for_popover(active_target.as_ref());
            for target in &targets {
                store.refresh(target.clone(), false, cx);
            }
            targets
        });
        let expanded = active_target
            .as_ref()
            .map(|target| HashSet::from([provider_key(target)]))
            .unwrap_or_default();
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());
        let settings_subscription = cx.observe_global::<SettingsStore>(|_, cx| cx.notify());
        let context_subscription = context_usage_source
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .map(|source| cx.observe(&source, |_, _, cx| cx.notify()));
        let mut subscriptions = vec![store_subscription, settings_subscription];
        if let Some(context_subscription) = context_subscription {
            subscriptions.push(context_subscription);
        }

        Self {
            active_target,
            targets,
            context_usage,
            context_usage_source,
            store,
            fs,
            expanded,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }
}

impl EventEmitter<DismissEvent> for QuotaPopover {}

impl Focusable for QuotaPopover {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QuotaPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_context_source(cx);

        let mut views = HashMap::default();
        for target in &self.targets {
            if let Some(view) = self.store.update(cx, |store, _cx| store.view(target)) {
                views.insert(target.clone(), view);
            }
        }
        let rows = provider_rows(&self.targets, self.active_target.as_ref(), &views);
        let context_usage = self.current_context_usage(cx);
        let standalone_context = should_render_standalone_context(
            self.active_target.as_ref(),
            &rows,
            context_usage.is_some(),
        );
        let any_fetching = views.values().any(|view| view.is_fetching);
        let active_snapshot = self
            .active_target
            .as_ref()
            .and_then(|target| views.get(target))
            .and_then(|view| view.snapshot.as_ref())
            .or_else(|| rows.first().and_then(|row| row.view.snapshot.as_ref()))
            .cloned();
        let settings = AgentSettings::get_global(cx).quota.clone();
        let popover = cx.entity().downgrade();
        let refresh_icon = Icon::new(IconName::RotateCw)
            .size(IconSize::Small)
            .color(Color::Muted);
        let refresh_icon = if any_fetching {
            refresh_icon.with_rotate_animation(1).into_any_element()
        } else {
            refresh_icon.into_any_element()
        };

        let now_unix_ms = current_unix_ms();

        v_flex()
            .elevation_2(cx)
            .w(px(360.))
            .max_w(px(420.))
            .max_h(px(640.))
            .id("quota-popover-scroll")
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .child(if rows.is_empty() {
                match empty_popover_content(context_usage.is_some()) {
                    EmptyPopoverContent::Context => {
                        if let Some(context_usage) = context_usage.as_ref() {
                            v_flex()
                                .gap_2()
                                .child(render_context_usage(context_usage, cx))
                                .into_any_element()
                        } else {
                            v_flex()
                                .gap_1()
                                .child(Label::new("Quota unavailable"))
                                .child(
                                    Label::new("No connected provider has quota data.")
                                        .color(Color::Muted),
                                )
                                .into_any_element()
                        }
                    }
                    EmptyPopoverContent::Unavailable => v_flex()
                        .gap_1()
                        .child(Label::new("Quota unavailable"))
                        .child(
                            Label::new("No connected provider has quota data.").color(Color::Muted),
                        )
                        .into_any_element(),
                }
            } else {
                v_flex()
                    .gap_2()
                    .children(rows.into_iter().map(|row| {
                        self.render_provider(row, now_unix_ms, context_usage.as_ref(), cx)
                    }))
                    .when_some(
                        context_usage.as_ref().filter(|_| standalone_context),
                        |this, context_usage| this.child(render_context_usage(context_usage, cx)),
                    )
                    .into_any_element()
            })
            .child(self.render_settings(
                settings,
                active_snapshot.as_ref(),
                context_usage.is_some(),
                cx,
            ))
            .child(
                h_flex().justify_end().child(
                    ButtonLike::new("refresh-quota")
                        .aria_label("Refresh quota")
                        .disabled(any_fetching)
                        .child(refresh_icon)
                        .on_click(move |_, _, cx| {
                            popover
                                .update(cx, |popover, cx| {
                                    let targets = popover.targets.clone();
                                    let store = popover.store.clone();
                                    store.update(cx, |store, cx| {
                                        for target in targets {
                                            store.refresh(target, true, cx);
                                        }
                                    });
                                    cx.notify();
                                })
                                .log_err();
                        }),
                ),
            )
            .into_any_element()
    }
}

impl QuotaPopover {
    fn sync_context_source(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.context_usage_source.as_ref() else {
            return;
        };
        let Ok(active_target) = source.read_with(cx, |indicator, _| indicator.active_target())
        else {
            return;
        };
        if !active_target_changed(self.active_target.as_ref(), &active_target.0) {
            return;
        }

        self.active_target = Some(active_target.0);
        let Some(active_target) = self.active_target.clone() else {
            return;
        };
        self.targets = self.store.update(cx, |store, cx| {
            let targets = store.targets_for_popover(Some(&active_target));
            for target in &targets {
                store.refresh(target.clone(), false, cx);
            }
            targets
        });
        self.expanded.insert(provider_key(&active_target));
    }

    fn current_context_usage(&self, cx: &App) -> Option<ContextUsageData> {
        self.context_usage_source
            .as_ref()
            .and_then(|source| {
                source
                    .read_with(cx, |indicator, _| indicator.context_usage())
                    .ok()
            })
            .flatten()
            .or_else(|| self.context_usage.clone())
    }

    fn render_provider(
        &mut self,
        row: ProviderRow,
        now_unix_ms: i64,
        context_usage: Option<&ContextUsageData>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = provider_key(&row.target);
        let expanded = row.expanded || self.expanded.contains(&key);
        let disclosure = Disclosure::new(key.clone(), expanded).on_click({
            let key = key.clone();
            let entity = cx.entity().downgrade();
            move |_, _, cx| {
                entity
                    .update(cx, |popover, cx| {
                        if !popover.expanded.insert(key.clone()) {
                            popover.expanded.remove(&key);
                        }
                        cx.notify();
                    })
                    .log_err();
            }
        });

        let mut heading = row
            .view
            .snapshot
            .as_ref()
            .map(|snapshot| provider_heading(snapshot, &row.target, now_unix_ms))
            .unwrap_or_else(|| row.view.provider_name.to_string());
        let warning = row
            .view
            .snapshot
            .is_some()
            .then(|| row.view.error.as_ref())
            .flatten();
        let warning_icon = warning.map(|error| {
            IconButton::new(format!("{key}-warning"), IconName::Warning)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Warning)
                .aria_label("Quota refresh warning")
                .tooltip(Tooltip::text(sanitized_error(error)))
        });
        if warning_icon.is_some() {
            heading.push(' ');
        }

        let header = h_flex()
            .gap_1()
            .child(disclosure)
            .child(
                v_flex()
                    .flex_1()
                    .gap_0p5()
                    .child(Label::new(heading))
                    .when_some(row.view.snapshot.as_ref(), |this, snapshot| {
                        this.child(provider_subheading(snapshot, &row.target, now_unix_ms))
                    }),
            )
            .when_some(warning_icon, |this, warning| this.child(warning));

        v_flex()
            .gap_2()
            .child(header)
            .when(expanded, |this| {
                this.child(self.render_provider_body(&row.view, now_unix_ms, context_usage, cx))
            })
            .into_any_element()
    }

    fn render_provider_body(
        &self,
        view: &QuotaView,
        now_unix_ms: i64,
        context_usage: Option<&ContextUsageData>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(snapshot) = view.snapshot.as_ref() else {
            let mut content = v_flex()
                .gap_1()
                .child(Label::new("Quota unavailable"))
                .child(Label::new("No quota data is available yet.").color(Color::Muted));
            if let Some(error) = provider_error_text(view) {
                content = content.child(Label::new(error).color(Color::Muted));
            }
            return content.into_any_element();
        };

        let mut content = v_flex().gap_2();
        if let Some(plan) = snapshot.plan.as_ref() {
            content = content.child(
                h_flex()
                    .gap_1()
                    .child(Label::new("Plan").color(Color::Muted))
                    .child(Label::new(plan.clone())),
            );
        }
        content = content.child(render_section("Quota", &snapshot.windows, now_unix_ms));
        if let Some(active_model_id) = snapshot.active_model_id.as_ref()
            && let Some(windows) = snapshot.model_windows.get(active_model_id)
        {
            content = content.child(render_section("Active model", windows, now_unix_ms));
        }
        for group in &snapshot.groups {
            let windows = group
                .buckets
                .iter()
                .map(|bucket| bucket.window.clone())
                .collect::<Vec<_>>();
            content = content.child(render_section(&group.display_name, &windows, now_unix_ms));
        }
        if let Some(error) = provider_error_text(view) {
            content = content.child(
                v_flex()
                    .gap_1()
                    .child(Label::new("Last quota could not be refreshed").color(Color::Warning))
                    .child(Label::new(error).color(Color::Muted)),
            );
        }
        if let Some(resets) = snapshot.available_resets.as_ref() {
            content = content.child(render_resets(resets, now_unix_ms));
        }
        if let Some(context_usage) = context_usage {
            content = content.child(render_context_usage(context_usage, cx));
        }
        content.into_any_element()
    }

    fn render_settings(
        &self,
        settings: agent_settings::AgentQuotaSettings,
        snapshot: Option<&QuotaSnapshot>,
        context_available: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let availability = ring_availability(snapshot, context_available);
        let visible = settings.visible_rings;
        let selected_available = [
            (visible.weekly, availability.weekly),
            (visible.five_hour, availability.five_hour),
            (visible.context, availability.context),
        ]
        .into_iter()
        .filter(|(selected, available)| *selected && *available)
        .count();
        let fs = self.fs.clone();
        let display_mode = ToggleButtonGroup::single_row(
            "quota-display-mode",
            [
                ToggleButtonSimple::new("Remaining", {
                    let fs = fs.clone();
                    move |_, _, cx| {
                        update_display_mode(fs.clone(), settings::QuotaDisplayMode::Remaining, cx)
                    }
                })
                .selected(settings.display_mode == settings::QuotaDisplayMode::Remaining),
                ToggleButtonSimple::new("Used", {
                    let fs = fs.clone();
                    move |_, _, cx| {
                        update_display_mode(fs.clone(), settings::QuotaDisplayMode::Used, cx)
                    }
                })
                .selected(settings.display_mode == settings::QuotaDisplayMode::Used),
            ],
        )
        .auto_width();

        let checkbox = |id: &'static str,
                        label: &'static str,
                        selected: bool,
                        available: bool,
                        last_selected: bool,
                        setting: RingSetting| {
            let fs = self.fs.clone();
            Checkbox::new(id, selected.into())
                .disabled(!available || (selected && last_selected))
                .on_click(move |state, _, cx| {
                    if let Some(fs) = fs.clone() {
                        let selected = matches!(state, ToggleState::Selected);
                        update_settings_file(fs, cx, move |settings, _| {
                            let quota = settings
                                .agent
                                .get_or_insert_default()
                                .quota
                                .get_or_insert_default();
                            match setting {
                                RingSetting::Weekly => quota.show_weekly_ring = Some(selected),
                                RingSetting::FiveHour => quota.show_five_hour_ring = Some(selected),
                                RingSetting::Context => quota.show_context_ring = Some(selected),
                            }
                        });
                    }
                })
                .label(label)
        };

        let weekly = checkbox(
            "quota-weekly-ring",
            "Weekly",
            visible.weekly,
            availability.weekly,
            selected_available == 1 && visible.weekly,
            RingSetting::Weekly,
        );
        let five_hour = checkbox(
            "quota-five-hour-ring",
            "5-hour",
            visible.five_hour,
            availability.five_hour,
            selected_available == 1 && visible.five_hour,
            RingSetting::FiveHour,
        );
        let context = checkbox(
            "quota-context-ring",
            "Context",
            visible.context,
            availability.context,
            selected_available == 1 && visible.context,
            RingSetting::Context,
        );

        v_flex()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .pt_2()
            .child(Label::new("Display").weight(FontWeight::MEDIUM))
            .child(display_mode)
            .child(Label::new("Compact rings").weight(FontWeight::MEDIUM))
            .child(h_flex().gap_2().children([weekly, five_hour, context]))
            .into_any_element()
    }
}

fn format_duration_label(value: String) -> String {
    format!("resets in {value}")
}

struct ProviderRow {
    target: QuotaTarget,
    view: QuotaView,
    expanded: bool,
}

fn should_render_standalone_context(
    active_target: Option<&QuotaTarget>,
    rows: &[ProviderRow],
    context_available: bool,
) -> bool {
    context_available
        && active_target
            .is_some_and(|active_target| !rows.iter().any(|row| row.target == *active_target))
}

#[derive(Clone, Copy)]
enum RingSetting {
    Weekly,
    FiveHour,
    Context,
}

#[cfg(test)]
struct ProviderWindow {
    label: String,
}

struct ResetRow {
    text: String,
}

struct ResetModel {
    available_count: u64,
    rows: Vec<ResetRow>,
}

fn provider_rows(
    targets: &[QuotaTarget],
    active: Option<&QuotaTarget>,
    views: &HashMap<QuotaTarget, QuotaView>,
) -> Vec<ProviderRow> {
    let mut ordered_targets = Vec::with_capacity(targets.len());
    if let Some(active) = active
        && targets.contains(active)
    {
        ordered_targets.push(active.clone());
    }
    ordered_targets.extend(
        targets
            .iter()
            .filter(|target| Some(*target) != active)
            .cloned(),
    );
    ordered_targets
        .into_iter()
        .filter_map(|target| {
            let view = views.get(&target)?.clone();
            if view.snapshot.is_none()
                && (view.account.is_none()
                    || view
                        .error
                        .as_ref()
                        .is_some_and(QuotaError::disconnects_provider))
            {
                return None;
            }
            Some(ProviderRow {
                expanded: active == Some(&target),
                target,
                view,
            })
        })
        .collect()
}

fn provider_key(target: &QuotaTarget) -> String {
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    format!("quota-provider-{:016x}", hasher.finish())
}

fn provider_error_text(view: &QuotaView) -> Option<String> {
    let error = view.error.as_ref()?;
    if view.snapshot.is_some() || (view.account.is_some() && !error.disconnects_provider()) {
        Some(sanitized_error(error))
    } else {
        None
    }
}

fn provider_heading(snapshot: &QuotaSnapshot, target: &QuotaTarget, _now_unix_ms: i64) -> String {
    let account = snapshot
        .account
        .safe_label
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| snapshot.account.fingerprint.to_string());
    let model = target
        .model_name
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| snapshot.active_model_id.as_ref().map(ToString::to_string));
    match model {
        Some(model) => format!("{} · {} · {}", snapshot.provider_name, account, model),
        None => format!("{} · {}", snapshot.provider_name, account),
    }
}

fn provider_subheading(
    snapshot: &QuotaSnapshot,
    _target: &QuotaTarget,
    now_unix_ms: i64,
) -> impl IntoElement {
    Label::new(format!(
        "Last success {} · {} ago",
        absolute_time(snapshot.fetched_at_unix_ms),
        relative_age(snapshot.fetched_at_unix_ms, now_unix_ms)
    ))
    .size(LabelSize::Small)
    .color(Color::Muted)
}

#[cfg(test)]
fn provider_windows(snapshot: &QuotaSnapshot) -> Vec<ProviderWindow> {
    let mut windows = snapshot
        .windows
        .iter()
        .map(|window| ProviderWindow {
            label: window.label.to_string(),
        })
        .collect::<Vec<_>>();
    if let Some(active_model_id) = snapshot.active_model_id.as_ref()
        && let Some(model_windows) = snapshot.model_windows.get(active_model_id)
    {
        windows.extend(model_windows.iter().map(|window| ProviderWindow {
            label: window.label.to_string(),
        }));
    }
    for group in &snapshot.groups {
        windows.extend(group.buckets.iter().map(|bucket| ProviderWindow {
            label: bucket.window.label.to_string(),
        }));
    }
    windows
}

fn render_section(title: &str, windows: &[QuotaWindow], now_unix_ms: i64) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(Label::new(title).weight(FontWeight::MEDIUM))
        .children(windows.iter().map(|window| {
            let reset = reset_value(window, now_unix_ms);
            h_flex()
                .justify_between()
                .gap_2()
                .child(Label::new(window.label.clone()).color(Color::Muted))
                .child(Label::new(window_value(window)))
                .when_some(reset, |this, reset| {
                    this.child(Label::new(format_duration_label(reset)).color(Color::Muted))
                })
        }))
}

fn reset_model(summary: Option<&QuotaResetSummary>, now_unix_ms: i64) -> ResetModel {
    let Some(summary) = summary else {
        return ResetModel {
            available_count: 0,
            rows: Vec::new(),
        };
    };
    let rows = summary
        .resets
        .iter()
        .filter(|reset| {
            reset
                .expires_at_unix_ms
                .is_none_or(|expires| expires > now_unix_ms)
        })
        .map(|reset| {
            let expiry = reset.expires_at_unix_ms.and_then(localized_date);
            let text = match expiry.as_ref() {
                Some(expiry) => format!("{} · Expires {expiry}", reset.title),
                None => reset.title.to_string(),
            };
            ResetRow { text }
        })
        .collect::<Vec<_>>();
    ResetModel {
        available_count: summary.available_count,
        rows,
    }
}

fn localized_date(unix_ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(unix_ms)
        .map(|date| date.with_timezone(&Local).format("%x").to_string())
}

fn render_resets(summary: &QuotaResetSummary, now_unix_ms: i64) -> impl IntoElement {
    let model = reset_model(Some(summary), now_unix_ms);
    if model.rows.is_empty() && model.available_count == 0 {
        return v_flex().into_any_element();
    }
    v_flex()
        .gap_1()
        .child(Label::new("Available Resets").weight(FontWeight::MEDIUM))
        .child(Label::new(format!("{} available", model.available_count)).color(Color::Muted))
        .children(model.rows.into_iter().map(|row| Label::new(row.text)))
        .into_any_element()
}

fn sanitized_error(error: &QuotaError) -> String {
    match error {
        QuotaError::MissingCredentials => "Quota credentials are not configured".to_string(),
        QuotaError::AmbiguousAccount => "Quota account could not be identified".to_string(),
        QuotaError::Cooldown => "Quota refresh is cooling down".to_string(),
        QuotaError::Authentication(_) => "Quota authentication failed".to_string(),
        QuotaError::RateLimited { .. } => "Quota provider rate limit reached".to_string(),
        QuotaError::Provider(_) => "Quota provider request failed".to_string(),
    }
}

fn ring_availability(
    snapshot: Option<&QuotaSnapshot>,
    context_available: bool,
) -> QuotaRingVisibility {
    let (weekly, five_hour) = snapshot
        .map(|snapshot| {
            snapshot.applicable_windows().into_iter().fold(
                (false, false),
                |(weekly, five_hour), window| {
                    (
                        weekly || window.window_seconds == Some(WEEKLY_SECONDS),
                        five_hour || window.window_seconds == Some(FIVE_HOUR_SECONDS),
                    )
                },
            )
        })
        .unwrap_or_default();
    QuotaRingVisibility {
        weekly,
        five_hour,
        context: context_available,
    }
}

fn update_display_mode(fs: Option<Arc<dyn Fs>>, mode: settings::QuotaDisplayMode, cx: &App) {
    if let Some(fs) = fs {
        update_settings_file(fs, cx, move |settings, _| {
            settings
                .agent
                .get_or_insert_default()
                .quota
                .get_or_insert_default()
                .display_mode = Some(mode);
        });
    }
}

fn render_context_usage(
    context: &ContextUsageData,
    _cx: &mut Context<QuotaPopover>,
) -> impl IntoElement {
    let used = crate::humanize_token_count(context.token_usage.used_tokens);
    let max = crate::humanize_token_count(context.token_usage.max_tokens);
    let mut content = v_flex()
        .gap_1()
        .child(Label::new("Context").weight(FontWeight::MEDIUM))
        .child(Label::new(format!("{used} / {max} tokens")).color(Color::Muted));
    if let Some(cost) = &context.cost {
        content = content.child(Label::new(format!(
            "Cost: {:.2} {}",
            cost.amount, cost.currency
        )));
    }
    if context.global_agents_md_loaded || context.project_rules_count > 0 {
        content = content.child(Label::new(format!(
            "Rules: {} global, {} project",
            usize::from(context.global_agents_md_loaded),
            context.project_rules_count
        )));
    }
    content
}

#[cfg(test)]
pub(crate) fn render_popover_text(snapshot: &QuotaSnapshot, now_unix_ms: i64) -> String {
    format!(
        "Last updated {} · {} ago",
        absolute_time(snapshot.fetched_at_unix_ms),
        relative_age(snapshot.fetched_at_unix_ms, now_unix_ms)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_usage::{
        QuotaAccountSummary, QuotaError, QuotaReset, QuotaResetSummary, QuotaSnapshot, QuotaTarget,
        QuotaTargetKind,
    };
    use collections::HashMap;
    use std::sync::Arc;

    fn snapshot_fetched_at(fetched_at_unix_ms: i64) -> QuotaSnapshot {
        QuotaSnapshot {
            provider_id: Arc::from("test"),
            provider_name: "Test".into(),
            account: QuotaAccountSummary {
                fingerprint: Arc::from("account"),
                safe_label: Some("Account".into()),
            },
            plan: None,
            fetched_at_unix_ms,
            active_model_id: Some(Arc::from("active")),
            windows: Vec::new(),
            model_windows: HashMap::default(),
            groups: Vec::new(),
            available_resets: None,
        }
    }

    #[test]
    fn tooltip_and_popover_show_last_updated_without_lifecycle_labels() {
        let snapshot = snapshot_fetched_at(1_700_000_000_000);
        let tooltip = super::super::indicator::render_tooltip_text(&snapshot, 1_700_000_018_000);
        let popover = render_popover_text(&snapshot, 1_700_000_018_000);

        assert!(tooltip.contains("Updated 18s ago"));
        assert!(popover.contains("Last updated"));
        assert!(popover.contains("18s ago"));
        assert!(!tooltip.contains("ready"));
        assert!(!tooltip.contains("stale"));
        assert!(!popover.contains("loading"));
    }

    fn target(id: &str) -> QuotaTarget {
        QuotaTarget {
            kind: QuotaTargetKind::ExternalAgent,
            provider_or_agent_id: Arc::from(id),
            upstream_provider_id: None,
            model_id: None,
            model_name: None,
        }
    }

    fn target_with_model(provider_id: &str, model_id: &str) -> QuotaTarget {
        let mut target = target(provider_id);
        target.model_id = Some(Arc::from(model_id));
        target.model_name = Some(model_id.into());
        target
    }

    fn view(snapshot: Option<QuotaSnapshot>, error: Option<QuotaError>) -> QuotaView {
        QuotaView {
            provider_id: Arc::from("provider"),
            provider_name: "Provider".into(),
            account: snapshot.as_ref().map(|snapshot| snapshot.account.clone()),
            snapshot,
            is_fetching: false,
            error,
        }
    }

    #[test]
    fn active_provider_is_first_and_expanded() {
        let active = target_with_model("provider", "active-model");
        let other = target_with_model("provider", "other-model");
        let mut views = HashMap::default();
        views.insert(active.clone(), view(Some(snapshot_fetched_at(0)), None));
        views.insert(other.clone(), view(Some(snapshot_fetched_at(0)), None));

        let rows = provider_rows(&[other.clone(), active.clone()], Some(&active), &views);

        assert_eq!(
            rows.iter().map(|row| &row.target).collect::<Vec<_>>(),
            vec![&active, &other]
        );
        assert!(rows[0].expanded);
        assert_ne!(provider_key(&active), provider_key(&other));
    }

    #[test]
    fn active_target_change_rebuilds_popover_targets() {
        let first = target_with_model("provider", "first");
        let second = target_with_model("provider", "second");

        assert!(active_target_changed(Some(&first), &second));
        assert!(!active_target_changed(Some(&first), &first));
        assert!(active_target_changed(None, &second));
    }

    #[test]
    fn empty_popover_preserves_context_content() {
        assert_eq!(
            empty_popover_content(false),
            EmptyPopoverContent::Unavailable
        );
        assert_eq!(empty_popover_content(true), EmptyPopoverContent::Context);
    }

    #[test]
    fn context_renders_standalone_when_active_target_has_no_provider_row() {
        let active = target("api");
        let connected = target("connected");
        let mut views = HashMap::default();
        views.insert(connected.clone(), view(Some(snapshot_fetched_at(0)), None));
        let rows = provider_rows(std::slice::from_ref(&connected), Some(&active), &views);

        assert!(!rows.is_empty());
        assert!(should_render_standalone_context(Some(&active), &rows, true));
        assert!(!should_render_standalone_context(
            Some(&connected),
            &rows,
            true
        ));
        assert!(!should_render_standalone_context(
            Some(&active),
            &rows,
            false
        ));
    }

    #[test]
    fn other_connected_providers_start_collapsed() {
        let active = target("active");
        let other = target("other");
        let mut views = HashMap::default();
        views.insert(active.clone(), view(Some(snapshot_fetched_at(0)), None));
        views.insert(other.clone(), view(Some(snapshot_fetched_at(0)), None));

        let rows = provider_rows(&[active.clone(), other], Some(&active), &views);

        assert!(!rows[1].expanded);
    }

    #[test]
    fn missing_or_initial_auth_failed_provider_is_filtered() {
        let connected = target("connected");
        let missing = target("missing");
        let auth_failed = target("auth-failed");
        let mut views = HashMap::default();
        views.insert(connected.clone(), view(Some(snapshot_fetched_at(0)), None));
        views.insert(
            missing.clone(),
            view(None, Some(QuotaError::MissingCredentials)),
        );
        views.insert(
            auth_failed.clone(),
            view(None, Some(QuotaError::Authentication("secret".into()))),
        );

        let rows = provider_rows(&[connected.clone(), missing, auth_failed], None, &views);

        assert_eq!(
            rows.iter().map(|row| &row.target).collect::<Vec<_>>(),
            vec![&connected]
        );
    }

    #[test]
    fn cached_provider_with_error_remains_visible() {
        let cached = target("cached");
        let mut views = HashMap::default();
        views.insert(
            cached.clone(),
            view(
                Some(snapshot_fetched_at(0)),
                Some(QuotaError::Provider("temporary failure".into())),
            ),
        );

        let rows = provider_rows(std::slice::from_ref(&cached), None, &views);

        assert_eq!(rows.len(), 1);
        assert!(rows[0].view.snapshot.is_some());

        let mut account_only = view(
            Some(snapshot_fetched_at(0)),
            Some(QuotaError::Provider("temporary failure".into())),
        );
        account_only.snapshot = None;
        assert_eq!(
            provider_error_text(&account_only),
            Some("Quota provider request failed".to_string())
        );
    }

    #[test]
    fn provider_keys_do_not_collide_on_delimiters() {
        let mut left = target_with_model("provider-a", "model");
        left.upstream_provider_id = Some(Arc::from("b"));
        left.model_id = Some(Arc::from("c"));
        left.model_name = Some("d".into());
        let mut right = target_with_model("provider", "model");
        right.upstream_provider_id = Some(Arc::from("a-b"));
        right.model_id = Some(Arc::from("c"));
        right.model_name = Some("d".into());

        assert_ne!(provider_key(&left), provider_key(&right));
    }

    #[test]
    fn hidden_compact_windows_remain_in_provider_rows() {
        let mut snapshot = snapshot_fetched_at(0);
        snapshot.windows = vec![QuotaWindow::value("hidden main", "10 credits")];
        snapshot.active_model_id = Some(Arc::from("active-model"));
        snapshot.model_windows.insert(
            Arc::from("active-model"),
            vec![QuotaWindow::value("hidden active", "20 credits")],
        );
        snapshot.groups.push(ai_usage::QuotaGroup {
            id: Arc::from("credits"),
            display_name: "Credits".into(),
            description: None,
            applies_to_model_ids: Vec::new(),
            affects_severity: false,
            buckets: vec![ai_usage::QuotaBucket {
                id: Arc::from("code-review"),
                display_name: "Code review".into(),
                description: None,
                window: QuotaWindow::value("hidden group", "30 credits"),
            }],
        });

        let rows = provider_windows(&snapshot);

        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["hidden main", "hidden active", "hidden group"]
        );
    }

    #[test]
    fn available_resets_show_count_title_and_expiry_only() {
        let summary = QuotaResetSummary {
            available_count: 2,
            resets: vec![QuotaReset {
                title: "Review credits".into(),
                expires_at_unix_ms: Some(1_735_689_600_000),
            }],
        };

        let model = reset_model(Some(&summary), 1_700_000_000_000);

        assert_eq!(model.available_count, 2);
        assert_eq!(model.rows.len(), 1);
        assert!(model.rows[0].text.starts_with("Review credits · Expires "));
        assert!(!model.rows[0].text.contains("description"));
        assert!(!model.rows[0].text.contains("id"));

        let count_only = reset_model(
            Some(&QuotaResetSummary {
                available_count: 1,
                resets: Vec::new(),
            }),
            1_700_000_000_000,
        );
        assert!(count_only.rows.is_empty());
        assert_eq!(count_only.available_count, 1);
    }

    #[test]
    fn redeemed_and_expired_resets_never_render() {
        let summary = QuotaResetSummary {
            available_count: 1,
            resets: vec![
                QuotaReset {
                    title: "Expired".into(),
                    expires_at_unix_ms: Some(1_699_999_999_999),
                },
                QuotaReset {
                    title: "Available".into(),
                    expires_at_unix_ms: None,
                },
            ],
        };

        let model = reset_model(Some(&summary), 1_700_000_000_000);

        assert_eq!(
            model
                .rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Available"]
        );
        assert!(!model.rows[0].text.contains("Use reset"));
    }

    #[test]
    fn errors_are_sanitized_for_warning_tooltips() {
        let error = QuotaError::Provider("token=secret response body".into());

        let sanitized = sanitized_error(&error);

        assert_eq!(sanitized, "Quota provider request failed");
        assert!(!sanitized.contains("secret"));
    }
}
