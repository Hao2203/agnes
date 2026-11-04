use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use super::RegistryError;

/// A model provider failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ModelError {
    /// The provider could not produce a completion.
    #[error("model failed: {0}")]
    Failed(String),
}

/// A minimal asynchronous model provider used by the demo layer.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// The provider name used for registration and lookup.
    fn name(&self) -> &str;

    /// Produces one completion for a prompt.
    async fn complete(&self, prompt: &str) -> Result<String, ModelError>;
}

/// A set of model providers keyed by name.
#[derive(Default)]
pub struct ModelRegistry {
    providers: BTreeMap<String, Arc<dyn ModelProvider>>,
}

impl ModelRegistry {
    /// Registers a provider under an explicit name.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Duplicate`] when the name is already taken.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<(), RegistryError> {
        let name = name.into();
        if self.providers.contains_key(&name) {
            return Err(RegistryError::Duplicate {
                kind: "model provider",
                name,
            });
        }
        self.providers.insert(name, provider);
        Ok(())
    }

    /// Looks up a provider by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn ModelProvider>> {
        self.providers.get(name)
    }

    /// The provider named `default`, or the first registered provider.
    pub fn default_provider(&self) -> Option<&Arc<dyn ModelProvider>> {
        self.providers
            .get("default")
            .or_else(|| self.providers.values().next())
    }

    /// Registered provider names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }
}
