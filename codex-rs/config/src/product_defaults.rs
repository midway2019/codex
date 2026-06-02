use crate::CloudRequirementsLoadError;
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

    pub fn from_toml_fragments<'a, I>(fragments: I) -> Result<Self, toml::de::Error>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut config = TomlValue::Table(Map::new());
        for contents in fragments {
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
    fut: Shared<BoxFuture<'static, Result<ProductDefaults, CloudRequirementsLoadError>>>,
}

impl ProductDefaultsLoader {
    pub fn new<F>(fut: F) -> Self
    where
        F: Future<Output = Result<ProductDefaults, CloudRequirementsLoadError>> + Send + 'static,
    {
        Self {
            fut: fut.boxed().shared(),
        }
    }

    pub fn from_defaults(defaults: ProductDefaults) -> Self {
        Self::new(async move { Ok(defaults) })
    }

    pub async fn get(&self) -> Result<ProductDefaults, CloudRequirementsLoadError> {
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
