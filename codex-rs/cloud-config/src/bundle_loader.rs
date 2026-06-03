use crate::backend::BackendBundleClient;
use crate::service::CLOUD_CONFIG_BUNDLE_TIMEOUT;
use crate::service::CloudConfigBundleService;
use codex_backend_client::Client as BackendClient;
use codex_config::CloudConfigBundleLoadError;
use codex_config::CloudConfigBundleLoadErrorCode;
use codex_config::CloudConfigBundleLoader;
use codex_config::ProductDefaults;
use codex_config::ProductDefaultsLoader;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::account::PlanType;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use tokio::task::JoinHandle;
use tokio::time::timeout;

fn refresher_task_slot() -> &'static Mutex<Option<JoinHandle<()>>> {
    static REFRESHER_TASK: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
    REFRESHER_TASK.get_or_init(|| Mutex::new(None))
}

pub fn cloud_config_bundle_loader(
    auth_manager: Arc<AuthManager>,
    chatgpt_base_url: String,
    codex_home: PathBuf,
) -> CloudConfigBundleLoader {
    let service = CloudConfigBundleService::new(
        auth_manager,
        Arc::new(BackendBundleClient::new(chatgpt_base_url)),
        codex_home,
        CLOUD_CONFIG_BUNDLE_TIMEOUT,
    );
    let refresh_service = service.clone();
    let task = tokio::spawn(async move { service.load_startup_bundle_with_timeout().await });
    let refresh_task =
        tokio::spawn(async move { refresh_service.refresh_cache_in_background().await });
    let mut refresher_guard = refresher_task_slot().lock().unwrap_or_else(|err| {
        tracing::warn!("cloud config bundle refresher task slot was poisoned");
        err.into_inner()
    });
    if let Some(existing_task) = refresher_guard.replace(refresh_task) {
        existing_task.abort();
    }
    CloudConfigBundleLoader::new(async move {
        task.await.map_err(|err| {
            tracing::error!(error = %err, "Cloud config bundle task failed");
            CloudConfigBundleLoadError::new(
                CloudConfigBundleLoadErrorCode::Internal,
                /*status_code*/ None,
                format!("cloud config bundle load failed: {err}"),
            )
        })?
    })
}

#[derive(Clone, Debug)]
pub struct BackendConfigLoaders {
    pub cloud_config_bundle: CloudConfigBundleLoader,
    pub product_defaults: ProductDefaultsLoader,
}

pub async fn cloud_config_bundle_loader_for_storage(
    codex_home: PathBuf,
    enable_codex_api_key_env: bool,
    credentials_store_mode: AuthCredentialsStoreMode,
    chatgpt_base_url: String,
) -> CloudConfigBundleLoader {
    backend_config_loaders_for_storage(
        codex_home,
        enable_codex_api_key_env,
        credentials_store_mode,
        chatgpt_base_url,
    )
    .await
    .cloud_config_bundle
}

pub async fn backend_config_loaders_for_storage(
    codex_home: PathBuf,
    enable_codex_api_key_env: bool,
    credentials_store_mode: AuthCredentialsStoreMode,
    chatgpt_base_url: String,
) -> BackendConfigLoaders {
    let auth_manager = AuthManager::shared(
        codex_home.clone(),
        enable_codex_api_key_env,
        credentials_store_mode,
        Some(chatgpt_base_url.clone()),
    )
    .await;
    let product_defaults = product_defaults_loader(auth_manager.clone(), chatgpt_base_url.clone());
    BackendConfigLoaders {
        cloud_config_bundle: cloud_config_bundle_loader(auth_manager, chatgpt_base_url, codex_home),
        product_defaults,
    }
}

pub fn product_defaults_loader(
    auth_manager: Arc<AuthManager>,
    chatgpt_base_url: String,
) -> ProductDefaultsLoader {
    ProductDefaultsLoader::new(fetch_product_defaults(auth_manager, chatgpt_base_url))
}

fn product_defaults_eligible_auth(auth: &CodexAuth) -> bool {
    auth.uses_codex_backend()
        && auth
            .account_plan_type()
            .is_some_and(PlanType::is_workspace_account)
}

async fn fetch_product_defaults(
    auth_manager: Arc<AuthManager>,
    chatgpt_base_url: String,
) -> Result<ProductDefaults, CloudConfigBundleLoadError> {
    let Some(auth) = auth_manager.auth().await else {
        return Ok(ProductDefaults::default());
    };
    if !product_defaults_eligible_auth(&auth) {
        return Ok(ProductDefaults::default());
    }

    let client = match BackendClient::from_auth(chatgpt_base_url, &auth) {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to construct backend client for product defaults; continuing without product defaults");
            return Ok(ProductDefaults::default());
        }
    };

    let response = match timeout(CLOUD_CONFIG_BUNDLE_TIMEOUT, client.get_config_bundle()).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "Failed to fetch product defaults; continuing without product defaults");
            return Ok(ProductDefaults::default());
        }
        Err(_) => {
            tracing::warn!(
                "Timed out fetching product defaults; continuing without product defaults"
            );
            return Ok(ProductDefaults::default());
        }
    };

    let fragments = response
        .config_toml
        .flatten()
        .and_then(|config_toml| config_toml.product_defaults.flatten())
        .unwrap_or_default();
    let contents = fragments.iter().map(|fragment| fragment.contents.as_str());
    match ProductDefaults::from_toml_fragments(contents) {
        Ok(product_defaults) => Ok(product_defaults),
        Err(err) => {
            tracing::error!(error = %err, "Failed to parse product defaults; continuing without product defaults");
            Ok(ProductDefaults::default())
        }
    }
}
