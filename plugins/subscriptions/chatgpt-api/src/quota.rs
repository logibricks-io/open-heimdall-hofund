//! Usage is not exposed through the ChatGPT API.

use hofund_plugin_api::{PluginError, QuotaWindow};

use crate::errors::USAGE_URL;

pub(crate) fn windows() -> Result<Vec<QuotaWindow>, PluginError> {
    Ok(vec![
        QuotaWindow::new("plan", 0, None, None)?
            .with_blocking(false)
            .with_display(format!(
                "a ChatGPT API account's usage isn't told through OpenAI's API, only at {USAGE_URL}"
            )),
    ])
}
