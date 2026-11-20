//! Loopback WebSocket server for subscription plugin tests.

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;

type Frames = Arc<dyn Fn(usize, &str) -> Vec<String> + Send + Sync>;

/// Frames a connection sends after each text message.
pub struct SocketScript {
    refuse: Option<u16>,
    frames: Frames,
}

impl SocketScript {
    /// Accepts the handshake and answers each text message with `frames`.
    ///
    /// `frames` receives the zero-based turn on that connection and the text.
    pub fn replies(frames: impl Fn(usize, &str) -> Vec<String> + Send + Sync + 'static) -> Self {
        Self {
            refuse: None,
            frames: Arc::new(frames),
        }
    }

    /// Refuses the handshake with `status` and sends no frames.
    #[must_use]
    pub fn refuse(status: u16) -> Self {
        Self {
            refuse: Some(status),
            frames: Arc::new(|_, _| Vec::new()),
        }
    }
}

/// One accepted handshake and the text frames that followed it.
#[derive(Clone, Debug)]
pub struct SocketHit {
    headers: Vec<(String, String)>,
    messages: Vec<String>,
}

impl SocketHit {
    /// Handshake headers, in wire order.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// One handshake header, with a case-insensitive name lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Text frames received on this connection, in order.
    #[must_use]
    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

struct SharedHit {
    headers: Vec<(String, String)>,
    messages: Vec<String>,
}

/// A WebSocket server bound to IPv4 loopback.
pub struct LoopbackSocket {
    base_url: String,
    hits: Arc<Mutex<Vec<Arc<Mutex<SharedHit>>>>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    children: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl LoopbackSocket {
    /// Binds an ephemeral loopback port and serves `script`.
    ///
    /// # Panics
    ///
    /// Panics when the loopback listener cannot be bound.
    pub async fn start(script: SocketScript) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback websocket");
        let address = listener.local_addr().expect("loopback address");
        let hits = Arc::new(Mutex::new(Vec::new()));
        let children = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&hits);
        let spawned = Arc::clone(&children);
        let script = Arc::new(script);
        let (shutdown, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let recorded = Arc::clone(&recorded);
                        let script = Arc::clone(&script);
                        let child = tokio::spawn(async move {
                            serve_connection(stream, recorded, script).await;
                        });
                        spawned.lock().expect("children").push(child);
                    }
                }
            }
        });
        tokio::task::yield_now().await;
        Self {
            base_url: format!("http://{address}"),
            hits,
            shutdown,
            task: Some(task),
            children,
        }
    }

    /// Base URL with no trailing slash. Handshakes use this origin.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Handshakes so far, including ones the script refused.
    #[must_use]
    pub fn hits(&self) -> Vec<SocketHit> {
        self.hits
            .lock()
            .expect("hits")
            .iter()
            .map(|hit| {
                let hit = hit.lock().expect("hit");
                SocketHit {
                    headers: hit.headers.clone(),
                    messages: hit.messages.clone(),
                }
            })
            .collect()
    }

    /// How many handshakes were seen, refused or not.
    #[must_use]
    pub fn opened(&self) -> usize {
        self.hits.lock().expect("hits").len()
    }
}

impl Drop for LoopbackSocket {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        for child in self.children.lock().expect("children").drain(..) {
            child.abort();
        }
    }
}

#[allow(clippy::result_large_err)]
async fn serve_connection(
    stream: tokio::net::TcpStream,
    hits: Arc<Mutex<Vec<Arc<Mutex<SharedHit>>>>>,
    script: Arc<SocketScript>,
) {
    let hit = Arc::new(Mutex::new(SharedHit {
        headers: Vec::new(),
        messages: Vec::new(),
    }));
    let recorded = Arc::clone(&hit);
    let refuse = script.refuse;
    let accepted = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, response: Response| {
            let headers = request
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().unwrap_or("").to_owned(),
                    )
                })
                .collect();
            recorded.lock().expect("hit").headers = headers;
            if let Some(status) = refuse {
                let rejected = error_response(status);
                Err(rejected)
            } else {
                Ok(response)
            }
        },
    )
    .await;
    hits.lock().expect("hits").push(hit.clone());
    let Ok(mut socket) = accepted else {
        return;
    };
    let mut turn = 0_usize;
    while let Some(message) = socket.next().await {
        let Ok(message) = message else { break };
        let Message::Text(text) = message else {
            if message.is_close() {
                break;
            }
            continue;
        };
        let text = text.to_string();
        hit.lock().expect("hit").messages.push(text.clone());
        let frames = (script.frames)(turn, &text);
        turn = turn.saturating_add(1);
        let mut failed = false;
        for frame in frames {
            if socket.send(Message::Text(frame.into())).await.is_err() {
                failed = true;
                break;
            }
        }
        if failed {
            break;
        }
    }
}

fn error_response(status: u16) -> ErrorResponse {
    http::Response::builder()
        .status(status)
        .body(None)
        .expect("status is a refusal response")
}
