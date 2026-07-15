use std::sync::Arc;

use ::settings::{Settings, SettingsStore};
use client::{Client, UserStore};
use collections::{HashMap, HashSet};
use credentials_provider::CredentialsProvider;
use gpui::{App, Context, Entity};
use language_model::{LanguageModelProviderId, LanguageModelRegistry};
use provider::deepseek::DeepSeekLanguageModelProvider;

pub mod extension;
pub mod provider;
mod settings;

pub use crate::extension::init_proxy as init_extension_proxy;

use crate::provider::anthropic::AnthropicLanguageModelProvider;
use crate::provider::anthropic_compatible::AnthropicCompatibleLanguageModelProvider;
use crate::provider::bedrock::BedrockLanguageModelProvider;
use crate::provider::cloud::CloudLanguageModelProvider;
use crate::provider::copilot_chat::CopilotChatLanguageModelProvider;
use crate::provider::google::GoogleLanguageModelProvider;
use crate::provider::llama_cpp::LlamaCppLanguageModelProvider;
use crate::provider::lmstudio::LmStudioLanguageModelProvider;
pub use crate::provider::mistral::MistralLanguageModelProvider;
use crate::provider::ollama::OllamaLanguageModelProvider;
use crate::provider::open_ai::OpenAiLanguageModelProvider;
use crate::provider::open_ai_compatible::OpenAiCompatibleLanguageModelProvider;
use crate::provider::open_router::OpenRouterLanguageModelProvider;
use crate::provider::openai_subscribed::OpenAiSubscribedProvider;
use crate::provider::opencode::OpenCodeLanguageModelProvider;
use crate::provider::vercel_ai_gateway::VercelAiGatewayLanguageModelProvider;
use crate::provider::x_ai::XAiLanguageModelProvider;
pub use crate::settings::*;

pub fn quota_target_for_model(
    model: &Arc<dyn language_model::LanguageModel>,
) -> ai_usage::QuotaTarget {
    ai_usage::QuotaTarget {
        kind: ai_usage::QuotaTargetKind::NativeLanguageModel,
        provider_or_agent_id: Arc::from(model.provider_id().0.as_ref()),
        upstream_provider_id: Some(Arc::from(model.upstream_provider_id().0.as_ref())),
        model_id: Some(Arc::from(model.id().0.as_ref())),
        model_name: Some(model.name().0),
    }
}

pub fn register_quota_collector(collector: Arc<dyn ai_usage::QuotaCollector>, cx: &mut App) {
    ai_usage::register_collector(collector, cx);
}

pub fn init(user_store: Entity<UserStore>, client: Arc<Client>, cx: &mut App) {
    ai_usage::init(client.http_client(), cx);
    let credentials_provider = client.credentials_provider();
    let registry = LanguageModelRegistry::global(cx);
    registry.update(cx, |registry, cx| {
        register_language_model_providers(
            registry,
            user_store,
            client.clone(),
            credentials_provider.clone(),
            cx,
        );
    });

    // Subscribe to extension store events to track LLM extension installations
    if let Some(extension_store) = extension_host::ExtensionStore::try_global(cx) {
        cx.subscribe(&extension_store, {
            let registry = registry.downgrade();
            move |extension_store, event, cx| {
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                match event {
                    extension_host::Event::ExtensionInstalled(extension_id) => {
                        if let Some(manifest) = extension_store
                            .read(cx)
                            .extension_manifest_for_id(extension_id)
                        {
                            if !manifest.language_model_providers.is_empty() {
                                registry.update(cx, |registry, cx| {
                                    registry.extension_installed(extension_id.clone(), cx);
                                });
                            }
                        }
                    }
                    extension_host::Event::ExtensionUninstalled(extension_id) => {
                        registry.update(cx, |registry, cx| {
                            registry.extension_uninstalled(extension_id, cx);
                        });
                    }
                    extension_host::Event::ExtensionsUpdated => {
                        let mut new_ids = HashSet::default();
                        for (extension_id, entry) in extension_store.read(cx).installed_extensions()
                        {
                            if !entry.manifest.language_model_providers.is_empty() {
                                new_ids.insert(extension_id.clone());
                            }
                        }
                        registry.update(cx, |registry, cx| {
                            registry.sync_installed_llm_extensions(new_ids, cx);
                        });
                    }
                    _ => {}
                }
            }
        })
        .detach();

        // Initialize with currently installed extensions
        registry.update(cx, |registry, cx| {
            let mut initial_ids = HashSet::default();
            for (extension_id, entry) in extension_store.read(cx).installed_extensions() {
                if !entry.manifest.language_model_providers.is_empty() {
                    initial_ids.insert(extension_id.clone());
                }
            }
            registry.sync_installed_llm_extensions(initial_ids, cx);
        });
    }

    let mut compatible_providers = CompatibleProviders::from_settings(cx);

    registry.update(cx, |registry, cx| {
        register_compatible_providers(
            registry,
            &CompatibleProviders::default(),
            &compatible_providers,
            &client,
            &credentials_provider,
            cx,
        );
    });

    let registry = registry.downgrade();
    cx.observe_global::<SettingsStore>(move |cx| {
        let Some(registry) = registry.upgrade() else {
            return;
        };
        let compatible_providers_new = CompatibleProviders::from_settings(cx);
        if compatible_providers_new != compatible_providers {
            registry.update(cx, |registry, cx| {
                register_compatible_providers(
                    registry,
                    &compatible_providers,
                    &compatible_providers_new,
                    &client,
                    &credentials_provider,
                    cx,
                );
            });
            compatible_providers = compatible_providers_new;
        }
    })
    .detach();
}

#[derive(Default, PartialEq, Eq)]
struct CompatibleProviders(HashMap<Arc<str>, CompatibleProviderKind>);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CompatibleProviderKind {
    OpenAi,
    Anthropic,
}

impl CompatibleProviders {
    fn from_settings(cx: &App) -> Self {
        let settings = AllLanguageModelSettings::get_global(cx);
        let mut providers: HashMap<Arc<str>, CompatibleProviderKind> = settings
            .openai_compatible
            .keys()
            .map(|id| (id.clone(), CompatibleProviderKind::OpenAi))
            .collect();
        for id in settings.anthropic_compatible.keys() {
            // The registry has a single provider ID namespace, so a name can
            // only refer to one provider. OpenAI-compatible entries win
            // collisions because they predate Anthropic-compatible ones, so
            // existing configurations keep working.
            if providers.contains_key(id) {
                log::warn!(
                    "ignoring `anthropic_compatible` provider `{id}`: \
                     an `openai_compatible` provider with the same name exists"
                );
            } else {
                providers.insert(id.clone(), CompatibleProviderKind::Anthropic);
            }
        }
        Self(providers)
    }
}

fn register_compatible_providers(
    registry: &mut LanguageModelRegistry,
    old: &CompatibleProviders,
    new: &CompatibleProviders,
    client: &Arc<Client>,
    credentials_provider: &Arc<dyn CredentialsProvider>,
    cx: &mut Context<LanguageModelRegistry>,
) {
    for (provider_id, old_kind) in &old.0 {
        if new.0.get(provider_id) != Some(old_kind) {
            registry.unregister_provider(LanguageModelProviderId::from(provider_id.clone()), cx);
        }
    }

    for (provider_id, kind) in &new.0 {
        if old.0.get(provider_id) != Some(kind) {
            match kind {
                CompatibleProviderKind::OpenAi => registry.register_provider(
                    Arc::new(OpenAiCompatibleLanguageModelProvider::new(
                        provider_id.clone(),
                        client.http_client(),
                        credentials_provider.clone(),
                        cx,
                    )),
                    cx,
                ),
                CompatibleProviderKind::Anthropic => registry.register_provider(
                    Arc::new(AnthropicCompatibleLanguageModelProvider::new(
                        provider_id.clone(),
                        client.http_client(),
                        credentials_provider.clone(),
                        cx,
                    )),
                    cx,
                ),
            }
        }
    }
}

fn register_language_model_providers(
    registry: &mut LanguageModelRegistry,
    user_store: Entity<UserStore>,
    client: Arc<Client>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &mut Context<LanguageModelRegistry>,
) {
    registry.register_provider(
        Arc::new(CloudLanguageModelProvider::new(
            user_store,
            client.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(AnthropicLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(OpenAiLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(OllamaLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(LmStudioLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(LlamaCppLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(DeepSeekLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(GoogleLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        MistralLanguageModelProvider::global(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        ),
        cx,
    );
    registry.register_provider(
        Arc::new(BedrockLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(OpenRouterLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(VercelAiGatewayLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(XAiLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(
        Arc::new(OpenCodeLanguageModelProvider::new(
            client.http_client(),
            credentials_provider.clone(),
            cx,
        )),
        cx,
    );
    registry.register_provider(Arc::new(CopilotChatLanguageModelProvider::new(cx)), cx);
    let provider = Arc::new(OpenAiSubscribedProvider::new(
        client.http_client(),
        credentials_provider,
        cx,
    ));
    register_quota_collector(provider.quota_collector(), cx);
    registry.register_provider(provider, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_usage::{QuotaAccount, QuotaCacheKey, QuotaCacheScope};
    use anyhow::Result;
    use clock::FakeSystemClock;
    use feature_flags::FeatureFlagAppExt as _;
    use futures::{future::BoxFuture, stream::BoxStream};
    use gpui::{AppContext as _, AsyncApp, BorrowAppContext as _};
    use http_client::FakeHttpClient;
    use language_model::{
        IconOrSvg, LanguageModel, LanguageModelCompletionError, LanguageModelCompletionEvent,
        LanguageModelId, LanguageModelName, LanguageModelProviderId, LanguageModelProviderName,
        LanguageModelRequest, LanguageModelToolChoice, fake_provider::FakeLanguageModel,
    };
    use release_channel::AppVersion;
    use std::future::Future;
    use std::pin::Pin;
    use ui::IconName;

    struct UpstreamModel {
        inner: FakeLanguageModel,
        upstream_provider_id: LanguageModelProviderId,
    }

    impl LanguageModel for UpstreamModel {
        fn id(&self) -> LanguageModelId {
            self.inner.id()
        }

        fn name(&self) -> LanguageModelName {
            self.inner.name()
        }

        fn provider_id(&self) -> LanguageModelProviderId {
            self.inner.provider_id()
        }

        fn provider_name(&self) -> LanguageModelProviderName {
            self.inner.provider_name()
        }

        fn upstream_provider_id(&self) -> LanguageModelProviderId {
            self.upstream_provider_id.clone()
        }

        fn telemetry_id(&self) -> String {
            self.inner.telemetry_id()
        }

        fn supports_images(&self) -> bool {
            self.inner.supports_images()
        }

        fn supports_tools(&self) -> bool {
            self.inner.supports_tools()
        }

        fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
            self.inner.supports_tool_choice(choice)
        }

        fn max_token_count(&self) -> u64 {
            self.inner.max_token_count()
        }

        fn stream_completion(
            &self,
            request: LanguageModelRequest,
            cx: &AsyncApp,
        ) -> BoxFuture<
            'static,
            Result<
                BoxStream<
                    'static,
                    Result<LanguageModelCompletionEvent, LanguageModelCompletionError>,
                >,
                LanguageModelCompletionError,
            >,
        > {
            self.inner.stream_completion(request, cx)
        }
    }

    #[test]
    fn quota_target_preserves_provider_and_upstream_ids() {
        let model = Arc::new(UpstreamModel {
            inner: FakeLanguageModel::with_id_and_thinking("adapter", "model", "Model", false),
            upstream_provider_id: LanguageModelProviderId::from("upstream".to_string()),
        }) as Arc<dyn LanguageModel>;

        let target = quota_target_for_model(&model);

        assert_eq!(target.provider_or_agent_id.as_ref(), "adapter");
        assert_eq!(target.upstream_provider_id.as_deref(), Some("upstream"));
        assert_eq!(target.model_id.as_deref(), Some("model"));
        assert_eq!(target.model_name.as_deref(), Some("Model"));
    }

    #[test]
    fn native_and_acp_use_distinct_credential_sources() {
        let native_account = QuotaAccount::from_stable_identity(
            "openai-subscribed",
            b"identical-account-identity",
            None,
        );
        let acp_account =
            QuotaAccount::from_stable_identity("codex-acp", b"identical-account-identity", None);
        let key = |account: &QuotaAccount| QuotaCacheKey {
            collector_id: Arc::from("collector"),
            credential_source: account.credential_source.clone(),
            account_fingerprint: account.fingerprint.clone(),
            provider_or_agent_id: Arc::from("provider"),
            scope: QuotaCacheScope::Account,
        };

        assert_ne!(key(&native_account), key(&acp_account));
        assert_eq!(native_account.fingerprint, acp_account.fingerprint);
    }

    struct FakeCredentialsProvider;

    impl CredentialsProvider for FakeCredentialsProvider {
        fn read_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
            Box::pin(async { Ok(None) })
        }

        fn write_credentials<'a>(
            &'a self,
            _url: &'a str,
            _username: &'a str,
            _password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn init_test(cx: &mut App) -> (Arc<Client>, Arc<dyn CredentialsProvider>) {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        cx.set_global(db::AppDatabase::test_new());
        let app_version = AppVersion::global(cx);
        release_channel::init_test(app_version, release_channel::ReleaseChannel::Dev, cx);
        gpui_tokio::init(cx);
        cx.update_flags(false, Vec::new());

        let client = Client::new(
            Arc::new(FakeSystemClock::new()),
            FakeHttpClient::with_404_response(),
            cx,
        );
        (client, Arc::new(FakeCredentialsProvider))
    }

    fn update_compatible_provider_settings(
        openai: &[&str],
        anthropic: &[&str],
        cx: &mut App,
    ) -> CompatibleProviders {
        fn section(ids: &[&str]) -> serde_json::Value {
            ids.iter()
                .map(|id| {
                    (
                        id.to_string(),
                        serde_json::json!({
                            "api_url": "https://example.com",
                            "available_models": [],
                        }),
                    )
                })
                .collect::<serde_json::Map<String, serde_json::Value>>()
                .into()
        }

        let content = serde_json::json!({
            "language_models": {
                "openai_compatible": section(openai),
                "anthropic_compatible": section(anthropic),
            }
        })
        .to_string();
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(&content, cx)
                .expect("failed to parse test settings");
        });
        CompatibleProviders::from_settings(cx)
    }

    fn provider_icons(registry: &LanguageModelRegistry, id: &str) -> Vec<IconOrSvg> {
        registry
            .providers()
            .into_iter()
            .filter(|provider| provider.id().0.as_ref() == id)
            .map(|provider| provider.icon())
            .collect()
    }

    #[gpui::test]
    fn test_compatible_provider_id_collision_resolves_when_one_entry_is_removed(cx: &mut App) {
        let (client, credentials_provider) = init_test(cx);
        let registry = cx.new(|_| LanguageModelRegistry::default());

        // The same provider name is configured in both `openai_compatible`
        // and `anthropic_compatible` settings sections; the OpenAI-compatible
        // entry wins the collision.
        let both = update_compatible_provider_settings(&["acme"], &["acme"], cx);
        registry.update(cx, |registry, cx| {
            register_compatible_providers(
                registry,
                &CompatibleProviders::default(),
                &both,
                &client,
                &credentials_provider,
                cx,
            );
        });
        assert_eq!(
            registry.read_with(cx, |registry, _| provider_icons(registry, "acme")),
            vec![IconOrSvg::Icon(IconName::AiOpenAiCompat)],
            "the OpenAI-compatible provider should win the name collision"
        );

        // The user removes the `anthropic_compatible` entry; the remaining
        // `openai_compatible` entry must stay registered.
        let openai_only = update_compatible_provider_settings(&["acme"], &[], cx);
        registry.update(cx, |registry, cx| {
            register_compatible_providers(
                registry,
                &both,
                &openai_only,
                &client,
                &credentials_provider,
                cx,
            );
        });
        assert_eq!(
            registry.read_with(cx, |registry, _| provider_icons(registry, "acme")),
            vec![IconOrSvg::Icon(IconName::AiOpenAiCompat)],
            "the provider registered for `acme` should be the OpenAI-compatible one"
        );
    }

    #[gpui::test]
    fn test_compatible_provider_changes_kind_and_unregisters(cx: &mut App) {
        let (client, credentials_provider) = init_test(cx);
        let registry = cx.new(|_| LanguageModelRegistry::default());

        let both = update_compatible_provider_settings(&["acme"], &["acme"], cx);
        registry.update(cx, |registry, cx| {
            register_compatible_providers(
                registry,
                &CompatibleProviders::default(),
                &both,
                &client,
                &credentials_provider,
                cx,
            );
        });

        // Removing the `openai_compatible` entry hands the name over to the
        // remaining `anthropic_compatible` entry.
        let anthropic_only = update_compatible_provider_settings(&[], &["acme"], cx);
        registry.update(cx, |registry, cx| {
            register_compatible_providers(
                registry,
                &both,
                &anthropic_only,
                &client,
                &credentials_provider,
                cx,
            );
        });
        assert_eq!(
            registry.read_with(cx, |registry, _| provider_icons(registry, "acme")),
            vec![IconOrSvg::Icon(IconName::AiAnthropicCompat)],
            "after removing the openai_compatible entry, the anthropic_compatible provider should be registered"
        );

        // Removing the last entry unregisters the provider entirely.
        let none = update_compatible_provider_settings(&[], &[], cx);
        registry.update(cx, |registry, cx| {
            register_compatible_providers(
                registry,
                &anthropic_only,
                &none,
                &client,
                &credentials_provider,
                cx,
            );
        });
        assert_eq!(
            registry.read_with(cx, |registry, _| provider_icons(registry, "acme")),
            Vec::new(),
            "removing all entries should unregister the provider"
        );
    }
}
