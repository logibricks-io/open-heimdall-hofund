//! Moving the model listener to a new address without a restart.
//!
//! The server's accept loop reads from a [`SwapListener`]. A [`Relisten`]
//! handle replaces or closes the socket inside it; connections already
//! accepted run in their own tasks and finish on the old socket's streams.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// How long the accept loop waits after an accept error that is not about one
/// connection, such as running out of file descriptors. Matches axum.
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// A relisten could not move the model listener.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RelistenError {
    /// The new address could not be bound. The listener still serves the
    /// address it had before.
    #[error("cannot bind {to}: {source}")]
    Bind {
        /// The address that was requested.
        to: SocketAddr,
        /// The operating-system error.
        source: io::Error,
    },
    /// The old listener was closed to free its port, the new address could not
    /// be bound, and rebinding the old address failed too. Nothing listens.
    #[error("cannot restore the listener on {was}: {source}")]
    RestoreFailed {
        /// The address that was being served.
        was: SocketAddr,
        /// The operating-system error from the restore.
        source: io::Error,
    },
    /// The server has stopped accepting connections.
    #[error("the listener has stopped")]
    Stopped,
}

/// A check run before each relisten bind, for tests that need a bind to fail.
#[doc(hidden)]
#[derive(Clone)]
pub struct BindHook(Arc<dyn Fn(SocketAddr) -> io::Result<()> + Send + Sync>);

impl BindHook {
    /// Runs `check` before each relisten bind; an error fails that bind.
    pub fn new(check: impl Fn(SocketAddr) -> io::Result<()> + Send + Sync + 'static) -> Self {
        Self(Arc::new(check))
    }
}

impl fmt::Debug for BindHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("BindHook").finish_non_exhaustive()
    }
}

enum Swap {
    /// Serve from this socket; the previous one, if any, is closed.
    Install(TcpListener, oneshot::Sender<()>),
    /// Close the current socket and accept nothing until the next install.
    Close(oneshot::Sender<()>),
}

/// The accept side: the current socket plus the channel that replaces it.
pub(crate) struct SwapListener {
    current: Option<TcpListener>,
    swaps: mpsc::UnboundedReceiver<Swap>,
    open: bool,
    backoff: Option<Instant>,
}

/// Addresses the listener was asked for and actually bound.
#[derive(Clone, Copy, Debug)]
struct Addresses {
    requested: SocketAddr,
    bound: SocketAddr,
}

struct Shared {
    swaps: mpsc::UnboundedSender<Swap>,
    addresses: Mutex<Addresses>,
    gate: tokio::sync::Mutex<()>,
    hook: Option<BindHook>,
}

/// Moves a running model listener. Clones share one listener.
#[derive(Clone)]
pub struct Relisten {
    shared: Arc<Shared>,
}

impl fmt::Debug for Relisten {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let addresses = *self.shared.addresses.lock().expect("listener addresses");
        formatter
            .debug_struct("Relisten")
            .field("requested", &addresses.requested)
            .field("bound", &addresses.bound)
            .finish_non_exhaustive()
    }
}

/// Wraps `listener`, bound for `requested`, in a swappable accept loop.
pub(crate) fn swappable(
    listener: TcpListener,
    requested: SocketAddr,
    bound: SocketAddr,
) -> (SwapListener, Relisten) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let accept = SwapListener {
        current: Some(listener),
        swaps: receiver,
        open: true,
        backoff: None,
    };
    let handle = Relisten {
        shared: Arc::new(Shared {
            swaps: sender,
            addresses: Mutex::new(Addresses { requested, bound }),
            gate: tokio::sync::Mutex::new(()),
            hook: None,
        }),
    };
    (accept, handle)
}

impl Relisten {
    /// The address the listener is serving now.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addresses().bound
    }

    /// The address the listener was last asked for. With port 0 this differs
    /// from [`Self::local_addr`].
    #[must_use]
    pub fn requested(&self) -> SocketAddr {
        self.addresses().requested
    }

    /// Adds a check run before each relisten bind. Only possible before the
    /// handle is cloned.
    pub(crate) fn set_hook(&mut self, hook: BindHook) {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.hook = Some(hook);
        }
    }

    fn addresses(&self) -> Addresses {
        *self.shared.addresses.lock().expect("listener addresses")
    }

    /// Moves the listener to `to` and returns the address now bound.
    ///
    /// An address equal to the last request is a no-op. A port change binds
    /// the new address first and then closes the old one. The same port on a
    /// different host closes the old listener first, because binding
    /// `0.0.0.0:p` while `127.0.0.1:p` listens fails on Linux; if the new bind
    /// then fails, the old address is bound again. Connections that were
    /// already accepted finish on the old socket.
    ///
    /// # Errors
    ///
    /// [`RelistenError::Bind`] when the new address cannot be bound (the old
    /// one is still served), [`RelistenError::RestoreFailed`] when the old one
    /// could not be bound again either, and [`RelistenError::Stopped`] after
    /// the server stopped.
    pub async fn relisten(&self, to: SocketAddr) -> Result<SocketAddr, RelistenError> {
        let _gate = self.shared.gate.lock().await;
        let was = self.addresses();
        if to == was.requested {
            return Ok(was.bound);
        }
        let same_port = to.port() != 0 && to.port() == was.bound.port();
        if !same_port {
            let listener = self
                .bind(to)
                .await
                .map_err(|source| RelistenError::Bind { to, source })?;
            return self.install(to, listener).await;
        }
        self.send(Swap::Close).await?;
        match self.bind(to).await {
            Ok(listener) => self.install(to, listener).await,
            Err(source) => match self.bind(was.bound).await {
                Ok(old) => {
                    self.install(was.requested, old).await?;
                    Err(RelistenError::Bind { to, source })
                }
                Err(restore) => Err(RelistenError::RestoreFailed {
                    was: was.bound,
                    source: restore,
                }),
            },
        }
    }

    async fn bind(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        if let Some(hook) = &self.shared.hook {
            (hook.0)(addr)?;
        }
        TcpListener::bind(addr).await
    }

    async fn install(
        &self,
        requested: SocketAddr,
        listener: TcpListener,
    ) -> Result<SocketAddr, RelistenError> {
        let bound = listener
            .local_addr()
            .map_err(|source| RelistenError::Bind {
                to: requested,
                source,
            })?;
        self.send(|done| Swap::Install(listener, done)).await?;
        *self.shared.addresses.lock().expect("listener addresses") = Addresses { requested, bound };
        Ok(bound)
    }

    /// Hands a swap to the accept loop and waits until it has taken effect.
    async fn send(
        &self,
        swap: impl FnOnce(oneshot::Sender<()>) -> Swap,
    ) -> Result<(), RelistenError> {
        let (done, applied) = oneshot::channel();
        self.shared
            .swaps
            .send(swap(done))
            .map_err(|_| RelistenError::Stopped)?;
        applied.await.map_err(|_| RelistenError::Stopped)
    }
}

enum Step {
    Swap(Option<Swap>),
    Accepted(io::Result<(TcpStream, SocketAddr)>),
}

impl SwapListener {
    fn apply(&mut self, swap: Option<Swap>) {
        match swap {
            Some(Swap::Install(listener, done)) => {
                // Replacing the socket drops, and so closes, the old one.
                self.current = Some(listener);
                self.backoff = None;
                let _ = done.send(());
            }
            Some(Swap::Close(done)) => {
                self.current = None;
                let _ = done.send(());
            }
            // Every handle is gone; keep serving the current socket.
            None => self.open = false,
        }
    }
}

async fn accept_on(
    listener: Option<&TcpListener>,
    backoff: Option<Instant>,
) -> io::Result<(TcpStream, SocketAddr)> {
    if let Some(until) = backoff {
        tokio::time::sleep_until(until).await;
    }
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

impl axum::serve::Listener for SwapListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let step = tokio::select! {
                biased;
                swap = self.swaps.recv(), if self.open => Step::Swap(swap),
                accepted = accept_on(self.current.as_ref(), self.backoff) => Step::Accepted(accepted),
            };
            match step {
                Step::Swap(swap) => self.apply(swap),
                Step::Accepted(Ok(accepted)) => {
                    self.backoff = None;
                    return accepted;
                }
                Step::Accepted(Err(error)) => {
                    self.backoff = None;
                    if !is_connection_error(&error) {
                        tracing::error!(%error, "accept error");
                        self.backoff = Some(Instant::now() + ACCEPT_BACKOFF);
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        match &self.current {
            Some(listener) => listener.local_addr(),
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the listener is moving to a new address",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream as StdStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::{BindHook, RelistenError};
    use crate::{Running, serve_relistenable};

    const REQUEST: &[u8] = b"GET /health HTTP/1.1\r\nhost: hofund\r\n\r\n";

    async fn serve(listen: SocketAddr) -> Running {
        let router = axum::Router::new().route("/health", get(|| async { "ok" }));
        serve_relistenable(
            router,
            listen,
            Arc::new(crate::responses_ws::SocketRegistry::default()),
        )
        .await
        .expect("serve")
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    /// A loopback port that was free a moment ago.
    fn free_port() -> u16 {
        std::net::TcpListener::bind(loopback(0))
            .expect("probe")
            .local_addr()
            .expect("probe addr")
            .port()
    }

    fn accepts(addr: SocketAddr) -> bool {
        StdStream::connect(addr).is_ok()
    }

    /// Reads one `ok` response off a keep-alive connection.
    async fn read_ok(stream: &mut TcpStream) -> String {
        let mut response = Vec::new();
        let mut chunk = [0_u8; 512];
        while !response.ends_with(b"\r\n\r\nok") {
            let read = stream.read(&mut chunk).await.expect("read");
            assert!(read > 0, "connection closed: {response:?}");
            response.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8(response).expect("utf8")
    }

    async fn get_ok(addr: SocketAddr) -> String {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(REQUEST).await.expect("request");
        read_ok(&mut stream).await
    }

    #[tokio::test]
    async fn relisten_port_change_binds_new_before_closing_old() {
        let running = serve(loopback(0)).await;
        let old = running.local_addr;
        let to = loopback(free_port());
        let old_open_at_bind = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&old_open_at_bind);
        let running = running.with_bind_hook(BindHook::new(move |addr| {
            seen.lock().expect("seen").push((addr, accepts(old)));
            Ok(())
        }));

        let bound = running.relisten(to).await.expect("relisten");

        assert_eq!(bound, to);
        assert_eq!(*old_open_at_bind.lock().expect("seen"), vec![(to, true)]);
        assert_eq!(running.listen_addr(), to);
        assert!(get_ok(to).await.starts_with("HTTP/1.1 200"));
        assert!(!accepts(old), "the old listener is closed after the swap");
        running.shutdown().await;
    }

    #[tokio::test]
    async fn relisten_same_port_host_change_closes_first_and_restores() {
        let running = serve(loopback(0)).await;
        let old = running.local_addr;
        let to = SocketAddr::from((Ipv4Addr::UNSPECIFIED, old.port()));
        let refuse_to = Arc::new(Mutex::new(true));
        let old_open_at_bind = Arc::new(Mutex::new(Vec::new()));
        let (seen, refuse) = (Arc::clone(&old_open_at_bind), Arc::clone(&refuse_to));
        let running = running.with_bind_hook(BindHook::new(move |addr| {
            seen.lock().expect("seen").push((addr, accepts(old)));
            if addr == to && *refuse.lock().expect("refuse") {
                return Err(std::io::ErrorKind::AddrInUse.into());
            }
            Ok(())
        }));

        // The new bind fails after the old socket closed: the old one comes back.
        let error = running.relisten(to).await.expect_err("bind refused");
        assert!(
            matches!(&error, RelistenError::Bind { to: failed, .. } if *failed == to),
            "{error:?}"
        );
        assert_eq!(
            *old_open_at_bind.lock().expect("seen"),
            vec![(to, false), (old, false)]
        );
        assert_eq!(running.listen_addr(), old);
        assert!(get_ok(old).await.starts_with("HTTP/1.1 200"));

        // The same move succeeds once the bind is allowed, and the request is
        // remembered: asking again is a no-op.
        *refuse_to.lock().expect("refuse") = false;
        old_open_at_bind.lock().expect("seen").clear();
        assert_eq!(running.relisten(to).await.expect("relisten"), to);
        assert_eq!(*old_open_at_bind.lock().expect("seen"), vec![(to, false)]);
        assert_eq!(running.listen_addr(), to);
        assert!(
            get_ok(loopback(old.port()))
                .await
                .starts_with("HTTP/1.1 200")
        );
        assert_eq!(running.relisten(to).await.expect("no-op"), to);
        assert_eq!(old_open_at_bind.lock().expect("seen").len(), 1);
        running.shutdown().await;
    }

    #[tokio::test]
    async fn relisten_restore_failure_reports_the_old_address() {
        let running = serve(loopback(0)).await;
        let old = running.local_addr;
        let to = SocketAddr::from((Ipv4Addr::UNSPECIFIED, old.port()));
        let running = running.with_bind_hook(BindHook::new(|_| {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }));

        let error = running.relisten(to).await.expect_err("restore fails");

        assert!(
            matches!(&error, RelistenError::RestoreFailed { was, .. } if *was == old),
            "{error:?}"
        );
        assert!(!accepts(old), "nothing listens after a failed restore");
        running.shutdown().await;
    }

    #[tokio::test]
    async fn relisten_bind_failure_keeps_the_old_listener() {
        let running = serve(loopback(0)).await;
        let old = running.local_addr;
        let held = std::net::TcpListener::bind(loopback(0)).expect("held");
        let to = held.local_addr().expect("held addr");

        let error = running.relisten(to).await.expect_err("in use");

        assert!(matches!(error, RelistenError::Bind { .. }), "{error:?}");
        assert_eq!(running.listen_addr(), old);
        assert!(get_ok(old).await.starts_with("HTTP/1.1 200"));
        running.shutdown().await;
    }

    #[tokio::test]
    async fn relisten_to_the_requested_address_is_a_no_op() {
        let running = serve(loopback(0)).await;
        let binds = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&binds);
        let running = running.with_bind_hook(BindHook::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));

        // Port 0 was requested; the bound port differs but nothing moves.
        let bound = running.relisten(loopback(0)).await.expect("no-op");

        assert_eq!(bound, running.local_addr);
        assert_eq!(binds.load(Ordering::SeqCst), 0);
        running.shutdown().await;
    }

    #[tokio::test]
    async fn model_relisten_inflight_request_finishes() {
        let running = serve(loopback(0)).await;
        let old = running.local_addr;
        let mut stream = TcpStream::connect(old).await.expect("connect");
        stream.write_all(REQUEST).await.expect("first request");
        assert!(read_ok(&mut stream).await.starts_with("HTTP/1.1 200"));
        let (head, tail) = REQUEST.split_at(10);
        stream.write_all(head).await.expect("partial request");

        let to = loopback(free_port());
        running.relisten(to).await.expect("relisten");

        assert!(!accepts(old), "no new connections on the old address");
        stream.write_all(tail).await.expect("rest of the request");
        assert!(read_ok(&mut stream).await.starts_with("HTTP/1.1 200"));
        assert!(get_ok(to).await.starts_with("HTTP/1.1 200"));
        drop(stream);
        running.shutdown().await;
    }

    #[tokio::test]
    async fn relisten_after_shutdown_reports_stopped() {
        let running = serve(loopback(0)).await;
        let handle = running.relisten_handle().expect("handle");
        running.shutdown().await;

        let error = handle
            .relisten(loopback(free_port()))
            .await
            .expect_err("stopped");

        assert!(matches!(error, RelistenError::Stopped), "{error:?}");
    }
}
