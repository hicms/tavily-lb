use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use bytes::Bytes;
use subtle::ConstantTimeEq;

const KEY_FIELDS: [&str; 2] = ["api_key", "apiKey"];

pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

pub fn verify(master: &str, token: &str) -> bool {
    !master.is_empty() && !token.is_empty() && bool::from(master.as_bytes().ct_eq(token.as_bytes()))
}

/// Credentials found on a proxied request, plus the query and body with legacy key fields removed.
#[derive(Debug)]
pub struct Extracted {
    pub tokens: Vec<String>,
    pub query: Option<String>,
    pub body: Bytes,
}

impl Extracted {
    pub fn has_credential(&self) -> bool {
        !self.tokens.is_empty()
    }

    pub fn authorized(&self, master: &str) -> bool {
        self.tokens.iter().any(|t| verify(master, t))
    }
}

pub fn extract(headers: &HeaderMap, query: Option<&str>, body: Bytes) -> Extracted {
    let mut tokens = Vec::new();
    if let Some(t) = bearer_token(headers) {
        tokens.push(t);
    }
    let (query_token, query) = strip_query(query);
    tokens.extend(query_token);
    let (body_token, body) = strip_body(body);
    tokens.extend(body_token);
    Extracted {
        tokens,
        query,
        body,
    }
}

fn strip_query(query: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(raw) = query.filter(|q| !q.is_empty()) else {
        return (None, None);
    };
    let pairs: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
        .into_owned()
        .collect();
    if !pairs.iter().any(|(k, _)| KEY_FIELDS.contains(&k.as_str())) {
        return (None, Some(raw.to_string()));
    }
    let token = KEY_FIELDS.iter().find_map(|field| {
        pairs
            .iter()
            .find(|(k, _)| k == field)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    });
    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs
        .iter()
        .filter(|(k, _)| !KEY_FIELDS.contains(&k.as_str()))
    {
        ser.append_pair(k, v);
    }
    let rebuilt = ser.finish();
    (token, (!rebuilt.is_empty()).then_some(rebuilt))
}

fn strip_body(body: Bytes) -> (Option<String>, Bytes) {
    let trimmed = body.trim_ascii();
    if !trimmed.starts_with(b"{") {
        return (None, body);
    }
    let Ok(serde_json::Value::Object(mut map)) =
        serde_json::from_slice::<serde_json::Value>(trimmed)
    else {
        return (None, body);
    };
    let mut token = None;
    let mut changed = false;
    for field in KEY_FIELDS {
        if let Some(v) = map.shift_remove(field) {
            changed = true;
            if token.is_none() {
                token = v
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
            }
        }
    }
    if !changed {
        return (None, body);
    }
    match serde_json::to_vec(&map) {
        Ok(out) => (token, Bytes::from(out)),
        Err(_) => (token, body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn bearer_parsing() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("bearer  abc "));
        assert_eq!(bearer_token(&h).as_deref(), Some("abc"));
        h.insert(AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer_token(&h), None);
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer"));
        assert_eq!(bearer_token(&h), None);
    }

    #[test]
    fn verify_is_exact() {
        assert!(verify("secret", "secret"));
        assert!(!verify("secret", "secre"));
        assert!(!verify("", ""));
    }

    #[test]
    fn query_untouched_without_key() {
        let (t, q) = strip_query(Some("b=2&a=1+x"));
        assert_eq!(t, None);
        assert_eq!(q.as_deref(), Some("b=2&a=1+x"));
    }

    #[test]
    fn query_strips_both_fields() {
        let (t, q) = strip_query(Some("foo=bar&apiKey=k2&api_key=k1"));
        assert_eq!(t.as_deref(), Some("k1"));
        assert_eq!(q.as_deref(), Some("foo=bar"));
        let (t, q) = strip_query(Some("apiKey=k2"));
        assert_eq!(t.as_deref(), Some("k2"));
        assert_eq!(q, None);
    }

    #[test]
    fn body_strip_preserves_order() {
        let (t, b) = strip_body(Bytes::from_static(
            br#"{"query":"q","api_key":"m","max_results":5,"topic":"news"}"#,
        ));
        assert_eq!(t.as_deref(), Some("m"));
        assert_eq!(&b[..], br#"{"query":"q","max_results":5,"topic":"news"}"#);
    }

    #[test]
    fn body_untouched_when_not_object_or_no_key() {
        let raw = Bytes::from_static(b"[1,2]");
        assert_eq!(strip_body(raw.clone()), (None, raw));
        let raw = Bytes::from_static(br#"{ "query": "q" }"#);
        assert_eq!(strip_body(raw.clone()), (None, raw));
        let raw = Bytes::from_static(b"{broken");
        assert_eq!(strip_body(raw.clone()), (None, raw));
    }

    #[test]
    fn non_string_key_is_removed_without_token() {
        let (t, b) = strip_body(Bytes::from_static(br#"{"api_key":123,"q":1}"#));
        assert_eq!(t, None);
        assert_eq!(&b[..], br#"{"q":1}"#);
    }
}
