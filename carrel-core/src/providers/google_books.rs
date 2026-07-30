use super::{EnrichmentData, EnrichmentProvider, ProviderConfig};

use crate::error::{CarrelError, CarrelResult};

#[derive(Default)]
pub struct GoogleBooksProvider {
    config: ProviderConfig,
}

impl GoogleBooksProvider {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EnrichmentProvider for GoogleBooksProvider {
    fn id(&self) -> &str {
        "google_books"
    }

    fn name(&self) -> &str {
        "Google Books"
    }

    fn requires_api_key(&self) -> bool {
        false
    }

    fn api_key_help(&self) -> &str {
        "Optional. Get a free key at https://console.cloud.google.com/apis/credentials for higher rate limits."
    }

    fn search_by_isbn(&self, isbn: &str) -> CarrelResult<Vec<EnrichmentData>> {
        let url = build_search_url(&format!("isbn:{}", isbn), &self.config);
        fetch_and_parse(&url)
    }

    fn search_by_title(
        &self,
        title: &str,
        author: Option<&str>,
    ) -> CarrelResult<Vec<EnrichmentData>> {
        let mut query = format!("intitle:{}", title);
        if let Some(a) = author {
            if !a.is_empty() {
                query.push_str(&format!("+inauthor:{}", a));
            }
        }
        let url = build_search_url(&query, &self.config);
        fetch_and_parse(&url)
    }

    fn configure(&mut self, config: ProviderConfig) {
        self.config = config;
    }

    fn config(&self) -> &ProviderConfig {
        &self.config
    }
}

/// Build the Google Books API search URL with optional API key.
fn build_search_url(query: &str, config: &ProviderConfig) -> String {
    let encoded = urlencoding(query);
    let mut url = format!(
        "https://www.googleapis.com/books/v1/volumes?q={}&maxResults=5",
        encoded
    );
    if let Some(ref key) = config.api_key {
        if !key.is_empty() {
            url.push_str(&format!("&key={}", key));
        }
    }
    url
}

fn fetch_and_parse(url: &str) -> CarrelResult<Vec<EnrichmentData>> {
    let resp = crate::http_retry::send_with_retry(
        reqwest::blocking::Client::new().get(url),
        "google_books",
        &crate::http_retry::RetryPolicy::default(),
    )?;
    if !resp.status().is_success() {
        return Err(CarrelError::network(format!(
            "Google Books HTTP {}",
            resp.status()
        )));
    }
    let json: serde_json::Value = resp
        .json()
        .map_err(|e| CarrelError::network(format!("JSON parse error: {e}")))?;

    // Google Books returns no `items` key when 0 results
    let items = match json["items"].as_array() {
        Some(arr) => arr,
        None => return Ok(Vec::new()),
    };

    Ok(items.iter().filter_map(parse_volume).collect())
}

/// Parse a single Google Books volume item into `EnrichmentData`.
fn parse_volume(item: &serde_json::Value) -> Option<EnrichmentData> {
    let info = &item["volumeInfo"];
    let title = info["title"].as_str().unwrap_or("").to_string();
    if title.is_empty() {
        return None;
    }

    let author = info["authors"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let description = info["description"].as_str().map(|s| s.to_string());

    let genres: Vec<String> = info["categories"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    let language = info["language"].as_str().map(|s| s.to_string());
    let publisher = info["publisher"].as_str().map(|s| s.to_string());
    let publish_year = info["publishedDate"].as_str().and_then(extract_year);

    // Prefer ISBN_13, fall back to ISBN_10
    let isbn = info["industryIdentifiers"]
        .as_array()
        .and_then(|ids| {
            ids.iter()
                .find(|id| id["type"].as_str() == Some("ISBN_13"))
                .or_else(|| ids.iter().find(|id| id["type"].as_str() == Some("ISBN_10")))
        })
        .and_then(|id| id["identifier"].as_str())
        .map(|s| s.to_string());

    // Use thumbnail, replacing http with https
    let cover_url = info["imageLinks"]["thumbnail"]
        .as_str()
        .map(|s| s.replace("http://", "https://"));

    let source_key = item["id"].as_str().map(|s| s.to_string());

    Some(EnrichmentData {
        title,
        author,
        description,
        genres,
        rating: None,
        isbn,
        cover_url,
        language,
        publisher,
        publish_year,
        source: "google_books".to_string(),
        source_key,
    })
}

/// Extract a 4-digit year from a date string like "2008-06-01" or "2008".
fn extract_year(date: &str) -> Option<u16> {
    let year_str = date.split('-').next()?;
    year_str.parse::<u16>().ok()
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
    fn parse_volume_complete() {
        let item = serde_json::json!({
            "id": "abc123",
            "volumeInfo": {
                "title": "Clean Code",
                "authors": ["Robert C. Martin"],
                "description": "A handbook of agile software craftsmanship.",
                "categories": ["Computers", "Software Engineering"],
                "language": "en",
                "publisher": "Prentice Hall",
                "publishedDate": "2008-08-01",
                "industryIdentifiers": [
                    {"type": "ISBN_10", "identifier": "0132350882"},
                    {"type": "ISBN_13", "identifier": "9780132350884"}
                ],
                "imageLinks": {
                    "thumbnail": "http://books.google.com/books/content?id=abc123&printsec=frontcover&img=1&zoom=1"
                }
            }
        });

        let result = parse_volume(&item).expect("should parse successfully");
        assert_eq!(result.title, "Clean Code");
        assert_eq!(result.author, "Robert C. Martin");
        assert_eq!(
            result.description.as_deref(),
            Some("A handbook of agile software craftsmanship.")
        );
        assert_eq!(result.genres, vec!["Computers", "Software Engineering"]);
        assert_eq!(result.isbn.as_deref(), Some("9780132350884"));
        assert_eq!(result.language.as_deref(), Some("en"));
        assert_eq!(result.publisher.as_deref(), Some("Prentice Hall"));
        assert_eq!(result.publish_year, Some(2008));
        assert!(result.cover_url.as_deref().unwrap().starts_with("https://"));
        assert_eq!(result.source, "google_books");
        assert_eq!(result.source_key.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_volume_minimal() {
        let item = serde_json::json!({
            "volumeInfo": {
                "title": "Some Book"
            }
        });

        let result = parse_volume(&item).expect("should parse minimal volume");
        assert_eq!(result.title, "Some Book");
        assert_eq!(result.author, "");
        assert!(result.description.is_none());
        assert!(result.genres.is_empty());
        assert!(result.isbn.is_none());
        assert!(result.cover_url.is_none());
        assert!(result.language.is_none());
        assert!(result.publisher.is_none());
        assert!(result.publish_year.is_none());
        assert_eq!(result.source, "google_books");
        assert!(result.source_key.is_none());
    }

    #[test]
    fn parse_volume_empty_title_returns_none() {
        let item = serde_json::json!({
            "volumeInfo": {
                "title": "",
                "authors": ["Someone"]
            }
        });
        assert!(parse_volume(&item).is_none());
    }

    #[test]
    fn parse_volume_prefers_isbn_13() {
        let item = serde_json::json!({
            "volumeInfo": {
                "title": "Test",
                "industryIdentifiers": [
                    {"type": "ISBN_10", "identifier": "1234567890"},
                    {"type": "ISBN_13", "identifier": "9781234567890"}
                ]
            }
        });
        let result = parse_volume(&item).unwrap();
        assert_eq!(result.isbn.as_deref(), Some("9781234567890"));
    }

    #[test]
    fn parse_volume_falls_back_to_isbn_10() {
        let item = serde_json::json!({
            "volumeInfo": {
                "title": "Test",
                "industryIdentifiers": [
                    {"type": "ISBN_10", "identifier": "1234567890"}
                ]
            }
        });
        let result = parse_volume(&item).unwrap();
        assert_eq!(result.isbn.as_deref(), Some("1234567890"));
    }

    #[test]
    fn extract_year_full_date() {
        assert_eq!(extract_year("2008-06-01"), Some(2008));
    }

    #[test]
    fn extract_year_only() {
        assert_eq!(extract_year("2020"), Some(2020));
    }

    #[test]
    fn extract_year_invalid() {
        assert_eq!(extract_year("unknown"), None);
    }

    #[test]
    fn extract_year_empty() {
        assert_eq!(extract_year(""), None);
    }

    #[test]
    fn build_search_url_without_key() {
        let config = ProviderConfig {
            enabled: true,
            api_key: None,
        };
        let url = build_search_url("isbn:9780132350884", &config);
        assert_eq!(
            url,
            "https://www.googleapis.com/books/v1/volumes?q=isbn:9780132350884&maxResults=5"
        );
    }

    #[test]
    fn build_search_url_with_key() {
        let config = ProviderConfig {
            enabled: true,
            api_key: Some("MY_KEY".to_string()),
        };
        let url = build_search_url("isbn:9780132350884", &config);
        assert_eq!(
            url,
            "https://www.googleapis.com/books/v1/volumes?q=isbn:9780132350884&maxResults=5&key=MY_KEY"
        );
    }

    #[test]
    fn build_search_url_with_empty_key() {
        let config = ProviderConfig {
            enabled: true,
            api_key: Some(String::new()),
        };
        let url = build_search_url("intitle:Clean+Code", &config);
        assert!(!url.contains("&key="));
    }

    #[test]
    fn provider_metadata() {
        let provider = GoogleBooksProvider::new();
        assert_eq!(provider.id(), "google_books");
        assert_eq!(provider.name(), "Google Books");
        assert!(!provider.requires_api_key());
        assert!(provider.config().enabled);
    }
}
