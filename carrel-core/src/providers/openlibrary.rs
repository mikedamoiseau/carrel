use super::{EnrichmentData, EnrichmentProvider, ProviderConfig};

use crate::error::{CarrelError, CarrelResult};

#[derive(Default)]
pub struct OpenLibraryProvider {
    config: ProviderConfig,
}

impl OpenLibraryProvider {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EnrichmentProvider for OpenLibraryProvider {
    fn id(&self) -> &str {
        "openlibrary"
    }

    fn name(&self) -> &str {
        "OpenLibrary"
    }

    fn requires_api_key(&self) -> bool {
        false
    }

    fn api_key_help(&self) -> &str {
        "No API key required. OpenLibrary is a free, open service."
    }

    fn search_by_isbn(&self, isbn: &str) -> CarrelResult<Vec<EnrichmentData>> {
        let url = format!(
            "https://openlibrary.org/search.json?isbn={}&limit=3&fields=key,title,author_name,first_sentence,subject,isbn,ratings_average,cover_i,language",
            urlencoding(isbn)
        );
        fetch_and_parse(&url)
    }

    fn search_by_title(
        &self,
        title: &str,
        author: Option<&str>,
    ) -> CarrelResult<Vec<EnrichmentData>> {
        let mut query = format!("title={}", urlencoding(title));
        if let Some(a) = author {
            if !a.is_empty() {
                query.push_str(&format!("&author={}", urlencoding(a)));
            }
        }
        let url = format!(
            "https://openlibrary.org/search.json?{}&limit=5&fields=key,title,author_name,first_sentence,subject,isbn,ratings_average,cover_i,language",
            query
        );
        fetch_and_parse(&url)
    }

    fn configure(&mut self, config: ProviderConfig) {
        self.config = config;
    }

    fn config(&self) -> &ProviderConfig {
        &self.config
    }
}

fn fetch_and_parse(url: &str) -> CarrelResult<Vec<EnrichmentData>> {
    let resp = crate::http_retry::send_with_retry(
        reqwest::blocking::Client::new().get(url),
        "openlibrary",
        &crate::http_retry::RetryPolicy::default(),
    )?;
    if !resp.status().is_success() {
        return Err(CarrelError::network(format!(
            "OpenLibrary HTTP {}",
            resp.status()
        )));
    }
    let json: serde_json::Value = resp
        .json()
        .map_err(|e| CarrelError::network(format!("JSON parse error: {e}")))?;

    let docs = json["docs"]
        .as_array()
        .ok_or_else(|| CarrelError::internal("Unexpected response format"))?;

    Ok(docs.iter().filter_map(parse_search_doc).collect())
}

/// Parse a single OpenLibrary search doc into `EnrichmentData`.
fn parse_search_doc(doc: &serde_json::Value) -> Option<EnrichmentData> {
    let title = doc["title"].as_str().unwrap_or("").to_string();
    if title.is_empty() {
        return None;
    }

    let author = doc["author_name"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let description = doc["first_sentence"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let genres: Vec<String> = doc["subject"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .take(10)
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    let rating = doc["ratings_average"].as_f64();

    let isbn = doc["isbn"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let cover_url = doc["cover_i"]
        .as_i64()
        .map(|id| format!("https://covers.openlibrary.org/b/id/{}-L.jpg", id));

    let language = doc["language"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let source_key = doc["key"].as_str().map(|s| s.to_string());

    Some(EnrichmentData {
        title,
        author,
        description,
        genres,
        rating,
        isbn,
        cover_url,
        language,
        publisher: None,
        publish_year: None,
        source: "openlibrary".to_string(),
        source_key,
    })
}

fn urlencoding(s: &str) -> String {
    s.replace(' ', "+")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('#', "%23")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_search_doc_complete() {
        let doc = serde_json::json!({
            "key": "/works/OL12345W",
            "title": "The Great Gatsby",
            "author_name": ["F. Scott Fitzgerald"],
            "first_sentence": ["In my younger and more vulnerable years my father gave me some advice."],
            "subject": ["Fiction", "Classic", "American literature"],
            "isbn": ["9780743273565", "0743273567"],
            "ratings_average": 3.82,
            "cover_i": 8231856,
            "language": ["eng"]
        });

        let result = parse_search_doc(&doc).expect("should parse successfully");
        assert_eq!(result.title, "The Great Gatsby");
        assert_eq!(result.author, "F. Scott Fitzgerald");
        assert_eq!(
            result.description.as_deref(),
            Some("In my younger and more vulnerable years my father gave me some advice.")
        );
        assert_eq!(
            result.genres,
            vec!["Fiction", "Classic", "American literature"]
        );
        assert!((result.rating.unwrap() - 3.82).abs() < f64::EPSILON);
        assert_eq!(result.isbn.as_deref(), Some("9780743273565"));
        assert_eq!(
            result.cover_url.as_deref(),
            Some("https://covers.openlibrary.org/b/id/8231856-L.jpg")
        );
        assert_eq!(result.language.as_deref(), Some("eng"));
        assert_eq!(result.source, "openlibrary");
        assert_eq!(result.source_key.as_deref(), Some("/works/OL12345W"));
    }

    #[test]
    fn parse_search_doc_minimal() {
        let doc = serde_json::json!({
            "title": "Unknown Book"
        });

        let result = parse_search_doc(&doc).expect("should parse minimal doc");
        assert_eq!(result.title, "Unknown Book");
        assert_eq!(result.author, "");
        assert!(result.description.is_none());
        assert!(result.genres.is_empty());
        assert!(result.rating.is_none());
        assert!(result.isbn.is_none());
        assert!(result.cover_url.is_none());
        assert!(result.language.is_none());
        assert_eq!(result.source, "openlibrary");
        assert!(result.source_key.is_none());
    }

    #[test]
    fn parse_search_doc_empty_title_returns_none() {
        let doc = serde_json::json!({
            "title": "",
            "author_name": ["Someone"]
        });
        assert!(parse_search_doc(&doc).is_none());
    }

    #[test]
    fn parse_search_doc_missing_title_returns_none() {
        let doc = serde_json::json!({
            "author_name": ["Someone"]
        });
        assert!(parse_search_doc(&doc).is_none());
    }

    #[test]
    fn provider_metadata() {
        let provider = OpenLibraryProvider::new();
        assert_eq!(provider.id(), "openlibrary");
        assert_eq!(provider.name(), "OpenLibrary");
        assert!(!provider.requires_api_key());
        assert!(provider.config().enabled);
    }

    #[test]
    fn urlencoding_spaces() {
        assert_eq!(urlencoding("hello world"), "hello+world");
    }

    #[test]
    fn urlencoding_special_chars() {
        assert_eq!(urlencoding("a&b=c#d"), "a%26b%3Dc%23d");
    }
}
