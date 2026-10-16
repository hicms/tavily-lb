use chrono::{DateTime, NaiveDate};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;
use super::request::{ContentOptions, is_http_url};

#[derive(Deserialize)]
pub(super) struct TavilyResponse {
    pub results: Vec<TavilyResult>,
    #[serde(default)]
    pub failed_results: Vec<TavilyFailure>,
}

#[derive(Deserialize)]
pub(super) struct TavilyResult {
    pub url: String,
    pub title: Option<String>,
    pub content: Option<String>,
    pub raw_content: Option<String>,
    pub published_date: Option<String>,
    pub author: Option<String>,
    pub favicon: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct TavilyFailure {
    pub url: String,
}

impl TavilyResponse {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.results.iter().any(|r| !is_http_url(&r.url)) {
            return Err(ApiError::upstream(
                "Upstream returned an invalid result URL.",
            ));
        }
        Ok(())
    }
}

impl TavilyResult {
    pub fn to_exa(&self, options: &ContentOptions, highlight: Option<&str>) -> Value {
        let mut result = json!({
            "id":self.url,
            "url":self.url,
            "title":self.title.as_deref().filter(|t| !t.is_empty()).unwrap_or(&self.url),
        });
        if let Some(author) = &self.author {
            result["author"] = json!(author);
        }
        if let Some(favicon) = self.favicon.as_deref().filter(|url| is_http_url(url)) {
            result["favicon"] = json!(favicon);
        }
        if let Some(date) = self.published_date.as_deref().and_then(published_date) {
            result["publishedDate"] = json!(date);
        }
        if let (true, Some(text)) = (options.wants_text(), &self.raw_content) {
            let limit = options.text.as_ref().and_then(|o| o.limit());
            result["text"] = json!(truncate(text, limit));
        }
        if options.wants_highlights() {
            let limit = options.highlights.as_ref().and_then(|o| o.limit());
            let highlights: Vec<String> = highlight
                .filter(|s| !s.is_empty())
                .map(|s| vec![truncate(s, limit)])
                .unwrap_or_default();
            result["highlights"] = json!(highlights);
        }
        result
    }
}

fn truncate(text: &str, limit: Option<usize>) -> String {
    match limit {
        Some(n) => text.chars().take(n).collect(),
        None => text.to_string(),
    }
}

fn published_date(raw: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(raw)
        .or_else(|_| DateTime::parse_from_rfc2822(raw))
        .map(|date| date.to_rfc3339())
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()
                .map(|date| format!("{date}T00:00:00Z"))
        })
}
