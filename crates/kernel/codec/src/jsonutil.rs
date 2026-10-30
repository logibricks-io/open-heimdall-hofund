//! Small JSON object helpers shared by the codecs.
//!
//! Every helper returns [`CodecError`] for a missing field or a wrong type.
//! None of them panic.

use hofund_ir::Extensions;
use serde_json::{Map, Value};

use crate::error::CodecError;

pub(crate) type Object = Map<String, Value>;

pub(crate) fn parse_object(bytes: &[u8]) -> Result<Object, CodecError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(CodecError::Empty);
    }
    match serde_json::from_slice(bytes)? {
        Value::Object(object) => Ok(object),
        _ => Err(CodecError::ExpectedObject),
    }
}

pub(crate) fn to_bytes(value: &Value) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(value)?)
}

pub(crate) fn take_string(
    object: &mut Object,
    key: &'static str,
) -> Result<Option<String>, CodecError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(CodecError::WrongType { field: key }),
    }
}

pub(crate) fn require_string(object: &mut Object, key: &'static str) -> Result<String, CodecError> {
    take_string(object, key)?.ok_or(CodecError::MissingField(key))
}

pub(crate) fn take_bool(
    object: &mut Object,
    key: &'static str,
) -> Result<Option<bool>, CodecError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(value)),
        Some(_) => Err(CodecError::WrongType { field: key }),
    }
}

pub(crate) fn take_u64(object: &mut Object, key: &'static str) -> Result<Option<u64>, CodecError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or(CodecError::WrongType { field: key }),
        Some(_) => Err(CodecError::WrongType { field: key }),
    }
}

pub(crate) fn take_i64(object: &mut Object, key: &'static str) -> Result<Option<i64>, CodecError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or(CodecError::WrongType { field: key }),
        Some(_) => Err(CodecError::WrongType { field: key }),
    }
}

pub(crate) fn take_f64(object: &mut Object, key: &'static str) -> Result<Option<f64>, CodecError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .map(Some)
            .ok_or(CodecError::WrongType { field: key }),
        Some(_) => Err(CodecError::WrongType { field: key }),
    }
}

pub(crate) fn take_object(
    object: &mut Object,
    key: &'static str,
) -> Result<Option<Object>, CodecError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(value)) => Ok(Some(value)),
        Some(_) => Err(CodecError::WrongType { field: key }),
    }
}

pub(crate) fn expect_object(value: Value, field: &'static str) -> Result<Object, CodecError> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(CodecError::WrongType { field }),
    }
}

pub(crate) fn expect_array(value: Value, field: &'static str) -> Result<Vec<Value>, CodecError> {
    match value {
        Value::Array(items) => Ok(items),
        _ => Err(CodecError::WrongType { field }),
    }
}

/// A string becomes a one-element list. An array must contain only strings.
pub(crate) fn take_string_list(
    object: &mut Object,
    key: &'static str,
) -> Result<Vec<String>, CodecError> {
    match object.remove(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![text]),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                Value::String(text) => Ok(text),
                _ => Err(CodecError::WrongType { field: key }),
            })
            .collect(),
        Some(_) => Err(CodecError::WrongType { field: key }),
    }
}

pub(crate) fn insert_some<T>(object: &mut Object, key: &str, value: Option<T>)
where
    T: Into<Value>,
{
    if let Some(value) = value {
        object.insert(key.to_owned(), value.into());
    }
}

pub(crate) fn insert_u64(object: &mut Object, key: &str, value: Option<u64>) {
    insert_some(object, key, value);
}

pub(crate) fn insert_i64(object: &mut Object, key: &str, value: Option<i64>) {
    insert_some(object, key, value);
}

pub(crate) fn insert_f64(
    object: &mut Object,
    key: &str,
    value: Option<f64>,
) -> Result<(), CodecError> {
    let Some(value) = value else {
        return Ok(());
    };
    let number = serde_json::Number::from_f64(value).ok_or_else(|| CodecError::Unsupported {
        message: format!("field `{key}` is not a finite number"),
    })?;
    object.insert(key.to_owned(), Value::Number(number));
    Ok(())
}

pub(crate) fn require_index(object: &mut Object, key: &'static str) -> Result<u32, CodecError> {
    let value = take_u64(object, key)?.ok_or(CodecError::MissingField(key))?;
    u32::try_from(value).map_err(|_| CodecError::Unsupported {
        message: format!("field `{key}` does not fit in u32"),
    })
}

/// `primary` wins when both objects contain the same key.
pub(crate) fn combine_extensions(primary: Object, secondary: Object) -> Extensions {
    let mut extensions = Extensions::from_map(primary);
    extensions.absorb(&Extensions::from_map(secondary));
    extensions
}
