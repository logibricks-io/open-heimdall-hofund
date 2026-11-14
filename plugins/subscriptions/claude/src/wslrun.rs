//! Running Claude Code from a WSL distro when Windows has no local `claude`.
//!
//! Only **running** distros are probed; stopped distros are never started.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hofund_plugin_api::{HostContext, SpawnRequest};

use crate::cli;

/// Marker env var for distro-side stop (bridge runs use the same name).
pub(crate) const RUN_VAR: &str = "HOFUND_WSL_RUN";

const FIND_AGE: Duration = Duration::from_secs(60);
const UP_AGE: Duration = Duration::from_secs(3);
const UP_FAILED_AGE: Duration = Duration::from_secs(30);
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A command installed in a WSL distro.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tool {
    pub distro: String,
    pub path: String,
    pub path_env: String,
    pub mount: String,
    /// Windows host address inside the distro; empty when loopback is shared.
    pub host: String,
    pub root: bool,
    pub home: String,
}

/// Whether WSL discovery is active on this host.
#[must_use]
pub fn active() -> bool {
    cfg!(windows) || TEST_ON.load(Ordering::Relaxed)
}

static TEST_ON: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static WSL_TEST_GUARD: Mutex<()> = Mutex::new(());

/// Serializes WSL discovery tests that share [`set_run`] / [`set_test_on`] state.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    WSL_TEST_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Test hook: pretend this host has WSL to probe.
#[cfg(test)]
pub(crate) fn set_test_on(value: bool) {
    TEST_ON.store(value, Ordering::Relaxed);
}

type RunFn = dyn Fn(Duration, &[&str]) -> Result<Vec<u8>, ()> + Send + Sync;

static RUN: OnceLock<Mutex<Option<Box<RunFn>>>> = OnceLock::new();

fn run_slot() -> &'static Mutex<Option<Box<RunFn>>> {
    RUN.get_or_init(|| Mutex::new(None))
}

/// Test hook: replace `wsl.exe` invocation.
#[cfg(test)]
pub(crate) fn set_run(hook: Option<Box<RunFn>>) {
    *run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

async fn invoke(host: &HostContext, timeout: Duration, args: &[&str]) -> Result<Vec<u8>, ()> {
    if let Some(hook) = run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        return hook(timeout, args);
    }
    if !active() {
        return Err(());
    }
    let cwd = host.dirs().state_dir().map_err(|_| ())?;
    let request = SpawnRequest::new(
        wsl_exe(),
        args.iter().map(|arg| OsString::from(*arg)).collect(),
        vec![(OsString::from("WSL_UTF8"), OsString::from("1"))],
        cwd,
    )
    .map_err(|_| ())?;
    let run = async {
        let child = host
            .spawn_process()
            .spawn_piped(&request)
            .await
            .map_err(|_| ())?;
        let mut stdout = child.stdout().map_err(|_| ())?;
        let bytes = hofund_plugin_sdk::timeout(timeout, cli::collect_bytes(&mut stdout))
            .await
            .ok_or(())?;
        let code = child.exit_code().await.map_err(|_| ())?;
        if code != 0 {
            return Err(());
        }
        Ok(bytes)
    };
    run.await
}

struct FindCache {
    at: HashMap<String, Instant>,
    tools: HashMap<String, Option<Tool>>,
}

static FIND: Mutex<Option<FindCache>> = Mutex::new(None);

struct UpCache {
    at: Instant,
    names: Vec<String>,
    ok: bool,
}

fn up_cache() -> &'static Mutex<UpCache> {
    static CACHE: std::sync::LazyLock<Mutex<UpCache>> = std::sync::LazyLock::new(|| {
        Mutex::new(UpCache {
            at: Instant::now(),
            names: Vec::new(),
            ok: false,
        })
    });
    &CACHE
}

static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Drops cached find and distro-list answers (tests).
#[cfg(test)]
pub(crate) fn forget() {
    *FIND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    let mut up = up_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    up.at = Instant::now() - UP_FAILED_AGE - Duration::from_secs(1);
    up.names.clear();
    up.ok = false;
}

#[cfg(test)]
pub(crate) fn expire_running_cache() {
    let mut up = up_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    up.at = Instant::now() - UP_FAILED_AGE - Duration::from_secs(1);
}

/// The tool [`find`] last stored for `name`, without probing again.
#[must_use]
#[allow(dead_code)]
pub(crate) fn known(name: &str) -> Option<Tool> {
    let guard = FIND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = guard.as_ref()?;
    cache.tools.get(name).and_then(|tool| tool.clone())
}

/// Finds `name` in a running distro, default distro first.
pub(crate) async fn find(host: &HostContext, name: &str) -> Option<Tool> {
    if !active() {
        return None;
    }
    if let Some(tool) = find_cached(host, name).await {
        return Some(tool);
    }
    {
        let mut cache_guard = FIND
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache_guard.is_none() {
            *cache_guard = Some(FindCache {
                at: HashMap::new(),
                tools: HashMap::new(),
            });
        }
        let cache = cache_guard.as_mut().expect("cache");
        cache.at.insert(name.to_owned(), Instant::now());
        cache.tools.insert(name.to_owned(), None);
    }
    let (mut names, listed) = running(host).await;
    if !listed {
        return None;
    }
    let mut default = String::new();
    if let Ok(list) = invoke(host, Duration::from_secs(10), &["-l", "-q"]).await {
        let all = distros(&list);
        if let Some(first) = all.first() {
            default.clone_from(first);
        }
    }
    if !default.is_empty()
        && let Some(pos) = names.iter().position(|n| n == &default)
        && pos > 0
    {
        names.swap(0, pos);
    }
    for distro in names {
        let script = probe_script(name);
        let args = ["-d", &distro, "-e", "sh", "-lc", &script];
        let Ok(out) = invoke(host, Duration::from_secs(30), &args).await else {
            continue;
        };
        let text = String::from_utf8_lossy(&out);
        if let Some(tool) = parse_probe(&distro, &text) {
            let mut cache_guard = FIND
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cache) = cache_guard.as_mut() {
                cache.tools.insert(name.to_owned(), Some(tool.clone()));
            }
            return Some(tool);
        }
    }
    None
}

async fn find_cached(host: &HostContext, name: &str) -> Option<Tool> {
    let snapshot = FIND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|cache| {
            let at = cache.at.get(name)?;
            (at.elapsed() < FIND_AGE)
                .then_some(())
                .and_then(|()| match cache.tools.get(name) {
                    None | Some(None) => None,
                    Some(Some(tool)) => Some(tool.clone()),
                })
        });
    let tool = snapshot?;
    if up(host, &tool.distro).await {
        Some(tool)
    } else {
        let mut guard = FIND
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cache) = guard.as_mut() {
            cache.tools.insert(name.to_owned(), None);
        }
        None
    }
}

/// Whether `distro` is running now (does not start it).
pub(crate) async fn up(host: &HostContext, distro: &str) -> bool {
    if !active() {
        return false;
    }
    let (names, _) = running(host).await;
    names.iter().any(|name| name == distro)
}

async fn running(host: &HostContext) -> (Vec<String>, bool) {
    let stale = {
        let guard = up_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let age = if guard.ok { UP_AGE } else { UP_FAILED_AGE };
        guard.at.elapsed() > age
    };
    if stale {
        let probe = invoke(host, Duration::from_secs(10), &["-l", "--running", "-q"]).await;
        let mut guard = up_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match probe {
            Ok(bytes) => {
                guard.names = distros(&bytes);
                guard.ok = true;
            }
            Err(()) => {
                guard.names.clear();
                guard.ok = false;
            }
        }
        guard.at = Instant::now();
    }
    let guard = up_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (guard.names.clone(), guard.ok)
}

/// `wsl.exe` arguments to run this tool with `args` inside the distro.
#[must_use]
pub(crate) fn argv(tool: &Tool, args: &[&str]) -> Vec<OsString> {
    let mark = format!("{}", RUN_SEQ.fetch_add(1, Ordering::Relaxed) + 1);
    let mut out = vec![
        OsString::from("-d"),
        OsString::from(&tool.distro),
        OsString::from("--exec"),
        OsString::from("/usr/bin/env"),
        OsString::from(format!("PATH={}", tool.path_env)),
        OsString::from(format!("{RUN_VAR}={mark}")),
        OsString::from(&tool.path),
    ];
    for arg in args {
        out.push(OsString::from(*arg));
    }
    out
}

/// Absolute Windows path of `wsl.exe`.
#[must_use]
pub(crate) fn wsl_exe() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        return std::path::PathBuf::from(r"C:\Windows\System32\wsl.exe");
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from("/fake/wsl.exe")
    }
}

/// Rewrites a Windows drive path to the distro mount form.
#[must_use]
#[allow(dead_code)]
pub(crate) fn linux_path(tool: &Tool, win: &str) -> String {
    let Some(rest) = drive_tail(win) else {
        return win.to_owned();
    };
    let drive = win.chars().next().expect("drive").to_ascii_lowercase();
    let rest = rest.trim_start_matches(['\\', '/']).replace('\\', "/");
    let mount = if tool.mount.is_empty() {
        "/mnt/"
    } else {
        tool.mount.as_str()
    };
    format!("{mount}{drive}/{rest}")
}

/// Adds `WSLENV` and rewrites loopback proxy hosts when needed.
#[must_use]
pub(crate) fn cross_env(
    tool: &Tool,
    pairs: &[(OsString, OsString)],
    names: &[&str],
) -> Vec<(OsString, OsString)> {
    let mut have = HashMap::new();
    for (key, _) in pairs {
        if let Some(text) = key.to_str() {
            have.insert(text.to_ascii_uppercase(), ());
        }
    }
    let mut pass = Vec::new();
    for name in names {
        let key = name.split('/').next().unwrap_or(name);
        if have.contains_key(&key.to_ascii_uppercase()) {
            pass.push(OsString::from(*name));
        }
    }
    const PROXY_VARS: &[&str] = &[
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "all_proxy",
        "no_proxy",
    ];
    let mut out: Vec<(OsString, OsString)> = Vec::with_capacity(pairs.len() + 1);
    let mut existing_wslenv = String::new();
    for (key, value) in pairs {
        if key == "WSLENV" {
            if let Some(text) = value.to_str() {
                existing_wslenv = text.to_owned();
            }
            continue;
        }
        let key_text = key.to_str().unwrap_or_default();
        let mut value = value.clone();
        for proxy in PROXY_VARS {
            if key_text == *proxy {
                if key_text.eq_ignore_ascii_case("NO_PROXY") {
                    pass.push(key.clone());
                } else if let Some(text) = value.to_str() {
                    value = OsString::from(rewrite_proxy(tool, text));
                    pass.push(key.clone());
                }
                break;
            }
        }
        out.push((key.clone(), value));
    }
    if !existing_wslenv.is_empty() {
        pass.insert(0, OsString::from(existing_wslenv));
    }
    let joined: String = pass
        .iter()
        .filter_map(|item| item.to_str())
        .collect::<Vec<_>>()
        .join(":");
    if !joined.is_empty() {
        out.push((OsString::from("WSLENV"), OsString::from(joined)));
    }
    out
}

fn rewrite_proxy(tool: &Tool, value: &str) -> String {
    if tool.host.is_empty() {
        return value.to_owned();
    }
    let Some(scheme_end) = value.find("://") else {
        return value.to_owned();
    };
    let rest = &value[scheme_end + 3..];
    let path_start = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..path_start];
    let suffix = &value[scheme_end + 3 + path_start..];
    let prefix = &value[..scheme_end + 3];
    let (host, port) = if let Some((h, p)) = authority.rsplit_once(':') {
        (h, p)
    } else {
        (authority, "")
    };
    let lower = host.to_ascii_lowercase();
    if !matches!(lower.as_str(), "127.0.0.1" | "localhost" | "::1") {
        return value.to_owned();
    }
    let new_authority = if port.is_empty() {
        tool.host.clone()
    } else {
        format!("{}:{}", tool.host, port)
    };
    format!("{prefix}{new_authority}{suffix}")
}

#[allow(dead_code)]
fn drive_tail(raw: &str) -> Option<&str> {
    let mut chars = raw.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    if chars.next()? != ':' {
        return None;
    }
    match chars.next() {
        Some('\\') | Some('/') | None => Some(&raw[2..]),
        _ => None,
    }
}

fn probe_script(name: &str) -> String {
    format!(
        r#"for p in "$(command -v {name} 2>/dev/null)" \
"$(bash -ic 'command -v {name}' 2>/dev/null </dev/null | tail -n1)" \
"$HOME/.local/bin/{name}" "/usr/local/bin/{name}"; do \
case "$p" in /mnt/*|"") continue;; esac; \
[ -x "$p" ] && {{ echo "bin:$p"; \
echo "path:$(bash -ic 'echo $PATH' 2>/dev/null </dev/null | tail -n1)"; echo "lpath:$PATH"; break; }}; done; \
echo "mount:$(wslpath -u 'C:\' 2>/dev/null)"; echo "uid:$(id -u)"; echo "home:$HOME"; \
ip route show default 2>/dev/null | head -n1 | sed 's/^/route:/'; \
command -v wslinfo >/dev/null 2>&1 && echo "net:$(wslinfo --networking-mode 2>/dev/null)"; true"#
    )
}

#[must_use]
fn host_loopback(mode: &str) -> bool {
    matches!(
        mode.trim().to_ascii_lowercase().as_str(),
        "mirrored" | "consomme" | "virtioproxy"
    )
}

#[must_use]
fn on_path(path_env: &str, dir: &str) -> bool {
    path_env.split(':').any(|entry| entry == dir)
}

#[must_use]
pub(crate) fn parse_probe(distro: &str, out: &str) -> Option<Tool> {
    let mut tool = Tool {
        distro: distro.to_owned(),
        path: String::new(),
        path_env: String::new(),
        mount: "/mnt/".to_owned(),
        host: String::new(),
        root: false,
        home: String::new(),
    };
    let mut ipath = String::new();
    let mut lpath = String::new();
    let mut net = String::new();
    for line in out.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "bin" => tool.path = value.to_owned(),
            "path" => ipath = value.to_owned(),
            "lpath" => lpath = value.to_owned(),
            "mount" => {
                let stem = value.trim_end_matches('/');
                let mount = stem.strip_suffix('c').unwrap_or(stem);
                if mount.starts_with('/') && mount != value {
                    tool.mount = if mount.ends_with('/') {
                        mount.to_owned()
                    } else {
                        format!("{mount}/")
                    };
                }
            }
            "route" => {
                let fields: Vec<&str> = value.split_whitespace().collect();
                if fields.len() >= 3 && fields[1] == "via" && looks_like_ip(fields[2]) {
                    tool.host = fields[2].to_owned();
                }
            }
            "net" => net = value.to_ascii_lowercase(),
            "uid" => tool.root = value == "0",
            "home" if value.starts_with('/') => tool.home = value.to_owned(),
            _ => {}
        }
    }
    if !tool.path.starts_with('/') {
        return None;
    }
    tool.path_env = lpath;
    if ipath.starts_with('/') {
        tool.path_env = ipath;
    }
    if tool.path_env.is_empty() {
        tool.path_env = DEFAULT_PATH.to_owned();
    }
    if let Some(pos) = tool.path.rfind('/') {
        let dir = &tool.path[..pos];
        if !dir.is_empty() && !on_path(&tool.path_env, dir) {
            tool.path_env = format!("{dir}:{}", tool.path_env);
        }
    }
    if host_loopback(&net) {
        tool.host.clear();
    }
    Some(tool)
}

fn looks_like_ip(text: &str) -> bool {
    text.chars()
        .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == ':')
        && text.contains('.')
}

#[must_use]
pub(crate) fn distros(bytes: &[u8]) -> Vec<String> {
    let text = decode_wsl_list(bytes);
    text.lines()
        .map(|line| line.trim().trim_matches('\u{feff}').trim_end_matches('\r'))
        .filter(|line| !line.is_empty() && !line.to_ascii_lowercase().starts_with("docker-desktop"))
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_list(bytes: &[u8]) -> String {
    let utf16le = bytes.len() >= 2
        && ((bytes[0] == 0xff && bytes[1] == 0xfe) || (bytes[1] == 0 && bytes[0] != 0));
    if !utf16le {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let start = if bytes[0] == 0xff && bytes[1] == 0xfe {
        2
    } else {
        0
    };
    let body = &bytes[start..];
    let (pairs, _) = body.as_chunks::<2>();
    let code_units: Vec<u16> = pairs
        .iter()
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    String::from_utf16_lossy(&code_units)
}

#[cfg(test)]
mod tests {
    use super::{
        RUN_VAR, Tool, argv, cross_env, decode_wsl_list, distros, find, forget, host_loopback,
        parse_probe, probe_script, set_run, set_test_on, up, wsl_exe,
    };
    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::PluginKind;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::sync::Mutex;
    use std::time::Duration;

    fn probe_host(dir: &tempfile::TempDir) -> TestHost {
        TestHost::open_for(
            PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("claude"),
            dir.path(),
        )
    }

    static PROBED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn utf16le(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xfe];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parse_probe_reads_path_mount_and_nat_host() {
        let out = "bin:/home/me/.local/bin/claude\n\
path:/home/me/.nvm/bin:/usr/bin\n\
lpath:/usr/bin:/bin\n\
mount:/mnt/c/\n\
route:default via 172.20.0.1 dev eth0 proto kernel\n\
net:nat\n";
        let tool = parse_probe("Ubuntu", out).expect("found");
        assert_eq!(tool.distro, "Ubuntu");
        assert_eq!(tool.path, "/home/me/.local/bin/claude");
        assert_eq!(tool.mount, "/mnt/");
        assert_eq!(tool.host, "172.20.0.1");
        assert_eq!(
            tool.path_env,
            "/home/me/.local/bin:/home/me/.nvm/bin:/usr/bin"
        );
    }

    #[test]
    fn mirrored_networking_clears_nat_host() {
        let out = "bin:/usr/bin/claude\nlpath:/usr/bin\nmount:/win/c/\n\
route:default via 10.0.0.1 dev eth0\nnet:mirrored\n";
        let tool = parse_probe("d", out).expect("found");
        assert!(tool.host.is_empty());
        assert_eq!(tool.mount, "/win/");
    }

    #[test]
    fn parse_probe_skips_empty_bin() {
        assert!(parse_probe("d", "mount:/mnt/c/\n").is_none());
    }

    #[test]
    fn host_loopback_modes() {
        assert!(host_loopback("mirrored"));
        assert!(host_loopback("virtioproxy"));
        assert!(!host_loopback("nat"));
    }

    #[test]
    fn distros_skips_docker_desktop() {
        let list = distros(&utf16le("docker-desktop\r\nFedora\r\nUbuntu\r\n"));
        assert_eq!(list, vec!["Fedora".to_owned(), "Ubuntu".to_owned()]);
    }

    #[test]
    fn cross_env_lists_wslenv_and_rewrites_proxy() {
        let tool = Tool {
            distro: "Ubuntu".into(),
            path: "/usr/bin/claude".into(),
            path_env: "/usr/bin".into(),
            mount: "/mnt/".into(),
            host: "172.20.0.1".into(),
            root: false,
            home: String::new(),
        };
        let env = cross_env(
            &tool,
            &[
                (
                    OsString::from("CLAUDE_CONFIG_DIR"),
                    OsString::from(r"C:\cfg"),
                ),
                (
                    OsString::from("HTTPS_PROXY"),
                    OsString::from("http://127.0.0.1:7890"),
                ),
                (OsString::from("OTHER"), OsString::from("x")),
            ],
            &["CLAUDE_CONFIG_DIR/p"],
        );
        let mut map = HashMap::new();
        for (k, v) in env {
            map.insert(
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            );
        }
        assert_eq!(
            map.get("WSLENV"),
            Some(&"CLAUDE_CONFIG_DIR/p:HTTPS_PROXY".to_owned())
        );
        assert_eq!(
            map.get("HTTPS_PROXY"),
            Some(&"http://172.20.0.1:7890".to_owned())
        );
    }

    #[test]
    fn find_prefers_default_running_distro_and_caches() {
        let _guard = super::test_guard();
        let dir = tempfile::tempdir().expect("dir");
        let host = probe_host(&dir);
        set_test_on(true);
        forget();
        PROBED.lock().unwrap().clear();
        let old = std::mem::take(&mut *PROBED.lock().unwrap());
        let _ = old;
        set_run(Some(Box::new(
            |_timeout: Duration, args: &[&str]| match args.join(" ") {
                args if args == "-l --running -q" => {
                    Ok(utf16le("docker-desktop\r\nFedora\r\nUbuntu\r\n"))
                }
                args if args == "-l -q" => Ok(utf16le("Ubuntu\r\nFedora\r\nStopped\r\n")),
                _ => {
                    let distro = args.get(1).copied().unwrap_or("");
                    PROBED.lock().unwrap().push(distro.to_owned());
                    if distro == "Fedora" {
                        Ok(b"bin:/usr/bin/claude\nlpath:/usr/bin\n".to_vec())
                    } else {
                        Ok(b"mount:/mnt/c/\n".to_vec())
                    }
                }
            },
        )));
        let tool = block_on(find(host.context(), "claude")).expect("found");
        assert_eq!(tool.distro, "Fedora");
        assert_eq!(
            *PROBED.lock().unwrap(),
            vec!["Ubuntu".to_owned(), "Fedora".to_owned()]
        );
        PROBED.lock().unwrap().clear();
        let again = block_on(find(host.context(), "claude")).expect("cached");
        assert_eq!(again.distro, "Fedora");
        assert!(PROBED.lock().unwrap().is_empty());
        set_run(None);
        set_test_on(false);
        forget();
    }

    #[test]
    fn find_ignores_stopped_distro_after_refresh() {
        let _guard = super::test_guard();
        let dir = tempfile::tempdir().expect("dir");
        let host = probe_host(&dir);
        set_test_on(true);
        forget();
        let run_list = std::sync::Arc::new(std::sync::Mutex::new(utf16le("Fedora\r\nUbuntu\r\n")));
        let running = std::sync::Arc::clone(&run_list);
        set_run(Some(Box::new(
            move |_timeout: Duration, args: &[&str]| match args.join(" ") {
                args if args == "-l --running -q" => Ok(running.lock().unwrap().clone()),
                args if args == "-l -q" => Ok(utf16le("Ubuntu\r\nFedora\r\n")),
                _ => {
                    let distro = args.get(1).copied().unwrap_or("");
                    if distro == "Fedora" {
                        Ok(b"bin:/usr/bin/claude\nlpath:/usr/bin\n".to_vec())
                    } else {
                        Ok(b"mount:/mnt/c/\n".to_vec())
                    }
                }
            },
        )));
        assert!(block_on(find(host.context(), "claude")).is_some());
        *run_list.lock().expect("run list") = utf16le("Ubuntu\r\n");
        super::expire_running_cache();
        assert!(block_on(find(host.context(), "claude")).is_none());
        assert!(!block_on(up(host.context(), "Fedora")));
        assert!(block_on(up(host.context(), "Ubuntu")));
        set_run(None);
        set_test_on(false);
        forget();
    }

    #[test]
    fn find_probes_through_host_spawn_when_hook_absent() {
        let _guard = super::test_guard();
        let dir = tempfile::tempdir().expect("dir");
        let host = probe_host(&dir);
        set_test_on(true);
        forget();
        let wsl = wsl_exe();
        host.process()
            .script_piped(wsl_probe_script(&wsl, "running"));
        host.process().script_piped(wsl_probe_script(&wsl, "all"));
        host.process()
            .script_piped(wsl_probe_script(&wsl, "ubuntu"));
        let tool = block_on(find(host.context(), "claude")).expect("found");
        assert_eq!(tool.distro, "Ubuntu");
        assert_eq!(tool.path, "/home/me/.local/bin/claude");
        let spawns = host.process().piped_spawns();
        assert_eq!(spawns.len(), 3);
        assert!(spawns.iter().all(|spawn| spawn.program == wsl));
        set_test_on(false);
        forget();
    }

    fn wsl_probe_script(
        program: &std::path::Path,
        kind: &str,
    ) -> hofund_kit_subscription::PipedScript {
        use hofund_kit_subscription::PipedScript;
        let kind = kind.to_owned();
        let program = program.to_path_buf();
        PipedScript::new().exits(move |spawn| {
            if spawn.program != program {
                return Vec::new();
            }
            match (kind.as_str(), spawn.arg_strings().join(" ").as_str()) {
                ("running", "-l --running -q") => vec!["Ubuntu\r\n".to_owned()],
                ("all", "-l -q") => vec!["Ubuntu\r\n".to_owned()],
                ("ubuntu", _)
                    if spawn.args.get(1).map(|a| a.to_string_lossy()) == Some("Ubuntu".into())
                        && spawn.args.get(2).map(|a| a.to_string_lossy()) == Some("-e".into()) =>
                {
                    vec!["bin:/home/me/.local/bin/claude\nlpath:/home/me/.local/bin\nmount:/mnt/c/\nnet:mirrored\n".to_owned()]
                }
                _ => Vec::new(),
            }
        })
    }

    #[test]
    fn argv_sets_path_and_run_marker() {
        let tool = Tool {
            distro: "Ubuntu".into(),
            path: "/usr/bin/claude".into(),
            path_env: "/usr/bin".into(),
            mount: "/mnt/".into(),
            host: String::new(),
            root: false,
            home: String::new(),
        };
        let args = argv(&tool, &["auth", "login"]);
        assert_eq!(args[0].to_string_lossy(), "-d");
        assert_eq!(args[1].to_string_lossy(), "Ubuntu");
        assert_eq!(args[2].to_string_lossy(), "--exec");
        assert!(
            args.iter()
                .any(|arg| arg.to_string_lossy().starts_with(&format!("{RUN_VAR}=")))
        );
        assert_eq!(args.last().unwrap().to_string_lossy(), "login");
    }

    #[test]
    fn probe_script_names_claude() {
        assert!(probe_script("claude").contains("command -v claude"));
    }

    #[test]
    fn decode_utf16_without_bom() {
        let raw = utf16le("Ubuntu");
        assert_eq!(decode_wsl_list(&raw[2..]), "Ubuntu");
    }
}
