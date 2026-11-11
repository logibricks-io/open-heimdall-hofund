//! Read-only discovery of the Copilot editors' and CLI's own sign-ins.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_plugin_api::{
    AccountId, BodyStream, DiscoveredAgentAccount, HostContext, PluginError, SecretString,
    SpawnRequest,
};
use hofund_plugin_sdk::json::{self, Map, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::site::{GITHUB, Site};

const KEYCHAIN_WAIT: Duration = Duration::from_secs(5);

/// The kind of external sign-in an account references.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Editor,
    Cli,
}

impl SourceKind {
    fn name(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Cli => "cli",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "editor" => Some(Self::Editor),
            "cli" => Some(Self::Cli),
            _ => None,
        }
    }
}

/// Where an external sign-in is read. No token is stored in this reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) kind: SourceKind,
    pub(crate) path: PathBuf,
    pub(crate) host: String,
    pub(crate) login: String,
}

impl Source {
    pub(crate) fn reference(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.kind.name(),
            self.path.display(),
            self.host,
            self.login
        )
    }

    pub(crate) fn id(&self) -> AccountId {
        account_id_from(&[self.host.as_str(), self.login.as_str()])
    }

    pub(crate) fn label(&self) -> String {
        account_name(&self.login, &self.host)
    }

    fn from_blob(blob: &SecretObject) -> Option<Self> {
        Some(Self {
            kind: SourceKind::parse(blob.text("source")?)?,
            path: PathBuf::from(blob.text("source_path")?),
            host: blob.text("host").unwrap_or(GITHUB).to_owned(),
            login: blob.text("login")?.to_owned(),
        })
    }

    pub(crate) fn secret(&self) -> SecretString {
        SecretObject::new()
            .with_number("v", 1)
            .with_text("source", self.kind.name())
            .with_text("source_path", &self.path.to_string_lossy())
            .with_text("host", &self.host)
            .with_text("login", &self.login)
            .to_secret()
    }
}

/// A credential read from the external store.
pub(crate) struct External {
    pub(crate) oauth: SecretString,
    pub(crate) site: Site,
    pub(crate) source: Source,
}

pub(crate) fn account_name(login: &str, host: &str) -> String {
    if host.is_empty() || host == GITHUB {
        login.to_owned()
    } else {
        format!("{login}@{host}")
    }
}

/// Lists the external accounts under `home`. Editor reads use the process's
/// XDG config directory, falling back to `.config` beside the CLI home.
pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    list_at(home, &editor_config_dir(home))
}

fn editor_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.parent().unwrap_or(home).join(".config"))
}

pub(crate) fn list_at(
    home: &Path,
    config: &Path,
) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let mut sources = editor_sources(config);
    sources.extend(cli_sources(home));
    let mut rows = Vec::new();
    for source in sources {
        if rows
            .iter()
            .any(|row: &DiscoveredAgentAccount| row.id() == &source.id())
        {
            continue;
        }
        rows.push(DiscoveredAgentAccount::new(
            source.id(),
            source.label(),
            source.reference(),
        )?);
    }
    Ok(rows)
}

/// Stores only the external reference selected by `reference`.
pub(crate) async fn import(
    home: &Path,
    reference: &str,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    import_at(home, &editor_config_dir(home), reference, host).await
}

pub(crate) async fn import_at(
    home: &Path,
    config: &Path,
    reference: &str,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let rows = list_at(home, config)?;
    if !rows.iter().any(|row| row.reference() == reference) {
        return Err(PluginError::config(
            "unknown Copilot agent sign-in reference",
        ));
    }
    let source = editor_sources(config)
        .into_iter()
        .chain(cli_sources(home))
        .find(|source| source.reference() == reference)
        .ok_or_else(|| {
            PluginError::auth("the Copilot editor or CLI sign-in is no longer present")
        })?;
    let id = source.id();
    let account = host.accounts().account(id.clone(), source.label())?;
    host.accounts().upsert(account, source.secret()).await?;
    Ok(id)
}

/// Reads the current token of a stored external account.
pub(crate) async fn read_external(
    host: &HostContext,
    source: &Source,
) -> Result<External, PluginError> {
    let site = Site::parse(&source.host)
        .map_err(|_| PluginError::auth("the stored Copilot sign-in names an unknown host"))?;
    let oauth = match source.kind {
        SourceKind::Editor => read_editor(source)?,
        SourceKind::Cli => read_cli(host, source).await?,
    };
    Ok(External {
        oauth,
        site,
        source: source.clone(),
    })
}

pub(crate) fn source_from_blob(blob: &SecretObject) -> Option<Source> {
    Source::from_blob(blob)
}

fn read_editor(source: &Source) -> Result<SecretString, PluginError> {
    let value = read_json(&source.path, false)?;
    let object = value.as_object().ok_or_else(invalid_editor)?;
    let mut matches = object.iter().filter(|(key, _)| {
        key.split_once(':').map_or(key.as_str(), |(host, _)| host) == source.host
    });
    let Some((_, account)) = matches.find(|(_, account)| {
        account
            .get("user")
            .and_then(Value::as_str)
            .is_some_and(|login| login.eq_ignore_ascii_case(&source.login))
    }) else {
        return Err(invalid_editor());
    };
    token(account.get("oauth_token")).ok_or_else(invalid_editor)
}

fn invalid_editor() -> PluginError {
    PluginError::auth(
        "the Copilot editor sign-in is missing or changed; import it again or sign in to Copilot",
    )
}

fn read_editor_file(path: &Path) -> Option<Map<String, Value>> {
    read_json(path, false).ok()?.as_object().cloned()
}

fn editor_sources(config: &Path) -> Vec<Source> {
    let mut out = Vec::new();
    for name in ["apps.json", "hosts.json"] {
        let path = config.join("github-copilot").join(name);
        let Some(apps) = read_editor_file(&path) else {
            continue;
        };
        let mut keys: Vec<_> = apps.keys().collect();
        keys.sort();
        for key in keys {
            let host = key.split_once(':').map_or(key.as_str(), |(host, _)| host);
            let Ok(site) = Site::parse(host) else {
                continue;
            };
            let Some(account) = apps.get(key) else {
                continue;
            };
            if token(account.get("oauth_token")).is_none() {
                continue;
            }
            let login = account
                .get("user")
                .and_then(Value::as_str)
                .filter(|login| !login.is_empty())
                .unwrap_or("GitHub");
            let source = Source {
                kind: SourceKind::Editor,
                path: path.clone(),
                host: site.name().to_owned(),
                login: login.to_owned(),
            };
            if !out
                .iter()
                .any(|seen: &Source| seen.kind == SourceKind::Editor && seen.host == source.host)
            {
                out.push(source);
            }
        }
    }
    out.sort_by_key(|source| source.host != GITHUB);
    out
}

async fn read_cli(host: &HostContext, source: &Source) -> Result<SecretString, PluginError> {
    let value = read_json(&source.path, true)?;
    let key = cli_key(&source.host, &source.login);
    if let Some(token) = value
        .get("copilotTokens")
        .and_then(|tokens| tokens.get(&key))
        .and_then(|value| token(Some(value)))
    {
        return Ok(token);
    }
    keychain_token(host, &source.path, &key)
        .await
        .ok_or_else(|| {
            PluginError::auth(
                "the Copilot CLI sign-in has no readable token; run `copilot login` again",
            )
        })
}

/// Reads the CLI's sign-in when it belongs to the same account as `editor`.
pub(crate) async fn stand_in(host: &HostContext, editor: &Source) -> Option<External> {
    let home = std::env::var_os("COPILOT_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".copilot"))
        })?;
    let source = cli_sources(&home).into_iter().find(|source| {
        source.host == editor.host && source.login.eq_ignore_ascii_case(&editor.login)
    })?;
    read_external(host, &source).await.ok()
}

fn cli_sources(home: &Path) -> Vec<Source> {
    let path = home.join("config.json");
    let Ok(value) = read_json(&path, true) else {
        return Vec::new();
    };
    let mut users = Vec::new();
    if let Some(last) = value.get("lastLoggedInUser") {
        users.push(last);
    }
    users.extend(
        value
            .get("loggedInUsers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten(),
    );
    for enterprise in [false, true] {
        for user in &users {
            let Some(login) = user
                .get("login")
                .and_then(Value::as_str)
                .filter(|login| !login.is_empty())
            else {
                continue;
            };
            let host_name = user.get("host").and_then(Value::as_str).unwrap_or("");
            let Ok(site) = Site::parse(host_name) else {
                continue;
            };
            if site.is_enterprise() != enterprise {
                continue;
            }
            let key = cli_key(site.name(), login);
            let plaintext = value
                .get("copilotTokens")
                .and_then(|tokens| tokens.get(&key))
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty());
            let has_keychain = plaintext || cfg!(any(target_os = "linux", target_os = "macos"));
            if has_keychain {
                return vec![Source {
                    kind: SourceKind::Cli,
                    path,
                    host: site.name().to_owned(),
                    login: login.to_owned(),
                }];
            }
        }
    }
    Vec::new()
}

fn cli_key(host: &str, login: &str) -> String {
    if host == GITHUB {
        format!("https://github.com:{login}")
    } else {
        format!("https://{host}:{login}")
    }
}

async fn keychain_token(host: &HostContext, home: &Path, account: &str) -> Option<SecretString> {
    let (program, args) = if cfg!(target_os = "macos") {
        (
            "/usr/bin/security",
            vec![
                "find-generic-password".to_owned(),
                "-s".to_owned(),
                "copilot-cli".to_owned(),
                "-a".to_owned(),
                account.to_owned(),
                "-w".to_owned(),
            ],
        )
    } else if cfg!(target_os = "linux") {
        ("/usr/bin/secret-tool", Vec::new())
    } else {
        return None;
    };
    let cwd = home.parent()?.to_path_buf();
    if cfg!(target_os = "linux") {
        for attribute in ["account", "username"] {
            let args = vec![
                "lookup".to_owned(),
                "service".to_owned(),
                "copilot-cli".to_owned(),
                attribute.to_owned(),
                account.to_owned(),
            ];
            if let Some(token) = run_secret_tool(host, program, args, &cwd).await {
                return Some(token);
            }
        }
        None
    } else {
        run_secret_tool(host, program, args, &cwd).await
    }
}

async fn run_secret_tool(
    host: &HostContext,
    program: &str,
    args: Vec<String>,
    cwd: &Path,
) -> Option<SecretString> {
    let request = SpawnRequest::new(
        PathBuf::from(program),
        args.into_iter().map(OsString::from).collect(),
        Vec::new(),
        cwd,
    )
    .ok()?;
    let read = async {
        let child = host.spawn_process().spawn_piped(&request).await.ok()?;
        let mut stdout = child.stdout().ok()?;
        let output = collect(&mut stdout, 4096).await?;
        let token = output.trim();
        (!token.is_empty()).then(|| SecretString::from(token.to_owned()))
    };
    hofund_plugin_sdk::timeout(KEYCHAIN_WAIT, read)
        .await
        .flatten()
}

async fn collect(stdout: &mut BodyStream, cap: usize) -> Option<String> {
    let mut out = Vec::new();
    while let Some(chunk) = stdout.next_chunk().await {
        let chunk = chunk.ok()?;
        if out.len() + chunk.len() > cap {
            return None;
        }
        out.extend_from_slice(&chunk);
    }
    String::from_utf8(out).ok()
}

fn token(value: Option<&Value>) -> Option<SecretString> {
    value
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(|token| SecretString::from(token.to_owned()))
}

fn read_json(path: &Path, jsonc: bool) -> Result<Value, PluginError> {
    let bytes = std::fs::read(path)
        .map_err(|_| PluginError::auth("the Copilot editor or CLI sign-in cannot be read"))?;
    let bytes = if jsonc { strip_jsonc(&bytes) } else { bytes };
    json::from_slice(&bytes)
        .map_err(|_| PluginError::auth("the Copilot editor or CLI sign-in is not valid JSON"))
}

/// Removes JSONC comments and trailing commas without touching string data.
fn strip_jsonc(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut index = 0;
    let mut string = false;
    let mut escaped = false;
    while index < input.len() {
        let byte = input[index];
        if string {
            out.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            string = true;
            out.push(byte);
            index += 1;
        } else if byte == b'/' && input.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < input.len() && input[index] != b'\n' {
                index += 1;
            }
        } else if byte == b'/' && input.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < input.len() && &input[index..index + 2] != b"*/" {
                if input[index] == b'\n' {
                    out.push(b'\n');
                }
                index += 1;
            }
            index = (index + 2).min(input.len());
        } else {
            out.push(byte);
            index += 1;
        }
    }
    strip_trailing_commas(&out)
}

fn strip_trailing_commas(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut string = false;
    let mut escaped = false;
    for (index, byte) in input.iter().copied().enumerate() {
        if string {
            out.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string = false;
            }
            continue;
        }
        if byte == b'"' {
            string = true;
            out.push(byte);
            continue;
        }
        if byte == b',' {
            let mut next = index + 1;
            while input.get(next).is_some_and(u8::is_ascii_whitespace) {
                next += 1;
            }
            if matches!(input.get(next), Some(b'}' | b']')) {
                continue;
            }
        }
        out.push(byte);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_preserves_strings_and_drops_comments_and_trailing_commas() {
        let input = br#"{
          "text":"escaped quote: \" // /* stay",
          "url":"https://github.com",
          /* a block
             comment */
          "items":[1,2,],
        }"#;
        let value: Value = json::from_slice(&strip_jsonc(input)).expect("JSONC");
        assert_eq!(value["text"], "escaped quote: \" // /* stay");
        assert_eq!(value["url"], "https://github.com");
        assert_eq!(value["items"], json::json!([1, 2]));
        assert!(json::from_slice::<Value>(&strip_jsonc(b"{/* unterminated")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn secret_tool_output_is_read_bounded_and_trimmed() {
        use hofund_kit_subscription::{PipedScript, TestHost, block_on};
        use hofund_plugin_api::{PluginId, PluginKind};
        use hofund_plugin_sdk::oauth::secret_text;

        let dir = tempfile::Builder::new()
            .prefix("hofund-r48-copilot-secret-tool-")
            .tempdir()
            .expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("copilot"),
            dir.path(),
        );
        host.process().script_piped(
            PipedScript::new()
                .line("  gho-keychain\n")
                .exits(|_| Vec::new()),
        );
        let secret = block_on(run_secret_tool(
            host.context(),
            "/usr/bin/secret-tool",
            vec!["lookup".to_owned()],
            dir.path(),
        ))
        .expect("secret");
        assert_eq!(secret_text(&secret), "gho-keychain");

        host.process()
            .script_piped(PipedScript::new().exits(|_| Vec::new()));
        let empty = block_on(run_secret_tool(
            host.context(),
            "/usr/bin/secret-tool",
            Vec::new(),
            dir.path(),
        ));
        assert!(empty.is_none());

        host.process().script_piped(
            PipedScript::new()
                .line("x".repeat(4097))
                .exits(|_| Vec::new()),
        );
        let oversized = block_on(run_secret_tool(
            host.context(),
            "/usr/bin/secret-tool",
            Vec::new(),
            dir.path(),
        ));
        assert!(oversized.is_none());
    }
}
