use super::{EnrichmentData, EnrichmentProvider, ProviderConfig};

use crate::error::{CarrelError, CarrelResult};

const BASE_URL: &str = "https://comicvine.gamespot.com/api";
const USER_AGENT: &str = "Carrel/1.0 (Desktop eBook Reader)";

#[derive(Default)]
pub struct ComicVineProvider {
    config: ProviderConfig,
}

impl ComicVineProvider {
    pub fn new() -> Self {
        Self {
            config: ProviderConfig {
                enabled: false,
                api_key: None,
            },
        }
    }
}

impl EnrichmentProvider for ComicVineProvider {
    fn id(&self) -> &str {
        "comic_vine"
    }

    fn name(&self) -> &str {
        "Comic Vine"
    }

    fn requires_api_key(&self) -> bool {
        true
    }

    fn api_key_help(&self) -> &str {
        "Free key from comicvine.gamespot.com/api"
    }

    fn search_by_isbn(&self, _isbn: &str) -> CarrelResult<Vec<EnrichmentData>> {
        // Comic Vine has no ISBN field
        Ok(Vec::new())
    }

    fn search_by_title(
        &self,
        title: &str,
        author: Option<&str>,
    ) -> CarrelResult<Vec<EnrichmentData>> {
        let api_key = self
            .config
            .api_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| CarrelError::invalid("Comic Vine requires an API key"))?;

        let query = match author {
            Some(a) if !a.is_empty() => format!("{} {}", title, a),
            _ => title.to_string(),
        };
        let encoded = urlencoding(&query);

        // Tier 1: search volumes
        let url = format!(
            "{}/search/?api_key={}&format=json&resources=volume&query={}&limit=5",
            BASE_URL, api_key, encoded
        );
        let results = fetch_and_parse(&url, parse_volume)?;
        if !results.is_empty() {
            return Ok(results);
        }

        // Tier 2: search issues
        let url = format!(
            "{}/search/?api_key={}&format=json&resources=issue&query={}&limit=5",
            BASE_URL, api_key, encoded
        );
        fetch_and_parse(&url, parse_issue)
    }

    fn configure(&mut self, config: ProviderConfig) {
        self.config = config;
    }

    fn config(&self) -> &ProviderConfig {
        &self.config
    }
}

fn fetch_and_parse(
    url: &str,
    parser: fn(&serde_json::Value) -> Option<EnrichmentData>,
) -> CarrelResult<Vec<EnrichmentData>> {
    let client = reqwest::blocking::Client::new();
    let resp = crate::http_retry::send_with_retry(
        client.get(url).header("User-Agent", USER_AGENT),
        "comic_vine",
        &crate::http_retry::RetryPolicy::default(),
    )?;

    if !resp.status().is_success() {
        return Err(CarrelError::network(format!(
            "Comic Vine HTTP {}",
            resp.status()
        )));
    }

    let json: serde_json::Value = resp.json()?;

    let results = match json["results"].as_array() {
        Some(arr) => arr,
        None => return Ok(Vec::new()),
    };

    Ok(results.iter().filter_map(parser).collect())
}

fn parse_volume(item: &serde_json::Value) -> Option<EnrichmentData> {
    let name = item["name"].as_str().unwrap_or("").to_string();
    if name.is_empty() {
        return None;
    }

    let description = item["deck"].as_str().map(|s| s.to_string());
    let cover_url = item["image"]["medium_url"].as_str().map(|s| s.to_string());
    let publisher = item["publisher"]["name"].as_str().map(|s| s.to_string());
    let publish_year = item["start_year"]
        .as_str()
        .and_then(|s| s.parse::<u16>().ok());
    let source_key = item["id"].as_u64().map(|id| id.to_string());

    Some(EnrichmentData {
        title: name.clone(),
        author: String::new(),
        description,
        genres: Vec::new(),
        rating: None,
        isbn: None,
        cover_url,
        language: None,
        publisher,
        publish_year,
        source: "comic_vine".to_string(),
        source_key,
    })
}

fn parse_issue(item: &serde_json::Value) -> Option<EnrichmentData> {
    let volume_name = item["volume"]["name"].as_str().unwrap_or("");
    let issue_number = item["issue_number"].as_str().unwrap_or("");

    // Use explicit name if present, otherwise build from volume + issue number
    let title = match item["name"].as_str() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ if !volume_name.is_empty() && !issue_number.is_empty() => {
            format!("{} #{}", volume_name, issue_number)
        }
        _ if !volume_name.is_empty() => volume_name.to_string(),
        _ => return None,
    };

    let description = item["deck"].as_str().map(|s| s.to_string());
    let cover_url = item["image"]["medium_url"].as_str().map(|s| s.to_string());
    let publish_year = item["cover_date"]
        .as_str()
        .and_then(|s| s.split('-').next())
        .and_then(|y| y.parse::<u16>().ok());
    let source_key = item["id"].as_u64().map(|id| id.to_string());

    Some(EnrichmentData {
        title,
        author: String::new(),
        description,
        genres: Vec::new(),
        rating: None,
        isbn: None,
        cover_url,
        language: None,
        publisher: None,
        publish_year,
        source: "comic_vine".to_string(),
        source_key,
    })
}

fn urlencoding(s: &str) -> String {
    s.replace('%', "%25")
        .replace(' ', "+")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('#', "%23")
        .replace('?', "%3F")
        .replace('/', "%2F")
        .replace(':', "%3A")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_volume_complete() {
        let item = serde_json::json!({
            "id": 12345,
            "name": "Astérix",
            "deck": "A series of French comic books.",
            "image": {
                "medium_url": "https://comicvine.gamespot.com/a/uploads/scale_medium/asterix.jpg"
            },
            "publisher": {
                "name": "Dargaud"
            },
            "start_year": "1959"
        });

        let result = parse_volume(&item).expect("should parse");
        assert_eq!(result.title, "Astérix");
        assert_eq!(
            result.description.as_deref(),
            Some("A series of French comic books.")
        );
        assert_eq!(result.publisher.as_deref(), Some("Dargaud"));
        assert_eq!(result.publish_year, Some(1959));
        assert!(result.cover_url.is_some());
        assert_eq!(result.source, "comic_vine");
        assert_eq!(result.source_key.as_deref(), Some("12345"));
    }

    #[test]
    fn parse_volume_minimal() {
        let item = serde_json::json!({
            "name": "Tintin"
        });

        let result = parse_volume(&item).expect("should parse minimal");
        assert_eq!(result.title, "Tintin");
        assert!(result.description.is_none());
        assert!(result.publisher.is_none());
        assert!(result.publish_year.is_none());
        assert!(result.cover_url.is_none());
        assert_eq!(result.source, "comic_vine");
    }

    #[test]
    fn parse_volume_empty_name_returns_none() {
        let item = serde_json::json!({
            "name": "",
            "deck": "Some description"
        });
        assert!(parse_volume(&item).is_none());
    }

    #[test]
    fn parse_issue_with_name() {
        let item = serde_json::json!({
            "id": 67890,
            "name": "The Blue Lotus",
            "volume": { "name": "Tintin" },
            "issue_number": "5",
            "deck": "Tintin travels to China.",
            "image": {
                "medium_url": "https://comicvine.gamespot.com/a/uploads/scale_medium/blue_lotus.jpg"
            },
            "cover_date": "1936-01-01"
        });

        let result = parse_issue(&item).expect("should parse");
        assert_eq!(result.title, "The Blue Lotus");
        assert_eq!(
            result.description.as_deref(),
            Some("Tintin travels to China.")
        );
        assert_eq!(result.publish_year, Some(1936));
        assert!(result.cover_url.is_some());
        assert_eq!(result.source, "comic_vine");
        assert_eq!(result.source_key.as_deref(), Some("67890"));
    }

    #[test]
    fn parse_issue_builds_title_from_volume_and_number() {
        let item = serde_json::json!({
            "id": 11111,
            "name": null,
            "volume": { "name": "Astérix" },
            "issue_number": "42",
            "cover_date": "2023-10-26"
        });

        let result = parse_issue(&item).expect("should parse");
        assert_eq!(result.title, "Astérix #42");
        assert_eq!(result.publish_year, Some(2023));
    }

    #[test]
    fn parse_issue_no_volume_no_name_returns_none() {
        let item = serde_json::json!({
            "name": null,
            "volume": { "name": "" },
            "issue_number": ""
        });
        assert!(parse_issue(&item).is_none());
    }

    #[test]
    fn provider_metadata() {
        let provider = ComicVineProvider::new();
        assert_eq!(provider.id(), "comic_vine");
        assert_eq!(provider.name(), "Comic Vine");
        assert!(provider.requires_api_key());
        assert!(!provider.config().enabled);
    }

    #[test]
    fn search_by_isbn_returns_empty() {
        let provider = ComicVineProvider::new();
        let result = provider.search_by_isbn("9781234567890").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn search_by_title_without_key_returns_error() {
        let provider = ComicVineProvider::new();
        let result = provider.search_by_title("Astérix", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key"));
    }

    #[test]
    fn urlencoding_spaces_and_special() {
        assert_eq!(urlencoding("hello world"), "hello+world");
        assert_eq!(urlencoding("a&b=c#d"), "a%26b%3Dc%23d");
    }
}
