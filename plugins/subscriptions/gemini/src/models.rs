//! Gemini CLI's experiment-driven model catalog and Code Assist id mapping.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, ModelSpec, PluginError, SecretString};
use hofund_plugin_sdk::json::{Value, json};

use crate::{Endpoints, GEMINI_CLI_VERSION, project};

const FLAG_GEMINI_31_PRO: u64 = 45_760_185;
const FLAG_PRO_NO_ACCESS: u64 = 45_768_879;
const FLAG_LATEST_FLASH: u64 = 45_842_815;
const FLAG_LATEST_FLASH_LITE: u64 = 45_827_489;
const EXPERIMENTS_FOR: Duration = Duration::from_secs(60 * 60);

/// The experiment flags that decide which models Gemini CLI offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    gemini_31: bool,
    no_pro: bool,
    latest_flash: bool,
    latest_flash_lite: bool,
}

/// Per-instance experiment readings, shared by list, quota, and upstream calls.
#[derive(Default)]
pub(crate) struct Experiments {
    readings: Mutex<HashMap<(AccountId, String), Reading>>,
}

#[derive(Clone, Copy)]
struct Reading {
    flags: Flags,
    at: SystemTime,
}

impl Experiments {
    pub(crate) fn forget(&self, account: &AccountId) {
        self.readings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(id, _), _| id != account);
    }

    pub(crate) async fn read(
        &self,
        host: &HostContext,
        endpoints: &Endpoints,
        account: &AccountId,
        project_id: &str,
        access: &SecretString,
    ) -> Flags {
        let key = (account.clone(), project_id.to_owned());
        let now = host.clock().now();
        if let Some(reading) = self
            .readings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .copied()
            && now.duration_since(reading.at).unwrap_or_default() < EXPERIMENTS_FOR
        {
            return reading.flags;
        }

        let body = json!({
            "project": project_id,
            "metadata": {
                "ideName": "IDE_UNSPECIFIED",
                "pluginType": "GEMINI",
                "ideVersion": GEMINI_CLI_VERSION,
                "platform": platform(),
                "updateChannel": "stable",
                "duetProject": project_id,
            }
        });
        let flags = project::call(host, endpoints, access, "listExperiments", &body)
            .await
            .map_or_else(|_| Flags::default(), |reply| parse_flags(&reply));
        self.readings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, Reading { flags, at: now });
        flags
    }
}

impl Flags {
    /// The id Gemini CLI sends for a displayed model id.
    pub(crate) fn wire(self, model: &str) -> String {
        match model {
            "gemini-3.5-flash" | "gemini-3-flash" if self.latest_flash => {
                "gemini-3.8-flash".to_owned()
            }
            "gemini-3.5-flash" | "gemini-3.8-flash" if !self.latest_flash => {
                "gemini-3-flash".to_owned()
            }
            "gemini-3.1-flash-lite" if self.latest_flash_lite => "gemini-3.5-flash-lite".to_owned(),
            "gemini-3.5-flash-lite" if !self.latest_flash_lite => {
                "gemini-3.1-flash-lite".to_owned()
            }
            _ => model.to_owned(),
        }
    }

    /// The displayed model a Code Assist quota bucket belongs to.
    pub(crate) fn shown(self, model: &str) -> String {
        match model {
            "gemini-3-flash" | "gemini-3.5-flash" | "gemini-3.8-flash" => {
                if self.latest_flash {
                    "gemini-3.8-flash".to_owned()
                } else {
                    "gemini-3.5-flash".to_owned()
                }
            }
            "gemini-3.1-flash-lite" | "gemini-3.5-flash-lite" => {
                if self.latest_flash_lite {
                    "gemini-3.5-flash-lite".to_owned()
                } else {
                    "gemini-3.1-flash-lite".to_owned()
                }
            }
            _ => model.to_owned(),
        }
    }

    /// Gemini CLI 0.61's model picker for an account with these flags.
    pub(crate) fn offered(self, preview: bool) -> Result<Vec<ModelSpec>, PluginError> {
        CATALOG
            .iter()
            .filter(|(id, _)| match *id {
                "gemini-3.1-pro-preview" => self.gemini_31 && preview && !self.no_pro,
                "gemini-3-pro-preview" => !self.gemini_31 && preview && !self.no_pro,
                "gemini-3-flash-preview" => preview,
                "gemini-3.8-flash" => self.latest_flash,
                "gemini-3.5-flash" => !self.latest_flash,
                "gemini-3.5-flash-lite" => self.latest_flash_lite,
                "gemini-3.1-flash-lite" => !self.latest_flash_lite,
                "gemini-2.5-pro" => !self.no_pro,
                "gemini-2.5-flash" => !self.latest_flash,
                _ => true,
            })
            .map(|(id, name)| {
                ModelSpec::new(*id, Some((*name).to_owned()))
                    .map(|model| model.with_image_input(Some(true)))
            })
            .collect()
    }
}

const CATALOG: &[(&str, &str)] = &[
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview"),
    ("gemini-3-pro-preview", "Gemini 3 Pro Preview"),
    ("gemini-3-flash-preview", "Gemini 3 Flash Preview"),
    ("gemini-3.8-flash", "Gemini 3.8 Flash"),
    ("gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("gemini-3.5-flash-lite", "Gemini 3.5 Flash Lite"),
    ("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite"),
    ("gemini-2.5-pro", "Gemini 2.5 Pro"),
    ("gemini-2.5-flash", "Gemini 2.5 Flash"),
    ("gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite"),
];

fn parse_flags(reply: &Value) -> Flags {
    let mut flags = Flags::default();
    for flag in reply
        .get("flags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = flag.get("flagId").and_then(Value::as_u64);
        let enabled = flag
            .get("boolValue")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match id {
            Some(FLAG_GEMINI_31_PRO) => flags.gemini_31 = enabled,
            Some(FLAG_PRO_NO_ACCESS) => flags.no_pro = enabled,
            Some(FLAG_LATEST_FLASH) => flags.latest_flash = enabled,
            Some(FLAG_LATEST_FLASH_LITE) => flags.latest_flash_lite = enabled,
            _ => {}
        }
    }
    flags
}

fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "x86_64") => "DARWIN_AMD64",
        ("macos", "aarch64") => "DARWIN_ARM64",
        ("linux", "x86_64") => "LINUX_AMD64",
        ("linux", "aarch64") => "LINUX_ARM64",
        ("windows", "x86_64") => "WINDOWS_AMD64",
        _ => "PLATFORM_UNSPECIFIED",
    }
}

#[cfg(test)]
mod tests {
    use super::{Flags, parse_flags};
    use hofund_plugin_sdk::json::json;

    fn ids(flags: Flags, preview: bool) -> Vec<String> {
        flags
            .offered(preview)
            .expect("catalog")
            .into_iter()
            .map(|model| model.id().to_owned())
            .collect()
    }

    #[test]
    fn r49_catalog_cases_match_gemini_cli() {
        let quota_preview = true;
        assert_eq!(
            ids(Flags::default(), quota_preview),
            [
                "gemini-3-pro-preview",
                "gemini-3-flash-preview",
                "gemini-3.5-flash",
                "gemini-3.1-flash-lite",
                "gemini-2.5-pro",
                "gemini-2.5-flash",
                "gemini-2.5-flash-lite",
            ]
        );
        let latest = parse_flags(&json!({"flags": [
            {"flagId": 45842815, "boolValue": true},
            {"flagId": 45827489, "boolValue": true},
            {"flagId": 45760185, "boolValue": true}
        ]}));
        assert_eq!(
            ids(latest, quota_preview),
            [
                "gemini-3.1-pro-preview",
                "gemini-3-flash-preview",
                "gemini-3.8-flash",
                "gemini-3.5-flash-lite",
                "gemini-2.5-pro",
                "gemini-2.5-flash-lite",
            ]
        );
        let no_pro = parse_flags(&json!({"flags": [
            {"flagId": 45768879, "boolValue": true}
        ]}));
        assert_eq!(
            ids(no_pro, false),
            [
                "gemini-3.5-flash",
                "gemini-3.1-flash-lite",
                "gemini-2.5-flash",
                "gemini-2.5-flash-lite",
            ]
        );
    }

    #[test]
    fn r49_wire_and_quota_names_follow_experiments() {
        let plain = Flags::default();
        for (shown, wire) in [
            ("gemini-3.5-flash", "gemini-3-flash"),
            ("gemini-3.8-flash", "gemini-3-flash"),
            ("gemini-3.1-flash-lite", "gemini-3.1-flash-lite"),
            ("gemini-3.5-flash-lite", "gemini-3.1-flash-lite"),
            ("gemini-2.5-pro", "gemini-2.5-pro"),
        ] {
            assert_eq!(plain.wire(shown), wire);
        }
        assert_eq!(plain.shown("gemini-3-flash"), "gemini-3.5-flash");
        assert_eq!(
            plain.shown("gemini-3.5-flash-lite"),
            "gemini-3.1-flash-lite"
        );

        let latest = parse_flags(&json!({"flags": [
            {"flagId": 45842815, "boolValue": true},
            {"flagId": 45827489, "boolValue": true}
        ]}));
        for shown in ["gemini-3.5-flash", "gemini-3-flash", "gemini-3.8-flash"] {
            assert_eq!(latest.wire(shown), "gemini-3.8-flash");
            assert_eq!(latest.shown(shown), "gemini-3.8-flash");
        }
        assert_eq!(
            latest.wire("gemini-3.1-flash-lite"),
            "gemini-3.5-flash-lite"
        );
        assert_eq!(
            latest.shown("gemini-3.1-flash-lite"),
            "gemini-3.5-flash-lite"
        );
    }

    #[test]
    fn r49_unknown_and_false_flags_do_not_enable_models() {
        let flags = parse_flags(&json!({"flags": [
            {"flagId": 45842815, "boolValue": false},
            {"flagId": 42, "boolValue": true},
            {"flagId": 45827489}
        ]}));
        assert_eq!(flags, Flags::default());
        assert_eq!(flags.shown("other"), "other");
        assert_eq!(flags.wire("models/other"), "models/other");
    }
}
