//! Loopback callback listener for browser sign-in.
//!
//! The kernel binds `127.0.0.1` only, answers one callback per sign-in, and
//! hands the result to the caller of [`LoopbackRegistry::await_callback`]. The
//! plugin only learns the redirect URI. A pasted address is the fallback when
//! the browser runs on another machine or the fixed port is taken.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use hofund_plugin_api::kernel::LoopbackBackend;
use hofund_plugin_api::{
    BindBlocked, LOOPBACK_CANCEL_PATH, LoopbackBinding, LoopbackSpec, PluginError, PluginKey,
    RedirectHost, SignInProof,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{Notify, oneshot};

/// How long a browser gets to send its request head.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the port stays open after the sign-in finished.
const LINGER: Duration = Duration::from_secs(1);
/// The largest request head the listener reads.
const MAX_HEAD: usize = 8 * 1024;

type Outcome = Result<SignInProof, PluginError>;

/// The one-shot result of a sign-in, shared by the listener and the paste path.
struct Shared {
    result: Mutex<Option<oneshot::Sender<Outcome>>>,
    finished: Notify,
}

impl Shared {
    /// Delivers `outcome` if nothing has yet. Returns whether this call won.
    fn finish(&self, outcome: Outcome) -> bool {
        let sender = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match sender {
            Some(sender) => {
                let _ = sender.send(outcome);
                self.finished.notify_one();
                true
            }
            None => false,
        }
    }

    /// Drops the sender so a waiting caller sees a cancellation.
    fn abandon(&self) {
        drop(
            self.result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        self.finished.notify_one();
    }

    fn is_open(&self) -> bool {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

struct Session {
    plugin: PluginKey,
    redirect_uri: String,
    state: String,
    shared: Arc<Shared>,
    receiver: Mutex<Option<oneshot::Receiver<Outcome>>>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
}

/// The kernel's loopback listeners, one per sign-in session.
///
/// Give it to a context with `HostContext::with_loopback`. The sign-in driver
/// calls [`LoopbackRegistry::await_callback`] after `start_sign_in`, and
/// [`LoopbackRegistry::release`] on every exit path.
#[derive(Default)]
pub struct LoopbackRegistry {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl std::fmt::Debug for LoopbackRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        formatter
            .debug_struct("LoopbackRegistry")
            .field("sessions", &count)
            .finish()
    }
}

impl LoopbackRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn session(&self, session: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .cloned()
    }

    /// Waits for the sign-in in `session` to finish.
    ///
    /// It races the loopback callback against lines from `paste`, which the
    /// driver reads from the terminal. A pasted line must pass
    /// [`parse_pasted_callback`]; a line that does not is skipped, and the
    /// driver can check one itself first to tell the user. `paste` ending
    /// does not end the wait. Once the sign-in finishes, times out or is
    /// cancelled, the listener answers late requests with 409 for a short
    /// linger and then closes; [`LoopbackRegistry::release`] closes it at once
    /// and must still be called on every exit path.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `session` has no binding or is already
    /// awaited, the provider refused the sign-in, the sign-in was cancelled,
    /// or `deadline` passed.
    pub async fn await_callback<S>(
        &self,
        session: &str,
        paste: S,
        deadline: Duration,
    ) -> Result<SignInProof, PluginError>
    where
        S: Stream<Item = String> + Unpin,
    {
        let entry = self
            .session(session)
            .ok_or_else(|| PluginError::config("sign-in session has no loopback binding"))?;
        let mut receiver = entry
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| PluginError::config("sign-in session is already awaited"))?;
        let mut paste = paste.fuse();
        let timer = tokio::time::sleep(deadline);
        tokio::pin!(timer);
        loop {
            tokio::select! {
                outcome = &mut receiver => {
                    return outcome.unwrap_or_else(|_| {
                        Err(PluginError::cancelled("the sign-in was cancelled"))
                    });
                }
                line = paste.next(), if !paste.is_done() => {
                    if let Some(line) = line
                        && !line.trim().is_empty()
                    {
                        match parse_pasted_callback(&line, &entry.redirect_uri, &entry.state) {
                            Ok(proof) => {
                                entry.shared.finish(Ok(proof));
                            }
                            Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Auth => {
                                entry.shared.finish(Err(error));
                            }
                            Err(_) => {
                                tracing::debug!("pasted address does not match this sign-in");
                            }
                        }
                    }
                }
                () = &mut timer => {
                    entry.shared.abandon();
                    return Err(PluginError::timeout("the sign-in timed out"));
                }
            }
        }
    }

    /// Closes the listener for `session` and cancels a pending wait.
    pub fn release(&self, session: &str) {
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session);
        if let Some(entry) = entry {
            entry.shared.abandon();
            drop(
                entry
                    .shutdown
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take(),
            );
        }
    }
}

impl LoopbackBackend for LoopbackRegistry {
    fn bind(
        &self,
        plugin: &PluginKey,
        session: &str,
        spec: &LoopbackSpec,
    ) -> Result<LoopbackBinding, PluginError> {
        let state = spec
            .state()
            .filter(|state| !state.is_empty())
            .ok_or_else(|| PluginError::config("loopback sign-in needs an expected state"))?
            .to_owned();
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| PluginError::internal("loopback listener needs a tokio runtime"))?;
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sessions.contains_key(session) {
            return Err(PluginError::config(
                "sign-in session already has a loopback binding",
            ));
        }
        let requested = spec.requested_port().unwrap_or(0);
        let (listener, port, blocked) = {
            let _guard = runtime.enter();
            match listen(requested) {
                Ok(listener) => {
                    let port = listener
                        .local_addr()
                        .map_err(|error| {
                            PluginError::io("loopback listener address").with_source(error)
                        })?
                        .port();
                    (Some(listener), port, None)
                }
                Err(error) if requested != 0 && error.kind() == ErrorKind::AddrInUse => {
                    (None, requested, Some(BindBlocked::Busy))
                }
                Err(error) if requested != 0 && error.kind() == ErrorKind::PermissionDenied => {
                    (None, requested, Some(BindBlocked::Reserved))
                }
                Err(error) => {
                    return Err(
                        PluginError::io("cannot bind the loopback listener").with_source(error)
                    );
                }
            }
        };
        let host = match spec.host() {
            RedirectHost::Ipv4 => "127.0.0.1",
            _ => "localhost",
        };
        let redirect_uri = format!("http://{host}:{port}{}", spec.callback_path());
        let (sender, receiver) = oneshot::channel();
        let shared = Arc::new(Shared {
            result: Mutex::new(Some(sender)),
            finished: Notify::new(),
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        if let Some(listener) = listener {
            runtime.spawn(serve(
                listener,
                spec.callback_path(),
                state.clone(),
                Arc::clone(&shared),
                shutdown_rx,
            ));
        }
        sessions.insert(
            session.to_owned(),
            Arc::new(Session {
                plugin: plugin.clone(),
                redirect_uri: redirect_uri.clone(),
                state,
                shared,
                receiver: Mutex::new(Some(receiver)),
                shutdown: Mutex::new(Some(shutdown_tx)),
            }),
        );
        Ok(LoopbackBinding::from_parts(redirect_uri, port, blocked))
    }

    fn release(&self, plugin: &PluginKey, session: &str) {
        let owned = self
            .session(session)
            .is_some_and(|entry| entry.plugin == *plugin);
        if owned {
            Self::release(self, session);
        }
    }
}

/// Binds `127.0.0.1:port`. Unix sets `SO_REUSEADDR` so a port left in
/// `TIME_WAIT` by the last sign-in binds again; Windows does not, because
/// there it would let two listeners share the port.
fn listen(port: u16) -> std::io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    socket.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    socket.listen(16)
}

async fn serve(
    listener: TcpListener,
    path: &'static str,
    state: String,
    shared: Arc<Shared>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut linger: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    loop {
        let lingering = linger.is_some();
        let close = async {
            match linger.as_mut() {
                Some(sleep) => sleep.as_mut().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = &mut shutdown => break,
            () = close => break,
            () = shared.finished.notified(), if !lingering => {
                linger = Some(Box::pin(tokio::time::sleep(LINGER)));
            }
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(answer(stream, path, state.clone(), Arc::clone(&shared)));
                }
            }
        }
    }
}

async fn answer(mut stream: TcpStream, path: &'static str, state: String, shared: Arc<Shared>) {
    let Ok(Some(target)) = tokio::time::timeout(HEADER_TIMEOUT, read_target(&mut stream)).await
    else {
        let _ = respond(
            &mut stream,
            400,
            "Bad request",
            "This request was not understood.",
        )
        .await;
        return;
    };
    let (status, title, text) = route(&target, path, &state, &shared);
    let _ = respond(&mut stream, status, title, text).await;
}

/// Decides the reply for one request target and delivers any outcome.
fn route(
    target: &str,
    path: &str,
    state: &str,
    shared: &Shared,
) -> (u16, &'static str, &'static str) {
    let (request_path, query) = target.split_once('?').unwrap_or((target, ""));
    if request_path == LOOPBACK_CANCEL_PATH {
        if shared.finish(Err(PluginError::cancelled("the sign-in was cancelled"))) {
            return (
                200,
                "Sign-in cancelled",
                "The sign-in was cancelled. You can close this tab.",
            );
        }
        return (
            409,
            "Sign-in finished",
            "This sign-in has already finished.",
        );
    }
    if request_path != path {
        return (404, "Not found", "There is nothing here.");
    }
    if !shared.is_open() {
        return (
            409,
            "Sign-in finished",
            "This sign-in has already finished.",
        );
    }
    let params = CallbackParams::parse(query);
    if params.state.as_deref() != Some(state) {
        return (
            400,
            "Wrong sign-in",
            "This address is not for the sign-in in progress.",
        );
    }
    if let Some(error) = params.error {
        shared.finish(Err(refused(&error)));
        return (
            200,
            "Sign-in failed",
            "The provider refused the sign-in. You can close this tab.",
        );
    }
    match params.code {
        Some(code) if !code.is_empty() => {
            if shared.finish(Ok(SignInProof::callback(code, state.to_owned()))) {
                (
                    200,
                    "Signed in",
                    "Sign-in complete. You can close this tab.",
                )
            } else {
                (
                    409,
                    "Sign-in finished",
                    "This sign-in has already finished.",
                )
            }
        }
        _ => (400, "Bad request", "The address has no authorization code."),
    }
}

/// The provider's `error` value names the refusal only when it is a short
/// plain token, so a crafted value cannot carry text into logs.
fn refused(error: &str) -> PluginError {
    let plain = !error.is_empty()
        && error.len() <= 64
        && error.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.'
        });
    if plain {
        PluginError::auth(format!("the provider refused the sign-in: {error}"))
    } else {
        PluginError::auth("the provider refused the sign-in")
    }
}

#[derive(Default)]
struct CallbackParams {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

impl CallbackParams {
    fn parse(query: &str) -> Self {
        let mut params = Self::default();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let slot = match key.as_ref() {
                "state" => &mut params.state,
                "code" => &mut params.code,
                "error" => &mut params.error,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(value.into_owned());
            }
        }
        params
    }
}

/// Reads a request head and returns the target of a `GET`, or `None`.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let line = head.split(|byte| *byte == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method != "GET" || !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return None;
    }
    Some(target.to_owned())
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    title: &str,
    text: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body><p>{text}</p></body></html>"
    );
    let reply = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(reply.as_bytes()).await?;
    stream.shutdown().await
}

/// Checks an address the user pasted from the browser.
///
/// The host must be `localhost`, `127.0.0.1`, or `::1`, and the port, path,
/// and `state` must match the binding. Error text never repeats the address.
///
/// # Errors
///
/// Returns a `Config` error when the address does not match this sign-in or
/// has no code, and an `Auth` error when it carries the provider's refusal.
pub fn parse_pasted_callback(
    raw: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<SignInProof, PluginError> {
    let mismatch = || PluginError::config("the pasted address is not for this sign-in");
    let expected = url::Url::parse(redirect_uri)
        .map_err(|_| PluginError::internal("the loopback redirect URI does not parse"))?;
    let pasted = url::Url::parse(raw.trim()).map_err(|_| mismatch())?;
    let host_ok = matches!(
        pasted.host(),
        Some(url::Host::Domain("localhost"))
            | Some(url::Host::Ipv4(Ipv4Addr::LOCALHOST))
            | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
    );
    if pasted.scheme() != "http"
        || !host_ok
        || pasted.port_or_known_default() != expected.port_or_known_default()
        || pasted.path() != expected.path()
    {
        return Err(mismatch());
    }
    let params = CallbackParams::parse(pasted.query().unwrap_or(""));
    if params.state.as_deref() != Some(state) {
        return Err(mismatch());
    }
    if let Some(error) = params.error {
        return Err(refused(&error));
    }
    match params.code {
        Some(code) if !code.is_empty() => Ok(SignInProof::callback(code, state.to_owned())),
        _ => Err(PluginError::config(
            "the pasted address has no authorization code",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::{StreamExt, stream};
    use hofund_plugin_api::kernel::LoopbackBackend;
    use hofund_plugin_api::{
        BindBlocked, LoopbackSpec, PluginErrorKind, PluginId, PluginKey, PluginKind, RedirectHost,
        SignInProof,
    };
    use secrecy::ExposeSecret;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::{LoopbackRegistry, parse_pasted_callback};

    const STATE: &str = "state-0123456789";
    const WAIT: Duration = Duration::from_secs(5);

    fn key() -> PluginKey {
        PluginKey::new(PluginKind::Subscription, PluginId::from_static("codex"))
    }

    fn session() -> String {
        crate::sign_in_ctx(PluginId::from_static("codex"))
            .expect("ctx")
            .session()
            .to_owned()
    }

    fn spec() -> LoopbackSpec {
        LoopbackSpec::ephemeral().expect_state(STATE)
    }

    fn no_paste() -> stream::Pending<String> {
        stream::pending()
    }

    async fn get(port: u16, target: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.expect("read");
        let status = reply[9..12].parse().expect("status");
        (status, reply)
    }

    fn code(proof: &SignInProof) -> String {
        match proof {
            SignInProof::Callback { code, state } => {
                assert_eq!(state, STATE);
                code.expose_secret().to_owned()
            }
            other => panic!("not a callback: {other:?}"),
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe")
            .local_addr()
            .expect("addr")
            .port()
    }

    async fn port_is_free(port: u16) -> bool {
        for _ in 0..40 {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn binds_only_ipv4_loopback() {
        let registry = LoopbackRegistry::new();
        let binding = registry
            .bind(&key(), &session(), &spec().path("/auth/callback"))
            .expect("bind");
        assert_eq!(binding.blocked(), None);
        assert_eq!(
            binding.redirect_uri(),
            format!("http://localhost:{}/auth/callback", binding.port())
        );
        assert!(
            TcpStream::connect(("127.0.0.1", binding.port()))
                .await
                .is_ok()
        );
        assert!(
            TcpStream::connect(("::1", binding.port())).await.is_err(),
            "the listener must not answer on IPv6"
        );
        let ipv4 = registry
            .bind(
                &key(),
                &session(),
                &spec().redirect_host(RedirectHost::Ipv4),
            )
            .expect("bind");
        assert_eq!(
            ipv4.redirect_uri(),
            format!("http://127.0.0.1:{}/callback", ipv4.port())
        );
    }

    #[tokio::test]
    async fn fixed_port_busy_reports_busy_and_paste_works() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("hold");
        let port = holder.local_addr().expect("addr").port();
        let registry = LoopbackRegistry::new();
        let session = session();
        let binding = registry
            .bind(
                &key(),
                &session,
                &LoopbackSpec::fixed(port).expect_state(STATE),
            )
            .expect("a busy port still binds paste-only");
        assert_eq!(binding.blocked(), Some(BindBlocked::Busy));
        assert_eq!(binding.port(), port);
        assert_eq!(
            binding.redirect_uri(),
            format!("http://localhost:{port}/callback")
        );
        let paste = stream::iter(vec![
            String::new(),
            "not an address".to_owned(),
            format!("http://localhost:{port}/callback?code=wrong&state=other"),
            format!("  http://localhost:{port}/callback?code=pasted-code&state={STATE}\n"),
        ])
        .chain(stream::pending());
        let proof = registry
            .await_callback(&session, Box::pin(paste), WAIT)
            .await
            .expect("pasted");
        assert_eq!(code(&proof), "pasted-code");
        drop(holder);
    }

    #[tokio::test]
    async fn fixed_port_binds_and_rebinds_after_release() {
        let port = free_port();
        let registry = LoopbackRegistry::new();
        for _ in 0..2 {
            let session = session();
            let binding = registry
                .bind(
                    &key(),
                    &session,
                    &LoopbackSpec::fixed(port).expect_state(STATE),
                )
                .expect("bind");
            assert_eq!((binding.port(), binding.blocked()), (port, None));
            let (status, _) = get(port, &format!("/callback?code=c&state={STATE}")).await;
            assert_eq!(status, 200);
            registry.release(&session);
            assert!(port_is_free(port).await);
        }
    }

    #[tokio::test]
    async fn wrong_state_does_not_claim() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let (status, _) = get(port, "/callback?code=foreign&state=other").await;
        assert_eq!(status, 400);
        let (status, _) = get(port, "/callback?code=foreign").await;
        assert_eq!(status, 400);
        let (status, _) = get(port, &format!("/callback?code=mine&state={STATE}")).await;
        assert_eq!(status, 200);
        let proof = registry
            .await_callback(&session, no_paste(), WAIT)
            .await
            .expect("claimed");
        assert_eq!(code(&proof), "mine");
    }

    #[tokio::test]
    async fn second_callback_after_claim_rejected() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let (status, reply) = get(port, &format!("/callback?code=first-code&state={STATE}")).await;
        assert_eq!(status, 200);
        assert!(!reply.contains("first-code"), "the page shows the code");
        let head = reply.to_ascii_lowercase();
        assert!(head.contains("cache-control: no-store"), "{reply}");
        assert!(head.contains("referrer-policy: no-referrer"), "{reply}");
        let (status, _) = get(port, &format!("/callback?code=second-code&state={STATE}")).await;
        assert_eq!(status, 409);
        let proof = registry
            .await_callback(&session, no_paste(), WAIT)
            .await
            .expect("claimed");
        assert_eq!(code(&proof), "first-code");
    }

    #[tokio::test]
    async fn port_closes_after_the_reply() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let (status, _) = get(port, &format!("/callback?code=c&state={STATE}")).await;
        assert_eq!(status, 200);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            TcpStream::connect(("127.0.0.1", port)).await.is_err(),
            "the port stays open after the linger"
        );
    }

    #[tokio::test]
    async fn error_param_fails_without_echoing_code() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let (status, reply) = get(
            port,
            &format!("/callback?error=access_denied&error_description=desc-sentinel&code=code-sentinel&state={STATE}"),
        )
        .await;
        assert_eq!(status, 200);
        assert!(!reply.contains("code-sentinel") && !reply.contains("desc-sentinel"));
        let error = registry
            .await_callback(&session, no_paste(), WAIT)
            .await
            .expect_err("refused");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
        let shown = format!("{error} {error:?}");
        assert!(shown.contains("access_denied"), "{shown}");
        assert!(!shown.contains("code-sentinel") && !shown.contains("desc-sentinel"));
    }

    #[test]
    fn a_crafted_error_value_is_not_repeated() {
        let error = super::refused("<script>alert(1)</script>");
        assert_eq!(error.message(), "the provider refused the sign-in");
        let long = "a".repeat(65);
        assert_eq!(
            super::refused(&long).message(),
            "the provider refused the sign-in"
        );
        assert_eq!(
            super::refused(&"a".repeat(64)).message(),
            format!("the provider refused the sign-in: {}", "a".repeat(64))
        );
    }

    #[tokio::test]
    async fn unknown_path_404() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let (status, _) = get(port, &format!("/favicon.ico?code=c&state={STATE}")).await;
        assert_eq!(status, 404);
        let mut post = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        post.write_all(b"POST /callback HTTP/1.1\r\n\r\n")
            .await
            .expect("write");
        let mut reply = String::new();
        post.read_to_string(&mut reply).await.expect("read");
        assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
        let (status, _) = get(port, &format!("/callback?code=c&state={STATE}")).await;
        assert_eq!(status, 200);
        assert!(
            registry
                .await_callback(&session, no_paste(), WAIT)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn an_idle_connection_does_not_block_the_callback() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let _idle = TcpStream::connect(("127.0.0.1", port)).await.expect("idle");
        let (status, _) = tokio::time::timeout(
            Duration::from_secs(3),
            get(port, &format!("/callback?code=c&state={STATE}")),
        )
        .await
        .expect("answered while another connection idles");
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn cancel_path_cancels() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let (status, _) = get(port, "/cancel").await;
        assert_eq!(status, 200);
        let error = registry
            .await_callback(&session, no_paste(), WAIT)
            .await
            .expect_err("cancelled");
        assert_eq!(error.kind(), PluginErrorKind::Cancelled);
    }

    #[test]
    fn pasted_url_must_match_host_port_path_state() {
        let redirect = "http://localhost:1455/auth/callback";
        for good in [
            format!("http://localhost:1455/auth/callback?code=c&state={STATE}"),
            format!("http://127.0.0.1:1455/auth/callback?state={STATE}&code=c"),
            format!("http://[::1]:1455/auth/callback?code=c&state={STATE}&scope=x"),
        ] {
            let proof = parse_pasted_callback(&good, redirect, STATE).expect(&good);
            assert_eq!(code(&proof), "c");
        }
        for bad in [
            format!("https://localhost:1455/auth/callback?code=c&state={STATE}"),
            format!("http://evil.example:1455/auth/callback?code=c&state={STATE}"),
            format!("http://127.0.0.2:1455/auth/callback?code=c&state={STATE}"),
            format!("http://localhost:1456/auth/callback?code=c&state={STATE}"),
            format!("http://localhost/auth/callback?code=c&state={STATE}"),
            format!("http://localhost:1455/auth/other?code=c&state={STATE}"),
            "http://localhost:1455/auth/callback?code=c&state=other".to_owned(),
            "http://localhost:1455/auth/callback?code=c".to_owned(),
            format!("http://localhost:1455/auth/callback?state={STATE}"),
            format!("http://localhost:1455/auth/callback?code=&state={STATE}"),
            "code=c".to_owned(),
        ] {
            let error = parse_pasted_callback(&bad, redirect, STATE).expect_err(&bad);
            assert_eq!(error.kind(), PluginErrorKind::Config, "{bad}");
            assert!(!error.message().contains("code=c"), "{bad}");
        }
        let refused = parse_pasted_callback(
            &format!("http://localhost:1455/auth/callback?error=access_denied&state={STATE}"),
            redirect,
            STATE,
        )
        .expect_err("refused");
        assert_eq!(refused.kind(), PluginErrorKind::Auth);
    }

    #[tokio::test]
    async fn binding_released_on_cancel_sign_in() {
        let port = free_port();
        let registry = std::sync::Arc::new(LoopbackRegistry::new());
        let session = session();
        registry
            .bind(
                &key(),
                &session,
                &LoopbackSpec::fixed(port).expect_state(STATE),
            )
            .expect("bind");
        let waiter = {
            let registry = std::sync::Arc::clone(&registry);
            let session = session.clone();
            tokio::spawn(async move { registry.await_callback(&session, no_paste(), WAIT).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        registry.release(&session);
        let error = waiter.await.expect("join").expect_err("cancelled");
        assert_eq!(error.kind(), PluginErrorKind::Cancelled);
        assert!(port_is_free(port).await, "the port is still bound");
        let error = registry
            .await_callback(&session, no_paste(), WAIT)
            .await
            .expect_err("released");
        assert_eq!(error.kind(), PluginErrorKind::Config);
    }

    #[tokio::test]
    async fn deadline_times_out_and_a_second_wait_is_refused() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let error = registry
            .await_callback(&session, no_paste(), Duration::from_millis(50))
            .await
            .expect_err("deadline");
        assert_eq!(error.kind(), PluginErrorKind::Timeout);
        let (status, _) = get(port, &format!("/callback?code=c&state={STATE}")).await;
        assert_eq!(status, 409, "a timed-out sign-in is not claimed late");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            TcpStream::connect(("127.0.0.1", port)).await.is_err(),
            "the port stays open after a timeout"
        );
        let error = registry
            .await_callback(&session, no_paste(), WAIT)
            .await
            .expect_err("already awaited");
        assert_eq!(error.kind(), PluginErrorKind::Config);
    }

    #[tokio::test]
    async fn a_session_binds_once_and_needs_a_state() {
        let registry = LoopbackRegistry::new();
        let session = session();
        registry.bind(&key(), &session, &spec()).expect("bind");
        let error = registry.bind(&key(), &session, &spec()).expect_err("twice");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        let error = registry
            .bind(&key(), &self::session(), &LoopbackSpec::ephemeral())
            .expect_err("no state");
        assert_eq!(error.kind(), PluginErrorKind::Config);
    }

    #[tokio::test]
    async fn only_the_binding_plugin_releases_through_the_backend() {
        let registry = LoopbackRegistry::new();
        let session = session();
        let port = registry
            .bind(&key(), &session, &spec())
            .expect("bind")
            .port();
        let other = PluginKey::new(PluginKind::Subscription, PluginId::from_static("gemini"));
        LoopbackBackend::release(&registry, &other, &session);
        let (status, _) = get(port, &format!("/callback?code=c&state={STATE}")).await;
        assert_eq!(status, 200, "another plugin closed the listener");
        let proof = registry
            .await_callback(&session, no_paste(), WAIT)
            .await
            .expect("claimed");
        assert_eq!(code(&proof), "c");
        LoopbackBackend::release(&registry, &key(), &session);
        let error = registry
            .await_callback(&session, no_paste(), WAIT)
            .await
            .expect_err("released");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        assert!(port_is_free(port).await, "the port is still bound");
    }

    #[test]
    fn binding_needs_a_runtime() {
        let error = LoopbackRegistry::new()
            .bind(&key(), &session(), &spec())
            .expect_err("no runtime");
        assert_eq!(error.kind(), PluginErrorKind::Internal);
    }

    #[tokio::test]
    async fn the_test_host_context_binds_through_the_handle() {
        let root = tempfile::tempdir().expect("root");
        let host = crate::TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("codex"),
            root.path(),
        );
        let ctx = crate::sign_in_ctx(PluginId::from_static("codex")).expect("ctx");
        let binding = host.context().loopback().bind(&ctx, &spec()).expect("bind");
        let (status, _) = get(binding.port(), &format!("/callback?code=c&state={STATE}")).await;
        assert_eq!(status, 200);
        let proof = host
            .loopback()
            .await_callback(ctx.session(), no_paste(), WAIT)
            .await
            .expect("claimed");
        assert_eq!(code(&proof), "c");
        host.context().loopback().release(&ctx);
        let error = host
            .loopback()
            .await_callback(ctx.session(), no_paste(), WAIT)
            .await
            .expect_err("released");
        assert_eq!(error.kind(), PluginErrorKind::Config);
    }
}
