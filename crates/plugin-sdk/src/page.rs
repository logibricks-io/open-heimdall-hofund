//! Cursors and string ids from one JSON page.
//!
//! `list_models` and the connectivity probe use this. The helper does not
//! send a request. Error text is a fixed sentence and does not include the body.
//! A paging loop with a page limit and repeated-cursor guard is deferred to
//! PR E2 (model listing).

use std::fmt;

use serde_json::Value;

const CURSOR_KEYS: &[&str] = &["next_page_token", "nextPageToken", "cursor", "next"];

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
    use super::{next_cursor, string_items};

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
}
