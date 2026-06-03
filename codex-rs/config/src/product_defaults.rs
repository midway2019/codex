use crate::CloudConfigBundleLoadError;
use crate::CloudConfigBundleLoader;
use crate::merge_toml_values;
use crate::state::ConfigLayerEntry;
use codex_app_server_protocol::ConfigLayerSource;
use codex_features::Feature;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::future::Shared;
use std::fmt;
use std::future::Future;
use toml::Value as TomlValue;
use toml::map::Map;

/// Product-owned default config supplied by OpenAI.
///
/// This is intentionally represented as a normal config layer so the existing
/// config merge and requirements enforcement paths determine final behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductDefaults {
    config: TomlValue,
}

impl Default for ProductDefaults {
    fn default() -> Self {
        Self {
            config: TomlValue::Table(Map::new()),
        }
    }
}

impl ProductDefaults {
    pub fn from_config(config: TomlValue) -> Self {
        Self { config }
    }

    pub fn from_toml_str(contents: &str) -> Result<Self, toml::de::Error> {
        let config = if contents.trim().is_empty() {
            TomlValue::Table(Map::new())
        } else {
            toml::from_str(contents)?
        };
        Ok(Self::from_config(config))
    }

    /// Merges backend-delivered fragments ordered highest precedence first.
    pub fn from_toml_fragments<'a, I>(fragments: I) -> Result<Self, toml::de::Error>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut config = TomlValue::Table(Map::new());
        let fragments = fragments.into_iter().collect::<Vec<_>>();
        for contents in fragments.into_iter().rev() {
            let fragment = Self::from_toml_str(contents)?.config;
            merge_toml_values(&mut config, &fragment);
        }
        Ok(Self::from_config(config))
    }

    pub fn set_feature_enabled(&mut self, feature: Feature, enabled: bool) -> &mut Self {
        if !self.config.is_table() {
            self.config = TomlValue::Table(Map::new());
        }
        let TomlValue::Table(root) = &mut self.config else {
            return self;
        };
        let features = root
            .entry("features".to_string())
            .or_insert_with(|| TomlValue::Table(Map::new()));
        if !features.is_table() {
            *features = TomlValue::Table(Map::new());
        }
        let TomlValue::Table(features) = features else {
            return self;
        };
        features.insert(feature.key().to_string(), TomlValue::Boolean(enabled));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.config.as_table().is_none_or(Map::is_empty)
    }

    pub fn config(&self) -> &TomlValue {
        &self.config
    }

    pub fn into_config_layer(self) -> Option<ConfigLayerEntry> {
        if self.is_empty() {
            None
        } else {
            Some(ConfigLayerEntry::new(
                ConfigLayerSource::ProductDefaults,
                self.config,
            ))
        }
    }
}

#[derive(Clone)]
pub struct ProductDefaultsLoader {
    fut: Shared<BoxFuture<'static, Result<ProductDefaults, CloudConfigBundleLoadError>>>,
}

impl ProductDefaultsLoader {
    pub fn new<F>(fut: F) -> Self
    where
        F: Future<Output = Result<ProductDefaults, CloudConfigBundleLoadError>> + Send + 'static,
    {
        Self {
            fut: fut.boxed().shared(),
        }
    }

    pub fn from_defaults(defaults: ProductDefaults) -> Self {
        Self::new(async move { Ok(defaults) })
    }

    pub fn from_cloud_config_bundle(cloud_config_bundle: CloudConfigBundleLoader) -> Self {
        Self::new(async move {
            let bundle = match cloud_config_bundle.get().await {
                Ok(Some(bundle)) => bundle,
                Ok(None) => return Ok(ProductDefaults::default()),
                Err(err) => {
                    tracing::warn!(error = %err, "Failed to load product defaults; continuing without product defaults");
                    return Ok(ProductDefaults::default());
                }
            };
            let contents = bundle
                .config_toml
                .product_defaults
                .iter()
                .map(|fragment| fragment.contents.as_str());
            match ProductDefaults::from_toml_fragments(contents) {
                Ok(product_defaults) => Ok(product_defaults),
                Err(err) => {
                    tracing::error!(error = %err, "Failed to parse product defaults; continuing without product defaults");
                    Ok(ProductDefaults::default())
                }
            }
        })
    }

    pub async fn get(&self) -> Result<ProductDefaults, CloudConfigBundleLoadError> {
        self.fut.clone().await
    }
}

impl Default for ProductDefaultsLoader {
    fn default() -> Self {
        Self::from_defaults(ProductDefaults::default())
    }
}

impl fmt::Debug for ProductDefaultsLoader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProductDefaultsLoader").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CloudConfigBundle;
    use crate::CloudConfigFragment;
    use crate::CloudConfigTomlBundle;
    use crate::CloudRequirementsTomlBundle;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[test]
    fn product_default_fragments_preserve_backend_precedence_order() {
        let defaults = ProductDefaults::from_toml_fragments([
            "[features]\nplugin_sharing = false\n",
            "[features]\nplugin_sharing = true\n",
        ])
        .expect("fragments should parse");

        assert_eq!(
            defaults
                .config()
                .get("features")
                .and_then(TomlValue::as_table)
                .and_then(|features| features.get("plugin_sharing"))
                .and_then(TomlValue::as_bool),
            Some(false)
        );
    }

    #[tokio::test]
    async fn product_defaults_loader_reads_from_shared_bundle() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let cloud_config_bundle = CloudConfigBundleLoader::new(async move {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            Ok(Some(CloudConfigBundle {
                config_toml: CloudConfigTomlBundle {
                    product_defaults: vec![
                        CloudConfigFragment {
                            id: "cfg_default_high".to_string(),
                            name: "High priority defaults".to_string(),
                            contents: "[features]\nplugin_sharing = false\n".to_string(),
                        },
                        CloudConfigFragment {
                            id: "cfg_default_low".to_string(),
                            name: "Low priority defaults".to_string(),
                            contents: "[features]\nplugin_sharing = true\n".to_string(),
                        },
                    ],
                    enterprise_managed: Vec::new(),
                },
                requirements_toml: CloudRequirementsTomlBundle::default(),
            }))
        });
        let product_defaults =
            ProductDefaultsLoader::from_cloud_config_bundle(cloud_config_bundle.clone());

        let (bundle, defaults) = tokio::join!(cloud_config_bundle.get(), product_defaults.get());
        assert!(bundle.expect("bundle should load").is_some());
        let defaults = defaults.expect("product defaults should load");
        assert_eq!(
            defaults
                .config()
                .get("features")
                .and_then(TomlValue::as_table)
                .and_then(|features| features.get("plugin_sharing"))
                .and_then(TomlValue::as_bool),
            Some(false)
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
