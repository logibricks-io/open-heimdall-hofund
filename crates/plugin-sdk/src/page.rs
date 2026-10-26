//! Cursors, bounded model-list walkers, and string ids from JSON pages.
//!
//! The walkers take an async transport closure and never choose an HTTP client.
//! Error text for malformed pages is a fixed sentence and does not include the
//! body. Transport errors pass through unchanged.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use serde_json::Value;
use url::Url;

const CURSOR_KEYS: &[&str] = &["next_page_token", "nextPageToken", "cursor", "next"];

/// The greatest number of pages one model-list walker reads.
pub const MAX_PAGES: usize = 20;
/// The page size requested after the first page, or on every Gemini page.
pub const PAGE_LIMIT: u32 = 1000;
/// The time budget for one candidate `after_id` list or one Gemini page.
pub const LIST_BUDGET: Duration = Duration::from_secs(8);
/// The largest response body a walker accepts, in bytes.
pub const BODY_CAP: usize = 8 * 1024 * 1024;
/// The greatest number of Unicode scalar values retained from a JSON error.
pub const ERROR_RUNES: usize = 160;

/// Why a page could not be read. The message is a fixed sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageError {
    message: &'static str,
}

impl PageError {
    const fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for PageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PageError {}

/// The next cursor, if the page names one.
///
/// The first non-empty string among `next_page_token`, `nextPageToken`,
/// `cursor`, and `next` wins. An empty string is no cursor.
///
/// # Errors
///
/// Returns [`PageError`] when `page` is not a JSON object. The body is not
/// included in the error.
pub fn next_cursor(page: &[u8]) -> Result<Option<String>, PageError> {
    let object = object(page)?;
    for key in CURSOR_KEYS {
        if let Some(cursor) = object.get(*key).and_then(Value::as_str)
            && !cursor.is_empty()
        {
            return Ok(Some(cursor.to_owned()));
        }
    }
    Ok(None)
}

/// String ids under `key`.
///
/// A missing key is an empty list. A string element is kept. An object with
/// a non-empty string `id` contributes that id. Any other element is skipped.
///
/// # Errors
///
/// Returns [`PageError`] when `page` is not a JSON object, `key` is empty, or
/// the value at `key` is present and not an array. The body is not included
/// in the error.
pub fn string_items(page: &[u8], key: &str) -> Result<Vec<String>, PageError> {
    if key.is_empty() {
        return Err(PageError::new("page item key is empty"));
    }
    let object = object(page)?;
    let Some(value) = object.get(key) else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(PageError::new("page items are not an array"));
    };
    let mut ids = Vec::new();
    for item in items {
        if let Some(text) = item.as_str() {
            if !text.is_empty() {
                ids.push(text.to_owned());
            }
            continue;
        }
        if let Some(id) = item.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

/// Reads an OpenAI-style or Anthropic-style list to its end.
///
/// The first request uses `first_url` unchanged. A page with `has_more` adds or
/// replaces `after_id` and `limit`, preserving every other query parameter.
/// Rows come from non-empty `data`, otherwise `models`. Bare strings and
/// objects with a non-empty `id` or `name` are retained as raw JSON values.
/// One [`LIST_BUDGET`] covers the whole walk, not each page.
///
/// # Errors
///
/// Returns the transport error when a fetch fails. Returns [`PageError`]
/// through `E` for an invalid URL, oversized or malformed page, empty list,
/// missing or repeated cursor, elapsed budget, or a list still paging after
/// [`MAX_PAGES`]. Any failure discards the partial list.
pub async fn after_id<F, Fut, E>(first_url: &str, mut fetch: F) -> Result<Vec<Value>, E>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, E>>,
    E: From<PageError>,
{
    tokio::time::timeout(LIST_BUDGET, async {
        let mut rows = Vec::new();
        let mut cursors = HashSet::new();
        let mut after = None;
        for page_number in 0..MAX_PAGES {
            let at = match after.as_deref() {
                Some(cursor) => after_id_url(first_url, cursor).map_err(E::from)?,
                None => first_url.to_owned(),
            };
            let bytes = fetch(at).await?;
            if bytes.len() > BODY_CAP {
                return Err(E::from(PageError::new("model list body is too large")));
            }
            let page = object(&bytes).map_err(E::from)?;
            rows.extend(after_id_rows(&page).map_err(E::from)?);
            if !page
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                if rows.is_empty() {
                    return Err(E::from(PageError::new("no models listed")));
                }
                return Ok(rows);
            }
            let last = page.get("last_id").and_then(Value::as_str).unwrap_or("");
            if last.is_empty() {
                return Err(E::from(PageError::new("paged model list has no last_id")));
            }
            if after.as_deref() == Some(last) || !cursors.insert(last.to_owned()) {
                return Err(E::from(PageError::new("model list cursor repeated")));
            }
            if page_number + 1 == MAX_PAGES {
                return Err(E::from(PageError::new(
                    "model list is still paging after 20 pages",
                )));
            }
            after = Some(last.to_owned());
        }
        unreachable!("the page cap returns from the loop")
    })
    .await
    .map_err(|_| E::from(PageError::new("model list exceeded its time budget")))?
}

/// Reads a Gemini native model list.
///
/// The first URL is `{base}/models?pageSize=1000`; later pages replace
/// `pageToken`. Every page has its own [`LIST_BUDGET`]. A token on the twentieth
/// page returns the rows already read instead of failing. Each returned object
/// retains its metadata and gains an `id` field derived from `name` by removing
/// one leading `models/`. Rows with generation methods are retained only when
/// they contain `generateContent`.
///
/// # Errors
///
/// Returns the transport error when a fetch fails. Returns [`PageError`]
/// through `E` for an empty base, invalid page URL, oversized or malformed
/// page, empty list, repeated cursor, or elapsed page budget. No partial list
/// is returned for those failures.
pub async fn gemini_models<F, Fut, E>(base: &str, mut fetch: F) -> Result<Vec<Value>, E>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, E>>,
    E: From<PageError>,
{
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(E::from(PageError::new("no base URL")));
    }
    let first_url = format!("{base}/models");
    let mut rows = Vec::new();
    let mut cursors = HashSet::new();
    let mut token = None;
    for page_number in 0..MAX_PAGES {
        let at = gemini_url(&first_url, token.as_deref()).map_err(E::from)?;
        let bytes = tokio::time::timeout(LIST_BUDGET, fetch(at))
            .await
            .map_err(|_| E::from(PageError::new("Gemini page exceeded its time budget")))??;
        if bytes.len() > BODY_CAP {
            return Err(E::from(PageError::new("model list body is too large")));
        }
        let page = object(&bytes).map_err(E::from)?;
        rows.extend(gemini_rows(&page).map_err(E::from)?);
        token = next_cursor(&bytes).map_err(E::from)?;
        let Some(next) = token.as_deref() else {
            break;
        };
        if page_number + 1 == MAX_PAGES {
            break;
        }
        if !cursors.insert(next.to_owned()) {
            return Err(E::from(PageError::new("model list cursor repeated")));
        }
    }
    if rows.is_empty() {
        return Err(E::from(PageError::new("no models listed")));
    }
    Ok(rows)
}

/// Reads generic cursor pages with explicit bounds and cycle detection.
///
/// `next_url` receives the first URL and the cursor from the previous page.
/// Returned values are the complete raw JSON page objects. At most `max_pages`
/// requests are made. A cursor seen on any earlier page fails before another
/// request, as does a next cursor on the last permitted page.
///
/// # Errors
///
/// Returns the transport error when a fetch fails. Returns [`PageError`]
/// through `E` when `max_pages` is zero, a body is oversized or malformed, a
/// cursor repeats, or the last permitted page still has a cursor.
pub async fn cursor_pages<F, U, Fut, E>(
    first_url: &str,
    max_pages: usize,
    mut next_url: U,
    mut fetch: F,
) -> Result<Vec<Value>, E>
where
    F: FnMut(String) -> Fut,
    U: FnMut(&str, &str) -> String,
    Fut: Future<Output = Result<Vec<u8>, E>>,
    E: From<PageError>,
{
    if max_pages == 0 {
        return Err(E::from(PageError::new("model list page limit is zero")));
    }
    let mut pages = Vec::new();
    let mut cursors = HashSet::new();
    let mut at = first_url.to_owned();
    for page_number in 0..max_pages {
        let bytes = fetch(at).await?;
        if bytes.len() > BODY_CAP {
            return Err(E::from(PageError::new("model list body is too large")));
        }
        let page: Value = serde_json::from_slice(&bytes)
            .map_err(|_| E::from(PageError::new("page is not JSON")))?;
        if !page.is_object() {
            return Err(E::from(PageError::new("page is not a JSON object")));
        }
        let next = next_cursor(&bytes).map_err(E::from)?;
        let Some(cursor) = next else {
            pages.push(page);
            break;
        };
        if !cursors.insert(cursor.clone()) {
            return Err(E::from(PageError::new("model list cursor repeated")));
        }
        if page_number + 1 == max_pages {
            return Err(E::from(PageError::new("model list reached its page limit")));
        }
        at = next_url(first_url, &cursor);
        pages.push(page);
    }
    Ok(pages)
}

/// Extracts and trims a vendor JSON error, capped at [`ERROR_RUNES`] runes.
///
/// Nested `error.message` wins over a string `error`, which wins over the
/// top-level `message`. Invalid JSON and absent messages produce an empty
/// string. A truncated message ends with an ellipsis.
#[must_use]
pub fn json_error_message(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    let mut message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match value.get("error") {
        Some(Value::Object(error)) => {
            if let Some(nested) = error.get("message").and_then(Value::as_str)
                && !nested.is_empty()
            {
                message = nested;
            }
        }
        Some(Value::String(error)) if !error.is_empty() => message = error,
        _ => {}
    }
    cut_error_message(message)
}

fn cut_error_message(message: &str) -> String {
    let message = message.trim();
    if message.chars().count() <= ERROR_RUNES {
        return message.to_owned();
    }
    let mut cut = message.chars().take(ERROR_RUNES).collect::<String>();
    cut.push('…');
    cut
}

fn after_id_url(first_url: &str, cursor: &str) -> Result<String, PageError> {
    let mut url = Url::parse(first_url).map_err(|_| PageError::new("invalid model list URL"))?;
    let kept = url
        .query_pairs()
        .filter(|(key, _)| key != "after_id" && key != "limit")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair("after_id", cursor)
        .append_pair("limit", &PAGE_LIMIT.to_string());
    Ok(url.into())
}

fn gemini_url(first_url: &str, token: Option<&str>) -> Result<String, PageError> {
    let mut url = Url::parse(first_url).map_err(|_| PageError::new("invalid model list URL"))?;
    url.query_pairs_mut()
        .append_pair("pageSize", &PAGE_LIMIT.to_string());
    if let Some(token) = token {
        url.query_pairs_mut().append_pair("pageToken", token);
    }
    Ok(url.into())
}

fn after_id_rows(page: &serde_json::Map<String, Value>) -> Result<Vec<Value>, PageError> {
    let data = page_items(page, "data")?;
    let rows = if data.is_empty() {
        page_items(page, "models")?
    } else {
        data
    };
    Ok(rows
        .iter()
        .filter(|row| match row {
            Value::String(id) => !id.is_empty(),
            Value::Object(row) => ["id", "name"].iter().any(|key| {
                row.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty())
            }),
            _ => false,
        })
        .cloned()
        .collect())
}

fn page_items<'a>(
    page: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a [Value], PageError> {
    match page.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(rows)) => Ok(rows),
        Some(_) => Err(PageError::new("page items are not an array")),
    }
}

fn gemini_rows(page: &serde_json::Map<String, Value>) -> Result<Vec<Value>, PageError> {
    let mut rows = Vec::new();
    for row in page_items(page, "models")? {
        let Value::Object(row) = row else {
            continue;
        };
        let methods = match row.get("supportedGenerationMethods") {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(methods)) => {
                if methods.iter().any(|method| !method.is_string()) {
                    return Err(PageError::new("generation methods are not strings"));
                }
                methods
            }
            Some(_) => return Err(PageError::new("generation methods are not an array")),
        };
        if !methods.is_empty()
            && !methods
                .iter()
                .any(|method| method.as_str() == Some("generateContent"))
        {
            continue;
        }
        let Some(name) = row.get("name").and_then(Value::as_str) else {
            continue;
        };
        let id = name.strip_prefix("models/").unwrap_or(name);
        if id.is_empty() {
            continue;
        }
        let mut row = row.clone();
        row.insert("id".to_owned(), Value::String(id.to_owned()));
        rows.push(Value::Object(row));
    }
    Ok(rows)
}

fn object(page: &[u8]) -> Result<serde_json::Map<String, Value>, PageError> {
    let value: Value =
        serde_json::from_slice(page).map_err(|_| PageError::new("page is not JSON"))?;
    value
        .as_object()
        .cloned()
        .ok_or(PageError::new("page is not a JSON object"))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use serde_json::json;

    use super::{
        BODY_CAP, ERROR_RUNES, MAX_PAGES, PageError, after_id, cursor_pages, gemini_models,
        json_error_message, next_cursor, string_items,
    };

    #[test]
    fn the_first_named_cursor_wins_and_items_keep_ids() {
        let page = br#"{"data":[{"id":"gpt"},"plain",{"id":""},{"name":"skip"}],"nextPageToken":"","cursor":"page-2"}"#;
        assert_eq!(
            next_cursor(page).expect("cursor").as_deref(),
            Some("page-2")
        );
        assert_eq!(
            string_items(page, "data").expect("items"),
            vec!["gpt".to_owned(), "plain".to_owned()]
        );
        assert_eq!(
            string_items(page, "missing").expect("missing"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_bad_page_does_not_echo_the_body() {
        let error = next_cursor(br"not-json SECRET-PAGE").expect_err("json");
        assert_eq!(error.to_string(), "page is not JSON");
        assert!(!error.to_string().contains("SECRET-PAGE"));
        let error = string_items(br#"{"models":"SECRET-SHAPE"}"#, "models").expect_err("shape");
        assert_eq!(error.to_string(), "page items are not an array");
        assert!(!error.to_string().contains("SECRET-SHAPE"));
    }

    #[tokio::test]
    async fn after_id_replaces_only_its_query_keys() {
        let requests = Cell::new(0);
        let rows = after_id::<_, _, PageError>(
            "https://example.test/models?keep=yes&after_id=old&limit=2",
            |at| {
                requests.set(requests.get() + 1);
                let first = requests.get() == 1;
                async move {
                    if first {
                        assert_eq!(
                            at,
                            "https://example.test/models?keep=yes&after_id=old&limit=2"
                        );
                        Ok(serde_json::to_vec(&json!({
                            "data": [{"id": "one", "context_length": 100}],
                            "has_more": true,
                            "last_id": "one"
                        }))
                        .expect("page"))
                    } else {
                        let url = url::Url::parse(&at).expect("URL");
                        let query = url.query_pairs().collect::<Vec<_>>();
                        assert_eq!(
                            query,
                            vec![
                                ("keep".into(), "yes".into()),
                                ("after_id".into(), "one".into()),
                                ("limit".into(), "1000".into())
                            ]
                        );
                        Ok(serde_json::to_vec(&json!({"data": [{"id": "two"}]})).expect("page"))
                    }
                }
            },
        )
        .await
        .expect("rows");
        assert_eq!(requests.get(), 2);
        assert_eq!(rows[0]["context_length"], 100);
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn after_id_rejects_a_twentieth_page_that_still_has_more() {
        let requests = Cell::new(0);
        let error = after_id::<_, _, PageError>("https://example.test/models", |_| {
            requests.set(requests.get() + 1);
            let page = requests.get();
            async move {
                Ok(serde_json::to_vec(&json!({
                    "data": [{"id": page}],
                    "has_more": true,
                    "last_id": page.to_string()
                }))
                .expect("page"))
            }
        })
        .await
        .expect_err("partial list");
        assert_eq!(requests.get(), MAX_PAGES);
        assert_eq!(
            error.to_string(),
            "model list is still paging after 20 pages"
        );
    }

    #[tokio::test]
    async fn after_id_rejects_missing_empty_and_repeated_last_ids() {
        for page in [
            json!({"data": [{"id": "one"}], "has_more": true}),
            json!({"data": [{"id": "one"}], "has_more": true, "last_id": ""}),
        ] {
            let error = after_id::<_, _, PageError>("https://example.test/models", |_| {
                let page = page.clone();
                async move { Ok(serde_json::to_vec(&page).expect("page")) }
            })
            .await
            .expect_err("missing cursor");
            assert_eq!(error.to_string(), "paged model list has no last_id");
        }
        let requests = Cell::new(0);
        let error = after_id::<_, _, PageError>("https://example.test/models", |_| {
            requests.set(requests.get() + 1);
            async move {
                Ok(serde_json::to_vec(&json!({
                    "data": [{"id": "one"}],
                    "has_more": true,
                    "last_id": "same"
                }))
                .expect("page"))
            }
        })
        .await
        .expect_err("repeated cursor");
        assert_eq!(requests.get(), 2);
        assert_eq!(error.to_string(), "model list cursor repeated");
    }

    #[tokio::test]
    async fn after_id_rejects_an_empty_successful_list() {
        let error = after_id::<_, _, PageError>("https://example.test/models", |_| async {
            Ok(br#"{"data": [], "models": []}"#.to_vec())
        })
        .await
        .expect_err("empty list");
        assert_eq!(error.to_string(), "no models listed");
    }

    #[tokio::test]
    async fn after_id_uses_nonempty_data_and_keeps_name_and_string_rows() {
        let rows = after_id::<_, _, PageError>("https://example.test/models", |_| async {
            Ok(serde_json::to_vec(&json!({
                "data": [{"name": "named", "metadata": true}, "bare"],
                "models": [{"id": "fallback"}]
            }))
            .expect("page"))
        })
        .await
        .expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "named");
        assert_eq!(rows[0]["metadata"], true);
        assert_eq!(rows[1], "bare");

        let rows = after_id::<_, _, PageError>("https://example.test/models", |_| async {
            Ok(serde_json::to_vec(&json!({
                "data": [],
                "models": [{"id": "fallback"}]
            }))
            .expect("page"))
        })
        .await
        .expect("fallback rows");
        assert_eq!(rows[0]["id"], "fallback");
    }

    #[tokio::test(start_paused = true)]
    async fn after_id_has_one_budget_for_all_pages() {
        let started = tokio::time::Instant::now();
        let requests = Cell::new(0);
        let error = after_id::<_, _, PageError>("https://example.test/models", |_| {
            requests.set(requests.get() + 1);
            let first = requests.get() == 1;
            async move {
                if first {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    Ok(serde_json::to_vec(&json!({
                        "data": [{"id": "one"}], "has_more": true, "last_id": "one"
                    }))
                    .expect("page"))
                } else {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        })
        .await
        .expect_err("deadline");
        assert_eq!(started.elapsed(), Duration::from_secs(8));
        assert_eq!(requests.get(), 2);
        assert_eq!(error.to_string(), "model list exceeded its time budget");
    }

    #[tokio::test]
    async fn walkers_reject_oversized_bodies() {
        let body = vec![b' '; BODY_CAP + 1];
        let error = after_id::<_, _, PageError>("https://example.test/models", |_| {
            let body = body.clone();
            async move { Ok(body) }
        })
        .await
        .expect_err("body cap");
        assert_eq!(error.to_string(), "model list body is too large");
    }

    #[tokio::test]
    async fn gemini_returns_partial_rows_at_twenty_pages() {
        let requests = Cell::new(0);
        let rows = gemini_models::<_, _, PageError>("https://example.test/v1beta", |at| {
            requests.set(requests.get() + 1);
            let page = requests.get();
            async move {
                let url = url::Url::parse(&at).expect("URL");
                assert_eq!(
                    url.query_pairs()
                        .find(|(key, _)| key == "pageSize")
                        .map(|(_, value)| value.into_owned()),
                    Some("1000".to_owned())
                );
                if page > 1 {
                    assert!(at.contains("pageToken="));
                }
                Ok(serde_json::to_vec(&json!({
                    "models": [{"name": format!("models/model-{page}")}],
                    "nextPageToken": format!("next-{page}")
                }))
                .expect("page"))
            }
        })
        .await
        .expect("partial result");
        assert_eq!(requests.get(), MAX_PAGES);
        assert_eq!(rows.len(), MAX_PAGES);
        assert_eq!(rows[19]["id"], "model-20");
    }

    #[tokio::test]
    async fn gemini_filters_methods_and_strips_the_model_prefix() {
        let rows = gemini_models::<_, _, PageError>("https://example.test/v1beta/", |_| async {
            Ok(serde_json::to_vec(&json!({
                "models": [
                    {
                        "name": "models/chat",
                        "displayName": "Chat",
                        "supportedGenerationMethods": ["embedContent", "generateContent"],
                        "inputTokenLimit": 100,
                        "outputTokenLimit": 20
                    },
                    {"name": "models/empty", "supportedGenerationMethods": []},
                    {"name": "models/absent"},
                    {"name": "models/embed", "supportedGenerationMethods": ["embedContent"]},
                    {"name": "models/image", "supportedGenerationMethods": ["predict"]},
                    {"name": "models/video", "supportedGenerationMethods": ["predictLongRunning"]}
                ]
            }))
            .expect("page"))
        })
        .await
        .expect("rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["id"], "chat");
        assert_eq!(rows[0]["displayName"], "Chat");
        assert_eq!(rows[0]["inputTokenLimit"], 100);
        assert_eq!(rows[0]["outputTokenLimit"], 20);
        assert_eq!(rows[1]["id"], "empty");
        assert_eq!(rows[2]["id"], "absent");
    }

    #[tokio::test]
    async fn gemini_rejects_an_empty_list() {
        let error = gemini_models::<_, _, PageError>("https://example.test", |_| async {
            Ok(br#"{"models": []}"#.to_vec())
        })
        .await
        .expect_err("empty list");
        assert_eq!(error.to_string(), "no models listed");
    }

    #[tokio::test(start_paused = true)]
    async fn gemini_gives_each_page_its_own_budget() {
        let started = tokio::time::Instant::now();
        let requests = Cell::new(0);
        let rows = gemini_models::<_, _, PageError>("https://example.test", |_| {
            requests.set(requests.get() + 1);
            let next = if requests.get() == 1 {
                Some("two")
            } else {
                None
            };
            async move {
                tokio::time::sleep(Duration::from_secs(7)).await;
                Ok(serde_json::to_vec(&json!({
                    "models": [{"name": "models/chat"}],
                    "nextPageToken": next
                }))
                .expect("page"))
            }
        })
        .await
        .expect("two pages");
        assert_eq!(rows.len(), 2);
        assert_eq!(started.elapsed(), Duration::from_secs(14));
    }

    #[test]
    fn json_errors_are_cut_at_160_multibyte_runes() {
        let message = "\u{e9}".repeat(ERROR_RUNES + 1);
        let body = serde_json::to_vec(&json!({"error": {"message": message}})).expect("body");
        let cut = json_error_message(&body);
        assert_eq!(cut.chars().count(), ERROR_RUNES + 1);
        assert_eq!(
            cut.chars()
                .filter(|character| *character == '\u{e9}')
                .count(),
            ERROR_RUNES
        );
        assert!(cut.ends_with('…'));
        assert_eq!(json_error_message(br#"{"message":"  short  "}"#), "short");
        assert_eq!(json_error_message(br#"{"error":"nested"}"#), "nested");
    }

    #[tokio::test]
    async fn generic_cursor_pages_reject_a_repeated_cursor() {
        let requests = Cell::new(0);
        let error = cursor_pages::<_, _, _, PageError>(
            "https://example.test/models",
            MAX_PAGES,
            |base, cursor| format!("{base}?cursor={cursor}"),
            |_| {
                requests.set(requests.get() + 1);
                let next = if requests.get() == 2 { "b" } else { "a" };
                async move { Ok(serde_json::to_vec(&json!({"next": next})).expect("page")) }
            },
        )
        .await
        .expect_err("cycle");
        assert_eq!(requests.get(), 3);
        assert_eq!(error.to_string(), "model list cursor repeated");
    }

    #[tokio::test]
    async fn generic_cursor_pages_enforce_the_page_limit() {
        let requests = Cell::new(0);
        let error = cursor_pages::<_, _, _, PageError>(
            "https://example.test/models",
            2,
            |base, cursor| format!("{base}?cursor={cursor}"),
            |_| {
                requests.set(requests.get() + 1);
                let next = requests.get().to_string();
                async move { Ok(serde_json::to_vec(&json!({"next": next})).expect("page")) }
            },
        )
        .await
        .expect_err("page limit");
        assert_eq!(requests.get(), 2);
        assert_eq!(error.to_string(), "model list reached its page limit");
    }
}
