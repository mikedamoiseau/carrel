pub mod bnf;
pub mod comic_vine;
pub mod google_books;
pub mod openlibrary;

use serde::{Deserialize, Serialize};

use crate::error::CarrelResult;

/// Common metadata returned by any enrichment provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichmentData {
    pub title: String,
    pub author: String,
    pub description: Option<String>,
    pub genres: Vec<String>,
    pub rating: Option<f64>,
    pub isbn: Option<String>,
    pub cover_url: Option<String>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    pub publish_year: Option<u16>,
    /// Identifier for the source (e.g., "openlibrary", "google_books")
    pub source: String,
    /// Provider-specific key for this result
    pub source_key: Option<String>,
}

/// Per-provider configuration stored in the settings table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub enabled: bool,
    pub api_key: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key: None,
        }
    }
}

/// Metadata about a provider (for UI display and registration).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub requires_api_key: bool,
    pub api_key_help: String,
    pub config: ProviderConfig,
}

/// Trait that all enrichment providers implement.
pub trait EnrichmentProvider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn requires_api_key(&self) -> bool;
    fn api_key_help(&self) -> &str;
    fn search_by_isbn(&self, isbn: &str) -> CarrelResult<Vec<EnrichmentData>>;
    fn search_by_title(
        &self,
        title: &str,
        author: Option<&str>,
    ) -> CarrelResult<Vec<EnrichmentData>>;
    fn configure(&mut self, config: ProviderConfig);
    fn config(&self) -> &ProviderConfig;
}

pub struct ProviderRegistry {
    providers: Vec<Box<dyn EnrichmentProvider>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: vec![
                Box::new(google_books::GoogleBooksProvider::new()),
                Box::new(openlibrary::OpenLibraryProvider::new()),
                Box::new(comic_vine::ComicVineProvider::new()),
                Box::new(bnf::BnfProvider::new()),
            ],
        }
    }

    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        self.providers
            .iter()
            .map(|p| ProviderInfo {
                id: p.id().to_string(),
                name: p.name().to_string(),
                requires_api_key: p.requires_api_key(),
                api_key_help: p.api_key_help().to_string(),
                config: p.config().clone(),
            })
            .collect()
    }

    pub fn enabled_providers(&self) -> Vec<String> {
        self.providers
            .iter()
            .filter(|p| p.config().enabled)
            .map(|p| p.id().to_string())
            .collect()
    }

    pub fn configure_provider(&mut self, id: &str, config: ProviderConfig) {
        if let Some(p) = self.providers.iter_mut().find(|p| p.id() == id) {
            p.configure(config);
        }
    }

    /// Reorder providers to match the given ID order.
    /// IDs not found in the registry are skipped.
    /// Providers not listed in `order` are appended at the end in their current relative order.
    pub fn reorder(&mut self, order: &[String]) {
        let mut ordered: Vec<Box<dyn EnrichmentProvider>> = Vec::new();
        let mut remaining = std::mem::take(&mut self.providers);

        for id in order {
            if let Some(pos) = remaining.iter().position(|p| p.id() == id) {
                ordered.push(remaining.remove(pos));
            }
        }
        ordered.append(&mut remaining);
        self.providers = ordered;
    }

    /// Search by ISBN across enabled providers. Returns first non-empty result
    /// along with the list of providers that were tried.
    pub fn search_by_isbn(&self, isbn: &str) -> SearchOutcome {
        let mut tried = Vec::new();
        for provider in &self.providers {
            if !provider.config().enabled {
                continue;
            }
            tried.push(provider.name().to_string());
            match provider.search_by_isbn(isbn) {
                Ok(results) if !results.is_empty() => {
                    return SearchOutcome {
                        results,
                        providers_tried: tried,
                    };
                }
                _ => continue,
            }
        }
        SearchOutcome {
            results: Vec::new(),
            providers_tried: tried,
        }
    }

    /// Search by title across enabled providers. Returns first non-empty result
    /// along with the list of providers that were tried.
    pub fn search_by_title(&self, title: &str, author: Option<&str>) -> SearchOutcome {
        let mut tried = Vec::new();
        for provider in &self.providers {
            if !provider.config().enabled {
                continue;
            }
            tried.push(provider.name().to_string());
            match provider.search_by_title(title, author) {
                Ok(results) if !results.is_empty() => {
                    return SearchOutcome {
                        results,
                        providers_tried: tried,
                    };
                }
                _ => continue,
            }
        }
        SearchOutcome {
            results: Vec::new(),
            providers_tried: tried,
        }
    }
}

/// Result of a multi-provider search, including which providers were queried.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<EnrichmentData>,
    /// Names of providers that were tried, in order (e.g. ["Google Books", "OpenLibrary"]).
    pub providers_tried: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrichment_data_default() {
        let data = EnrichmentData::default();
        assert_eq!(data.title, "");
        assert_eq!(data.author, "");
        assert!(data.description.is_none());
        assert!(data.genres.is_empty());
        assert!(data.rating.is_none());
        assert!(data.isbn.is_none());
        assert!(data.cover_url.is_none());
        assert!(data.language.is_none());
        assert!(data.publisher.is_none());
        assert!(data.publish_year.is_none());
        assert_eq!(data.source, "");
        assert!(data.source_key.is_none());
    }

    #[test]
    fn provider_config_default() {
        let config = ProviderConfig::default();
        assert!(config.enabled);
        assert!(config.api_key.is_none());
    }

    #[test]
    fn registry_lists_providers_in_order() {
        let reg = ProviderRegistry::new();
        let providers = reg.list_providers();
        assert_eq!(providers.len(), 4);
        assert_eq!(providers[0].id, "google_books");
        assert_eq!(providers[0].name, "Google Books");
        assert_eq!(providers[1].id, "openlibrary");
        assert_eq!(providers[1].name, "OpenLibrary");
        assert_eq!(providers[2].id, "comic_vine");
        assert_eq!(providers[2].name, "Comic Vine");
        assert_eq!(providers[3].id, "bnf");
        assert_eq!(providers[3].name, "BnF (Bibliothèque nationale de France)");
    }

    #[test]
    fn registry_get_enabled_providers() {
        let mut reg = ProviderRegistry::new();
        // Google Books + OpenLibrary + BnF enabled; Comic Vine disabled (needs API key)
        assert_eq!(reg.enabled_providers().len(), 3);
        reg.configure_provider(
            "google_books",
            ProviderConfig {
                enabled: false,
                api_key: None,
            },
        );
        let enabled = reg.enabled_providers();
        assert_eq!(enabled.len(), 2);
        assert_eq!(enabled[0], "openlibrary");
        assert_eq!(enabled[1], "bnf");
    }

    #[test]
    fn reorder_changes_provider_order() {
        let mut reg = ProviderRegistry::new();
        reg.reorder(&[
            "bnf".to_string(),
            "comic_vine".to_string(),
            "openlibrary".to_string(),
            "google_books".to_string(),
        ]);
        let providers = reg.list_providers();
        assert_eq!(providers[0].id, "bnf");
        assert_eq!(providers[1].id, "comic_vine");
        assert_eq!(providers[2].id, "openlibrary");
        assert_eq!(providers[3].id, "google_books");
    }

    #[test]
    fn reorder_appends_unlisted_providers_at_end() {
        let mut reg = ProviderRegistry::new();
        reg.reorder(&["bnf".to_string(), "openlibrary".to_string()]);
        let providers = reg.list_providers();
        assert_eq!(providers[0].id, "bnf");
        assert_eq!(providers[1].id, "openlibrary");
        assert_eq!(providers[2].id, "google_books");
        assert_eq!(providers[3].id, "comic_vine");
    }

    #[test]
    fn reorder_ignores_unknown_ids() {
        let mut reg = ProviderRegistry::new();
        reg.reorder(&[
            "nonexistent".to_string(),
            "bnf".to_string(),
            "google_books".to_string(),
        ]);
        let providers = reg.list_providers();
        assert_eq!(providers[0].id, "bnf");
        assert_eq!(providers[1].id, "google_books");
        assert_eq!(providers[2].id, "openlibrary");
        assert_eq!(providers[3].id, "comic_vine");
    }

    #[test]
    fn reorder_with_empty_order_is_noop() {
        let mut reg = ProviderRegistry::new();
        reg.reorder(&[]);
        let providers = reg.list_providers();
        assert_eq!(providers[0].id, "google_books");
        assert_eq!(providers[1].id, "openlibrary");
        assert_eq!(providers[2].id, "comic_vine");
        assert_eq!(providers[3].id, "bnf");
    }
}
