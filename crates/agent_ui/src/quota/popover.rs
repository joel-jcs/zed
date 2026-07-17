use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use ai_usage::{QuotaError, QuotaResetSummary, QuotaSnapshot, QuotaTarget, QuotaView, QuotaWindow};
use chrono::{DateTime, Local, Utc};
use collections::HashMap;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Render, Subscription, WeakEntity, Window, prelude::*,
};
use ui::{
    ButtonLike, CircularProgress, CommonAnimationExt, Divider, Icon, IconButton, Tooltip,
    prelude::*,
};
use util::ResultExt as _;

use super::{
    ActiveQuotaTarget,
    indicator::{
        ContextQuotaIndicator, ContextUsageData, absolute_time, current_unix_ms,
        quota_progress_color, relative_age, reset_value, window_display_label, window_value,
    },
};
use editor::{BUFFER_HEADER_PADDING, FILE_HEADER_HEIGHT};

const OTHER_PROVIDERS_KEY: &str = "quota-other-providers";

fn active_target_changed(current: Option<&QuotaTarget>, next: &QuotaTarget) -> bool {
    current != Some(next)
}

pub(crate) struct QuotaPopover {
    active_target: Option<QuotaTarget>,
    targets: Vec<QuotaTarget>,
    context_usage: Option<ContextUsageData>,
    context_usage_source: Option<WeakEntity<ContextQuotaIndicator>>,
    store: Entity<ai_usage::QuotaStore>,
    expanded: HashSet<String>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl QuotaPopover {
    pub(crate) fn new_with_context_source(
        active_target: Option<ActiveQuotaTarget>,
        context_usage: Option<ContextUsageData>,
        store: Entity<ai_usage::QuotaStore>,
        context_usage_source: Option<WeakEntity<ContextQuotaIndicator>>,
        window: &mut Window,
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
        let context_subscription = context_usage_source
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .map(|source| cx.observe(&source, |_, _, cx| cx.notify()));
        let mut subscriptions = vec![store_subscription];
        if let Some(context_subscription) = context_subscription {
            subscriptions.push(context_subscription);
        }

        let focus_handle = cx.focus_handle();
        cx.on_focus_out(&focus_handle, window, |_this, _event, _window, cx| {
            cx.emit(DismissEvent);
        })
        .detach();

        Self {
            active_target,
            targets,
            context_usage,
            context_usage_source,
            store,
            expanded,
            focus_handle,
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_context_source(cx);

        let mut views = HashMap::default();
        for target in &self.targets {
            if let Some(view) = self.store.update(cx, |store, _cx| store.view(target)) {
                views.insert(target.clone(), view);
            }
        }
        let rows = provider_rows(&self.targets, self.active_target.as_ref(), &views);
        let context_usage = self.current_context_usage(cx);
        let any_fetching = views.values().any(|view| view.is_fetching);

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

        let mut content = v_flex()
            .elevation_2(cx)
            .w(px(360.))
            .max_w(px(420.))
            .max_h(px(640.))
            .id("quota-popover-scroll")
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .track_focus(&self.focus_handle);

        if let Some(context_usage) = context_usage.as_ref() {
            content = content.child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new("Context")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(render_context_usage(context_usage)),
            );
            content = content.child(Divider::horizontal());
        }

        let refresh_button = ButtonLike::new("refresh-quota")
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
            });

        let quota_header = h_flex()
            .justify_between()
            .child(
                Label::new("Quota")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(refresh_button);

        content = content.child(quota_header);

        let (active_row, other_rows) = partition_provider_rows(rows);

        if active_row.is_none() && other_rows.is_empty() {
            content = content.child(
                v_flex()
                    .gap_1()
                    .child(Label::new("Quota unavailable"))
                    .child(Label::new("No connected provider has quota data.").color(Color::Muted)),
            );
        } else {
            let mut providers = v_flex().gap_2();
            if let Some(row) = active_row {
                providers = providers.child(self.render_provider(row, now_unix_ms, window, cx));
            } else {
                providers = providers.child(
                    v_flex()
                        .gap_1()
                        .child(Label::new("Quota unavailable").color(Color::Muted))
                        .child(
                            Label::new("Active provider has no quota data.").color(Color::Muted),
                        ),
                );
            }
            if !other_rows.is_empty() {
                let other_expanded = self.expanded.contains(OTHER_PROVIDERS_KEY);
                let toggle_action = {
                    let entity = cx.entity().downgrade();
                    move |_: &gpui::ClickEvent, _window: &mut Window, cx: &mut App| {
                        entity
                            .update(cx, |popover, cx| {
                                popover.toggle_expanded(OTHER_PROVIDERS_KEY, cx);
                            })
                            .log_err();
                    }
                };

                let other_header = ButtonLike::new("other-providers-toggle")
                    .style(ButtonStyle::Transparent)
                    .size(ButtonSize::None)
                    .full_width()
                    .aria_label(if other_expanded {
                        format!("Collapse other providers ({})", other_rows.len())
                    } else {
                        format!("Expand other providers ({})", other_rows.len())
                    })
                    .aria_expanded(other_expanded)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                Icon::new(if other_expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .color(Color::Muted)
                                .size(IconSize::Small),
                            )
                            .child(
                                Label::new(format!("OTHER PROVIDERS ({})", other_rows.len()))
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            ),
                    )
                    .on_click(toggle_action);

                let mut other_content = v_flex().gap_2().child(other_header);
                if other_expanded {
                    other_content = other_content.children(
                        other_rows
                            .into_iter()
                            .map(|row| self.render_provider(row, now_unix_ms, window, cx)),
                    );
                }
                providers = providers.child(other_content);
            }
            content = content.child(providers);
        }

        content.into_any_element()
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

        let previous = self.active_target.clone();
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

        sync_expanded_for_active_target(&mut self.expanded, previous.as_ref(), &active_target);
    }

    fn toggle_expanded(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.insert(key.to_owned()) {
            self.expanded.remove(key);
        }
        cx.notify();
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = provider_key(&row.target);
        let expanded = self.expanded.contains(&key);
        let toggle_action = {
            let key = key.clone();
            let entity = cx.entity().downgrade();
            move |_: &gpui::ClickEvent, _window: &mut Window, cx: &mut App| {
                entity
                    .update(cx, |popover, cx| {
                        popover.toggle_expanded(&key, cx);
                    })
                    .log_err();
            }
        };

        let heading = row
            .view
            .snapshot
            .as_ref()
            .map(provider_heading)
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

        let colors = cx.theme().colors();
        let opaque_window =
            cx.theme().window_background_appearance() == gpui::WindowBackgroundAppearance::Opaque;

        let header_height = FILE_HEADER_HEIGHT as f32 * window.line_height();

        let disclosure_icon = Icon::new(if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .color(Color::Muted)
        .size(IconSize::Small);

        let header_button = ButtonLike::new(format!("{key}-toggle"))
            .style(ButtonStyle::Transparent)
            .size(ButtonSize::None)
            .height(header_height.into())
            .full_width()
            .aria_label(format!(
                "{} provider {}",
                if expanded { "Collapse" } else { "Expand" },
                heading,
            ))
            .aria_expanded(expanded)
            .on_click(toggle_action)
            .child(
                h_flex()
                    .size_full()
                    .p(BUFFER_HEADER_PADDING)
                    .pl_1()
                    .gap_1p5()
                    .child(disclosure_icon)
                    .child(v_flex().min_w_0().flex_1().child(Label::new(heading))),
            );

        let header = h_flex()
            .w_full()
            .h(header_height)
            .rounded_sm()
            .when(opaque_window, |this| {
                this.bg(colors.editor_subheader_background)
            })
            .hover(|this| this.bg(colors.element_hover))
            .child(div().min_w_0().flex_1().child(header_button))
            .when_some(warning_icon, |this, warning| {
                this.child(div().pr_2().child(warning))
            });

        v_flex()
            .w_full()
            .rounded_sm()
            .border_1()
            .border_color(colors.border)
            .child(header)
            .when(expanded, |this| {
                this.child(Divider::horizontal())
                    .child(v_flex().p_3().child(self.render_provider_body(
                        &key,
                        &row.view,
                        row.is_active,
                        now_unix_ms,
                        cx,
                    )))
            })
            .into_any_element()
    }

    fn render_provider_body(
        &self,
        provider_key: &str,
        view: &QuotaView,
        is_active: bool,
        now_unix_ms: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(snapshot) = view.snapshot.as_ref() else {
            let mut content = v_flex()
                .gap_1()
                .child(Label::new("Quota unavailable"))
                .child(Label::new("No quota data is available yet.").color(Color::Muted));
            if let Some(error) = provider_error_text(view, is_active) {
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
                    .child(Label::new(plan.to_uppercase())),
            );
        }
        if !snapshot.windows.is_empty() {
            content = content.child(render_section("Quota", &snapshot.windows, now_unix_ms, cx));
        }
        if let Some(active_model_id) = snapshot.active_model_id.as_ref()
            && let Some(windows) = snapshot.model_windows.get(active_model_id)
            && !windows.is_empty()
        {
            content = content.child(render_section("Active model", windows, now_unix_ms, cx));
        }
        for group in &snapshot.groups {
            if !group.buckets.is_empty() {
                let windows = group
                    .buckets
                    .iter()
                    .map(|bucket| bucket.window.clone())
                    .collect::<Vec<_>>();
                content = content.child(render_section(
                    &group.display_name,
                    &windows,
                    now_unix_ms,
                    cx,
                ));
            }
        }
        if let Some(error) = provider_error_text(view, is_active) {
            content = content.child(
                v_flex()
                    .gap_1()
                    .child(Label::new("Last quota could not be refreshed").color(Color::Warning))
                    .child(Label::new(error).color(Color::Muted)),
            );
        }
        if let Some(resets) = snapshot.available_resets.as_ref() {
            content = content.child(self.render_resets(provider_key, resets, now_unix_ms, cx));
        }
        content = content.child(
            h_flex().w_full().justify_end().child(
                Label::new(format!(
                    "Last refreshed {} · {} ago",
                    absolute_time(snapshot.fetched_at_unix_ms),
                    relative_age(snapshot.fetched_at_unix_ms, now_unix_ms)
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            ),
        );
        content.into_any_element()
    }

    fn render_resets(
        &self,
        provider_key: &str,
        summary: &QuotaResetSummary,
        now_unix_ms: i64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let model = reset_model(Some(summary), now_unix_ms);
        if model.rows.is_empty() && model.available_count == 0 {
            return v_flex().into_any_element();
        }

        let resets_key = format!("{}-resets", provider_key);
        let has_details = !model.rows.is_empty();
        let resets_expanded = has_details && self.expanded.contains(&resets_key);

        let toggle_action = {
            let resets_key = resets_key.clone();
            let entity = cx.entity().downgrade();
            move |_: &gpui::ClickEvent, _window: &mut Window, cx: &mut App| {
                entity
                    .update(cx, |popover, cx| {
                        popover.toggle_expanded(&resets_key, cx);
                    })
                    .log_err();
            }
        };

        v_flex()
            .gap_1()
            .child(Label::new("Available Resets").weight(FontWeight::MEDIUM))
            .child(if has_details {
                ButtonLike::new(resets_key.clone())
                    .style(ButtonStyle::Transparent)
                    .size(ButtonSize::None)
                    .full_width()
                    .aria_label(if resets_expanded {
                        format!("Collapse {} available resets", model.available_count)
                    } else {
                        format!("Expand {} available resets", model.available_count)
                    })
                    .aria_expanded(resets_expanded)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                Icon::new(if resets_expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .color(Color::Muted)
                                .size(IconSize::XSmall),
                            )
                            .child(
                                Label::new(format!("{} available", model.available_count))
                                    .color(Color::Muted),
                            ),
                    )
                    .on_click(toggle_action)
                    .into_any_element()
            } else {
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(format!("{} available", model.available_count))
                            .color(Color::Muted),
                    )
                    .into_any_element()
            })
            .when(resets_expanded, |this| {
                this.child(
                    v_flex()
                        .pl_4()
                        .gap_1()
                        .children(model.rows.into_iter().map(|row| Label::new(row.text))),
                )
            })
            .into_any_element()
    }
}

fn format_duration_label(value: String) -> String {
    format!("Resets in {value}")
}

struct ProviderRow {
    target: QuotaTarget,
    view: QuotaView,
    is_active: bool,
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

fn partition_provider_rows(rows: Vec<ProviderRow>) -> (Option<ProviderRow>, Vec<ProviderRow>) {
    let mut active = None;
    let mut others = Vec::new();

    for row in rows {
        if row.is_active && active.is_none() {
            active = Some(row);
        } else {
            others.push(row);
        }
    }

    (active, others)
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
    let filtered_rows = ordered_targets
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
                if active != Some(&target) {
                    return None;
                }
            }
            Some(ProviderRow {
                is_active: active == Some(&target),
                target,
                view,
            })
        })
        .collect::<Vec<_>>();

    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for row in filtered_rows {
        if let Some(account) = &row.view.account {
            let key = (
                row.view.quota_family_id.clone(),
                account.fingerprint.clone(),
            );
            if seen.insert(key) {
                result.push(row);
            }
        } else {
            result.push(row);
        }
    }
    result
}

fn provider_key(target: &QuotaTarget) -> String {
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    format!("quota-provider-{:016x}", hasher.finish())
}

fn sync_expanded_for_active_target(
    expanded: &mut HashSet<String>,
    previous: Option<&QuotaTarget>,
    next: &QuotaTarget,
) {
    if let Some(previous) = previous {
        expanded.remove(&provider_key(previous));
    }

    expanded.insert(provider_key(next));
    expanded.remove(OTHER_PROVIDERS_KEY);
}

fn provider_error_text(view: &QuotaView, is_active: bool) -> Option<String> {
    let error = view.error.as_ref()?;
    if view.snapshot.is_some()
        || is_active
        || (view.account.is_some() && !error.disconnects_provider())
    {
        Some(sanitized_error(error))
    } else {
        None
    }
}

fn provider_heading(snapshot: &QuotaSnapshot) -> String {
    snapshot.provider_name.to_string()
}

#[cfg(test)]
fn provider_windows(snapshot: &QuotaSnapshot) -> Vec<ProviderWindow> {
    let mut windows = Vec::new();
    if !snapshot.windows.is_empty() {
        windows.extend(snapshot.windows.iter().map(|window| ProviderWindow {
            label: window.label.to_string(),
        }));
    }
    if let Some(active_model_id) = snapshot.active_model_id.as_ref()
        && let Some(model_windows) = snapshot.model_windows.get(active_model_id)
        && !model_windows.is_empty()
    {
        windows.extend(model_windows.iter().map(|window| ProviderWindow {
            label: window.label.to_string(),
        }));
    }
    for group in &snapshot.groups {
        if !group.buckets.is_empty() {
            windows.extend(group.buckets.iter().map(|bucket| ProviderWindow {
                label: bucket.window.label.to_string(),
            }));
        }
    }
    windows
}

fn render_section(
    title: &str,
    windows: &[QuotaWindow],
    now_unix_ms: i64,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(Label::new(title).weight(FontWeight::MEDIUM))
        .children(windows.iter().map(|window| {
            let reset = reset_value(window, now_unix_ms);
            let label = window_display_label(window);
            let value_element = h_flex()
                .gap_1p5()
                .when_some(window.remaining_percent, |this, remaining| {
                    this.child(
                        CircularProgress::new(remaining as f32, 100.0, px(12.0), cx)
                            .stroke_width(px(2.0))
                            .progress_color(quota_progress_color(remaining, cx)),
                    )
                })
                .child(Label::new(window_value(window)));

            h_flex()
                .justify_between()
                .gap_2()
                .child(Label::new(label).color(Color::Muted))
                .child(value_element)
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
            let expiry = reset.expires_at_unix_ms.and_then(localized_expiry);
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

fn format_utc_offset(offset_secs: i32) -> String {
    if offset_secs == 0 {
        "UTC-0".to_string()
    } else {
        let sign = if offset_secs < 0 { "-" } else { "+" };
        let abs_secs = offset_secs.abs();
        let hours = abs_secs / 3600;
        let mins = (abs_secs % 3600) / 60;
        if mins == 0 {
            format!("UTC{}{}", sign, hours)
        } else {
            format!("UTC{}{}:{:02}", sign, hours, mins)
        }
    }
}
fn localized_expiry(unix_ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(unix_ms).map(|date| {
        let local_date = date.with_timezone(&Local);
        use chrono::Offset as _;
        let offset_secs = local_date.offset().fix().local_minus_utc();
        let offset_str = format_utc_offset(offset_secs);
        format!(
            "{}, {} ({})",
            local_date.format("%x"),
            local_date.format("%-I:%M %p"),
            offset_str
        )
    })
}

fn sanitized_error(error: &QuotaError) -> String {
    match error {
        QuotaError::MissingCredentials => "Quota credentials are not configured".to_string(),
        QuotaError::AmbiguousAccount => "Quota account could not be identified".to_string(),
        QuotaError::Cooldown => "Quota refresh is cooling down".to_string(),
        QuotaError::Authentication(msg) => msg.to_string(),
        QuotaError::RateLimited { .. } => "Quota provider rate limit reached".to_string(),
        QuotaError::Provider(_) => "Quota provider request failed".to_string(),
    }
}

fn render_context_usage(context: &ContextUsageData) -> impl IntoElement {
    let used = crate::humanize_token_count(context.token_usage.used_tokens);
    let max = crate::humanize_token_count(context.token_usage.max_tokens);

    let mut content = v_flex()
        .gap_1()
        .child(Label::new(format!("{used} / {max} tokens")).color(Color::Muted));

    if let Some(cost) = &context.cost {
        content = content.child(Label::new(format!(
            "Cost: {:.2} {}",
            cost.amount, cost.currency
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
    fn provider_row(id: &str, is_active: bool) -> ProviderRow {
        let mut target = target(id);
        target.provider_or_agent_id = Arc::from(id);
        let mut view = view(id, None, None);
        view.provider_id = Arc::from(id);
        view.quota_family_id = Arc::from(id);
        ProviderRow {
            is_active,
            target,
            view,
        }
    }

    #[test]
    fn partition_preserves_inactive_provider_order() {
        let first = provider_row("first", false);
        let active = provider_row("active", true);
        let second = provider_row("second", false);

        let (active_row, others) = partition_provider_rows(vec![first, active, second]);

        assert!(active_row.is_some());
        assert_eq!(
            others
                .iter()
                .map(|row| row.target.provider_or_agent_id.as_ref())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
        );
    }

    #[test]
    fn partition_with_only_active_has_no_other_providers() {
        let active = provider_row("active", true);

        let (active_row, others) = partition_provider_rows(vec![active]);

        assert!(active_row.is_some());
        assert!(others.is_empty());
    }

    #[test]
    fn active_target_expansion_synchronization() {
        let mut expanded = HashSet::default();
        let active = target("active");
        let next = target("next");

        expanded.insert(OTHER_PROVIDERS_KEY.to_string());
        expanded.insert(provider_key(&active));

        sync_expanded_for_active_target(&mut expanded, Some(&active), &next);

        assert!(!expanded.contains(OTHER_PROVIDERS_KEY));
        assert!(!expanded.contains(&provider_key(&active)));
        assert!(expanded.contains(&provider_key(&next)));
    }

    #[test]
    fn reset_details_start_collapsed() {
        let provider_key = "quota-provider-test";
        let resets_key = format!("{provider_key}-resets");
        let expanded = HashSet::<String>::new();
        let has_details = true;

        let resets_expanded = has_details && expanded.contains(&resets_key);

        assert!(!resets_expanded);
    }

    #[test]
    fn format_utc_offset_formats_correctly() {
        assert_eq!(format_utc_offset(0), "UTC-0");
        assert_eq!(format_utc_offset(3600), "UTC+1");
        assert_eq!(format_utc_offset(-18000), "UTC-5");
        assert_eq!(format_utc_offset(19800), "UTC+5:30");
    }

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

    fn view(
        provider_id: &str,
        snapshot: Option<QuotaSnapshot>,
        error: Option<QuotaError>,
    ) -> QuotaView {
        QuotaView {
            provider_id: Arc::from(provider_id),
            quota_family_id: Arc::from(provider_id),
            provider_name: "Provider".into(),
            account: snapshot.as_ref().map(|snapshot| snapshot.account.clone()),
            snapshot,
            is_fetching: false,
            error,
        }
    }

    #[test]
    fn active_provider_is_first_and_expanded() {
        let active = target_with_model("active", "active-model");
        let other = target_with_model("other", "other-model");
        let mut views = HashMap::default();
        views.insert(
            active.clone(),
            view(
                &active.provider_or_agent_id,
                Some(snapshot_fetched_at(0)),
                None,
            ),
        );
        views.insert(
            other.clone(),
            view(
                &other.provider_or_agent_id,
                Some(snapshot_fetched_at(0)),
                None,
            ),
        );

        let rows = provider_rows(&[other.clone(), active.clone()], Some(&active), &views);

        assert_eq!(
            rows.iter().map(|row| &row.target).collect::<Vec<_>>(),
            vec![&active, &other]
        );
        assert!(rows[0].is_active);
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
    fn family_based_deduplication_keeps_only_active_first() {
        let active = target_with_model("openai-subscribed", "gpt-4o");
        let inactive = target_with_model("codex", "codex-model");

        let mut views = HashMap::default();
        let mut active_view = view("mock1", Some(snapshot_fetched_at(0)), None);
        active_view.quota_family_id = Arc::from("openai-codex");
        let mut inactive_view = view("mock2", Some(snapshot_fetched_at(0)), None);
        inactive_view.quota_family_id = Arc::from("openai-codex");

        views.insert(active.clone(), active_view);
        views.insert(inactive.clone(), inactive_view);

        let rows = provider_rows(&[inactive.clone(), active.clone()], Some(&active), &views);

        // Deduplication happens because family is "openai-codex" and account is the same ("account").
        // The active one is ordered first, so only `active` is kept.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target, active);
    }

    #[test]
    fn different_accounts_are_not_deduplicated() {
        let active = target_with_model("openai-subscribed", "gpt-4o");
        let inactive = target_with_model("codex", "codex-model");

        let mut views = HashMap::default();
        let mut active_view = view("mock3", Some(snapshot_fetched_at(0)), None);
        active_view.quota_family_id = Arc::from("openai-codex");

        let mut inactive_snapshot = snapshot_fetched_at(0);
        inactive_snapshot.account.fingerprint = Arc::from("other-account");
        let mut inactive_view = view("mock4", Some(inactive_snapshot), None);
        inactive_view.quota_family_id = Arc::from("openai-codex");

        views.insert(active.clone(), active_view);
        views.insert(inactive.clone(), inactive_view);

        let rows = provider_rows(&[inactive.clone(), active.clone()], Some(&active), &views);

        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn other_connected_providers_start_collapsed() {
        let active = target("active");
        let other = target("other");
        let mut views = HashMap::default();
        views.insert(
            active.clone(),
            view(
                &active.provider_or_agent_id,
                Some(snapshot_fetched_at(0)),
                None,
            ),
        );
        views.insert(
            other.clone(),
            view(
                &other.provider_or_agent_id,
                Some(snapshot_fetched_at(0)),
                None,
            ),
        );

        let rows = provider_rows(&[active.clone(), other], Some(&active), &views);

        assert!(!rows[1].is_active);
    }

    #[test]
    fn missing_or_initial_auth_failed_provider_is_filtered() {
        let connected = target("connected");
        let missing = target("missing");
        let auth_failed = target("auth-failed");
        let mut views = HashMap::default();
        views.insert(
            connected.clone(),
            view(
                &connected.provider_or_agent_id,
                Some(snapshot_fetched_at(0)),
                None,
            ),
        );
        views.insert(
            missing.clone(),
            view(
                &missing.provider_or_agent_id,
                None,
                Some(QuotaError::MissingCredentials),
            ),
        );
        views.insert(
            auth_failed.clone(),
            view(
                &auth_failed.provider_or_agent_id,
                None,
                Some(QuotaError::Authentication("secret".into())),
            ),
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
                "mock5",
                Some(snapshot_fetched_at(0)),
                Some(QuotaError::Provider("temporary failure".into())),
            ),
        );

        let rows = provider_rows(std::slice::from_ref(&cached), None, &views);

        assert_eq!(rows.len(), 1);
        assert!(rows[0].view.snapshot.is_some());

        let mut account_only = view(
            "mock6",
            Some(snapshot_fetched_at(0)),
            Some(QuotaError::Provider("temporary failure".into())),
        );
        account_only.snapshot = None;
        assert_eq!(
            provider_error_text(&account_only, false),
            Some("Quota provider request failed".to_string())
        );
    }

    #[test]
    fn active_authentication_failure_remains_visible_with_guidance() {
        let mut views = HashMap::default();
        let target = target("provider");
        let error = QuotaError::Authentication("re-authenticate Codex".into());
        views.insert(
            target.clone(),
            view(&target.provider_or_agent_id, None, Some(error)),
        );

        let rows = provider_rows(std::slice::from_ref(&target), Some(&target), &views);
        assert_eq!(rows.len(), 1);

        let error_text = provider_error_text(&rows[0].view, rows[0].is_active);
        assert_eq!(error_text.as_deref(), Some("re-authenticate Codex"));
    }

    #[test]
    fn inactive_authentication_failure_remains_filtered() {
        let mut views = HashMap::default();
        let active = target("active");
        let inactive = target("inactive");
        let error = QuotaError::Authentication("re-authenticate Codex".into());
        views.insert(
            inactive.clone(),
            view(&inactive.provider_or_agent_id, None, Some(error)),
        );
        views.insert(
            active.clone(),
            view(&active.provider_or_agent_id, None, None),
        );

        let rows = provider_rows(&[inactive, active.clone()], Some(&active), &views);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target, active);
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
    fn arbitrary_provider_error_payload_is_never_rendered() {
        let error = QuotaError::Provider("token=secret response body user@ account_id".into());

        let sanitized = sanitized_error(&error);

        assert_eq!(sanitized, "Quota provider request failed");
        assert!(!sanitized.contains("token"));
        assert!(!sanitized.contains("secret"));
        assert!(!sanitized.contains("user@"));
        assert!(!sanitized.contains("account_id"));
    }

    #[test]
    fn empty_windows_and_groups_do_not_create_sections() {
        let mut snapshot = snapshot_fetched_at(0);
        snapshot.windows.clear();
        snapshot.active_model_id = Some(Arc::from("model"));
        snapshot
            .model_windows
            .insert(Arc::from("model"), Vec::new());
        snapshot.groups.push(ai_usage::QuotaGroup {
            id: Arc::from("group"),
            display_name: "Group".into(),
            description: None,
            applies_to_model_ids: Vec::new(),
            affects_severity: false,
            buckets: Vec::new(),
        });

        let sections = provider_windows(&snapshot);
        assert!(sections.is_empty());
    }
}
