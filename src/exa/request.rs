use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SearchRequest {
    pub query: String,
    pub num_results: Option<u32>,
    pub include_domains: Option<Vec<String>>,
    pub exclude_domains: Option<Vec<String>>,
    pub r#type: Option<String>,
    pub category: Option<String>,
    pub moderation: Option<bool>,
    pub contents: Option<ContentOptions>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContentsRequest {
    pub urls: Option<Vec<String>>,
    pub ids: Option<Vec<String>>,
    pub text: Option<TextMode>,
    pub highlights: Option<HighlightMode>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContentOptions {
    pub text: Option<TextMode>,
    pub highlights: Option<HighlightMode>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum TextMode {
    Enabled(bool),
    Options(TextOptions),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct TextOptions {
    max_characters: Option<usize>,
    include_html_tags: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum HighlightMode {
    Enabled(bool),
    Options(HighlightOptions),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct HighlightOptions {
    max_characters: Option<usize>,
    query: Option<String>,
}

impl TextMode {
    pub fn enabled(&self) -> bool {
        !matches!(self, Self::Enabled(false))
    }

    pub fn limit(&self) -> Option<usize> {
        match self {
            Self::Options(o) => o.max_characters,
            _ => None,
        }
    }
}

impl HighlightMode {
    pub fn enabled(&self) -> bool {
        !matches!(self, Self::Enabled(false))
    }

    pub fn limit(&self) -> Option<usize> {
        match self {
            Self::Options(o) => o.max_characters,
            _ => None,
        }
    }

    pub fn query(&self) -> Option<&str> {
        match self {
            Self::Options(o) => o.query.as_deref(),
            _ => None,
        }
    }
}

impl ContentOptions {
    pub fn wants_text(&self) -> bool {
        self.text.as_ref().is_some_and(TextMode::enabled)
    }

    pub fn wants_highlights(&self) -> bool {
        self.highlights.as_ref().is_some_and(HighlightMode::enabled)
    }

    pub fn highlight_query(&self) -> Option<&str> {
        self.highlights.as_ref().and_then(HighlightMode::query)
    }

    pub fn validate(&self) -> Result<(), ApiError> {
        if let Some(TextMode::Options(o)) = &self.text {
            validate_limit(o.max_characters)?;
            if o.include_html_tags == Some(true) {
                return Err(ApiError::unsupported(
                    "text.includeHtmlTags=true is not supported.",
                ));
            }
        }
        if let Some(HighlightMode::Options(o)) = &self.highlights {
            validate_limit(o.max_characters)?;
            if o.query.as_ref().is_some_and(|q| q.trim().is_empty()) {
                return Err(ApiError::invalid("highlights.query must not be empty."));
            }
        }
        Ok(())
    }
}

fn validate_limit(limit: Option<usize>) -> Result<(), ApiError> {
    if limit.is_some_and(|n| !(1..=1_000_000).contains(&n)) {
        return Err(ApiError::invalid(
            "maxCharacters must be between 1 and 1000000.",
        ));
    }
    Ok(())
}

impl SearchRequest {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.query.trim().is_empty() {
            return Err(ApiError::invalid("query must not be empty."));
        }
        if !matches!(
            self.r#type.as_deref(),
            None | Some("auto" | "fast" | "instant")
        ) {
            return Err(ApiError::unsupported(
                "Supported search types: auto, fast, instant.",
            ));
        }
        if !matches!(self.category.as_deref(), None | Some("news")) {
            return Err(ApiError::unsupported("Only category=news is supported."));
        }
        if self.num_results.is_some_and(|n| !(1..=20).contains(&n)) {
            return Err(ApiError::unsupported(
                "numResults must be between 1 and 20 for the Tavily backend.",
            ));
        }
        for (domains, limit) in [(&self.include_domains, 300), (&self.exclude_domains, 150)] {
            if let Some(domains) = domains {
                if domains.len() > limit {
                    return Err(ApiError::unsupported(format!(
                        "Domain list exceeds the upstream limit of {limit}."
                    )));
                }
                for domain in domains {
                    if domain.is_empty()
                        || domain
                            .chars()
                            .any(|c| c.is_whitespace() || "/:*?#@\\".contains(c))
                        || reqwest::Url::parse(&format!("https://{domain}")).is_err()
                    {
                        return Err(ApiError::unsupported(
                            "Domain filters must be plain hostnames; paths and wildcards are not supported.",
                        ));
                    }
                }
            }
        }
        if let Some(contents) = &self.contents {
            contents.validate()?;
        }
        Ok(())
    }

    pub fn upstream_body(&self) -> Value {
        let depth = match self.r#type.as_deref() {
            Some("fast") => "fast",
            Some("instant") => "ultra-fast",
            _ => "basic",
        };
        let mut body = json!({
            "query": self.query,
            "max_results": self.num_results.unwrap_or(10),
            "search_depth": depth,
            "include_answer": false,
            "include_published_date": true,
            "include_raw_content": false,
        });
        if self
            .contents
            .as_ref()
            .is_some_and(ContentOptions::wants_text)
        {
            body["include_raw_content"] = json!("markdown");
        }
        if let Some(domains) = &self.include_domains {
            body["include_domains"] = json!(domains);
        }
        if let Some(domains) = &self.exclude_domains {
            body["exclude_domains"] = json!(domains);
        }
        if let Some(category) = &self.category {
            body["topic"] = json!(category);
        }
        if let Some(moderation) = self.moderation {
            body["safe_search"] = json!(moderation);
        }
        body
    }
}

impl ContentsRequest {
    pub fn into_parts(self) -> Result<(Vec<String>, ContentOptions), ApiError> {
        let urls = match (self.urls, self.ids) {
            (Some(urls), None) | (None, Some(urls)) => urls,
            _ => return Err(ApiError::invalid("Provide exactly one of urls or ids.")),
        };
        if !(1..=100).contains(&urls.len()) {
            return Err(ApiError::invalid(
                "urls/ids must contain between 1 and 100 entries.",
            ));
        }
        for url in &urls {
            if url.chars().count() > 2048 || !is_http_url(url) {
                return Err(ApiError::invalid(
                    "urls/ids must be HTTP(S) URLs of at most 2048 characters; opaque Exa IDs are not supported.",
                ));
            }
        }
        let text = if self.text.is_none() && self.highlights.is_none() {
            Some(TextMode::Enabled(true))
        } else {
            self.text
        };
        let options = ContentOptions {
            text,
            highlights: self.highlights,
        };
        options.validate()?;
        Ok((urls, options))
    }
}

pub(super) fn is_http_url(raw: &str) -> bool {
    raw.trim() == raw
        && !raw.chars().any(char::is_control)
        && reqwest::Url::parse(raw).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host_str().is_some()
                && u.username().is_empty()
                && u.password().is_none()
        })
}
