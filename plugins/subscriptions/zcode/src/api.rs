//! Z.ai JSON envelope HTTP helpers.

use std::path::Path;

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;

use crate::device::device_mid;
use crate::http_auth::{auth_for_http_status, key_refused_code, parse_envelope_code};
use crate::{APP_VERSION, Endpoints};

pub(crate) struct EnvelopeRequest<'a> {
    pub host: &'a HostContext,
    pub endpoints: &'a Endpoints,
    pub method: &'a str,
    pub url: &'a str,
    pub auth: Option<&'a SecretString>,
    pub extra_headers: &'a [(&'a str, &'a str)],
    pub body: Option<&'a Value>,
    pub device_file: &'a Path,
}

impl<'a> EnvelopeRequest<'a> {
    pub fn get(
        host: &'a HostContext,
        endpoints: &'a Endpoints,
        url: &'a str,
        auth: Option<&'a SecretString>,
        extra_headers: &'a [(&'a str, &'a str)],
        device_file: &'a Path,
    ) -> Self {
        Self {
            host,
            endpoints,
            method: "GET",
            url,
            auth,
            extra_headers,
            body: None,
            device_file,
        }
    }
}

pub(crate) async fn get_envelope(req: &EnvelopeRequest<'_>) -> Result<Value, PluginError> {
    call(req).await
}

pub(crate) async fn call(req: &EnvelopeRequest<'_>) -> Result<Value, PluginError> {
    let EnvelopeRequest {
        host,
        endpoints,
        method,
        url,
        auth,
        extra_headers,
        body,
        device_file,
    } = req;
    let payload = body
        .map(json::to_vec)
        .transpose()
        .map_err(|_| PluginError::decode("Z Code request body could not be encoded"))?;
    let mut call = match *method {
        "GET" => Call::get(*url),
        "POST" => Call::post(*url),
        other => {
            return Err(PluginError::unsupported(format!(
                "Z Code HTTP method `{other}` is not supported"
            )));
        }
    };
    call = call
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("user-agent", format!("ZCode/{APP_VERSION}"));
    if url.starts_with(&format!("{}/", endpoints.portal.trim_end_matches('/')))
        && !extra_headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-device-mid"))
    {
        call = call.header("x-device-mid", device_mid(device_file)?);
    }
    if let Some(token) = auth {
        call = call.sensitive_header("authorization", secret_text(token));
    }
    for (name, value) in *extra_headers {
        call = call.header(*name, *value);
    }
    if let Some(bytes) = payload {
        call = call.bytes(bytes);
    }
    let reply = call.fetch(host.http()).await?;
    let status = reply.status();
    let raw = reply.body();
    if !(200..300).contains(&status) {
        if let Ok(Value::Object(env)) = json::from_slice(raw)
            && let Some(msg) = env
                .get("msg")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
        {
            if status == 401 || status == 403 {
                return Err(PluginError::auth(msg.to_owned()));
            }
            return Err(PluginError::status_error(status, msg.to_owned()));
        }
        return Err(auth_for_http_status(status, "Z Code request failed"));
    }
    let Value::Object(env) =
        json::from_slice(raw).map_err(|_| PluginError::decode("Z Code reply is not JSON"))?
    else {
        return Err(PluginError::decode("Z Code reply is not a JSON object"));
    };
    let code = env.get("code").map(code_string).unwrap_or_default();
    if !code.is_empty() && code != "0" && code != "200" {
        let msg = env
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("Z Code returned an error");
        if key_refused_code(parse_envelope_code(env.get("code")).unwrap_or(0)) {
            return Err(PluginError::auth(msg));
        }
        return Err(PluginError::decode(msg));
    }
    match env.get("data") {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(value) => Ok(value.clone()),
    }
}

fn code_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().trim_matches('"').to_owned(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}
