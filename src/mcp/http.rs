//! Streamable HTTP (MCP 2025-06-18) for one launcher-owned faculty context.
//!
//! POST returns the same bounded native JSON-RPC output as stdio; accepted
//! notifications return 202. GET returns 405 (no standalone SSE stream), and
//! DELETE closes a transport session. There is no event replay or automatic
//! retry. Losing an HTTP connection does not cancel an admitted operation.
//!
//! Native adapters can be blocking and need not be Send/Sync. They execute
//! sequentially on the calling thread, outside the dedicated HTTP I/O runtime.
//! A bounded channel connects the two; admission includes body reads, queued
//! work and running work, including work whose client has disconnected.
//!
//! This is an authenticated internal hop, not another OAuth server. A hosting
//! edge (such as Playground) must authenticate the user and select that user's
//! fixed worker/pile/key before forwarding. Do not switch process environment
//! or trust a caller-supplied tenant/path. Even loopback needs authentication:
//! some jail deployments share the parent's network stack. TLS and public
//! OAuth discovery belong to the existing hosting edge.
//!
//! The one exception is [`Config::app_loopback`], used only by a machine-local
//! desktop owner. It carries no credential: any local process able to connect
//! to its loopback port, including other users' processes, may use it. It
//! refuses browser-shaped requests instead: any Origin header (including
//! `null`), any Host or absolute-form authority other than this listener's
//! own address or `localhost` at its actual port, and every non-JSON POST.
//! It installs no CORS and answers no preflight. Bearer endpoints are unchanged.

use std::collections::HashMap;
use std::future::Future;
use std::io::{self, IoSlice, Read};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{DefaultBodyLimit, FromRequest, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::serve::Listener;
use axum::{routing::any, Router};
use rand_core::RngCore;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

use super::{decoded, envelope, Faculty, Limits, Server, PROTOCOL_VERSION};

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";

/// Only a constant-time-comparable digest is retained; never printed or
/// included in discovery. Use an independently generated random token for
/// each worker, not a colleague's public OAuth access token or signing key.
#[derive(Clone)]
pub struct BearerToken(blake3::Hash);

impl BearerToken {
    pub fn new(token: &str) -> Result<Self> {
        if !(32..=1024).contains(&token.len())
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
        {
            bail!("MCP HTTP token must contain 32..=1024 ASCII bearer-token characters");
        }
        Ok(Self(blake3::hash(token.as_bytes())))
    }

    /// Bounded read; permits one ordinary final LF/CRLF. The plaintext buffer
    /// is zeroized on every return path. File permissions are launcher-owned.
    pub fn from_file(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).context("open MCP HTTP token file")?;
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(1027)
            .read_to_end(&mut bytes)
            .context("read MCP HTTP token file")?;
        let text = std::str::from_utf8(&bytes).context("MCP HTTP token file must be UTF-8")?;
        let text = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        Self::new(text)
    }

    fn accepts(&self, value: &str) -> bool {
        // blake3::Hash equality uses constant_time_eq_32, also for bad lengths.
        self.0 == blake3::hash(value.as_bytes())
    }
}

/// How a request is admitted before any MCP processing.
#[derive(Clone)]
enum Access {
    Bearer(BearerToken),
    /// See [`Config::app_loopback`]. Never a default.
    AppLoopback,
}

pub struct Config {
    pub bind: SocketAddr,
    access: Access,
    /// Exact serialized origins, e.g. https://mcp.example.com (no final slash).
    /// Absent Origin is allowed for server-to-server clients. Every present
    /// Origin must be allowed; no CORS or wildcard access is installed.
    pub allowed_origins: Vec<String>,
    pub limits: Limits,
    /// Accepted TCP streams per endpoint, including idle/pre-header clients.
    /// Full capacity stops accepting; it does not reject or cancel native work.
    pub max_connections: usize,
    pub max_sessions: usize,
    pub session_idle_timeout: Duration,
    pub max_requests: usize,
    pub body_timeout: Duration,
}

impl Config {
    pub fn new(bind: SocketAddr, token: BearerToken) -> Self {
        Self::with_access(bind, Access::Bearer(token))
    }

    /// Erlkonig's machine-local owner, with no credential (see the module
    /// documentation). `bind` must be a loopback IP; `allowed_origins` must
    /// stay empty, because this mode refuses every Origin. Binding fails
    /// otherwise, including after either field is changed.
    pub fn app_loopback(bind: SocketAddr) -> Self {
        Self::with_access(bind, Access::AppLoopback)
    }

    fn with_access(bind: SocketAddr, access: Access) -> Self {
        Self {
            bind,
            access,
            allowed_origins: Vec::new(),
            limits: Limits::default(),
            max_connections: 64,
            max_sessions: 64,
            session_idle_timeout: Duration::from_secs(30 * 60),
            max_requests: 16,
            body_timeout: Duration::from_secs(30),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.max_connections == 0
            || self.max_connections > Semaphore::MAX_PERMITS
            || self.max_sessions == 0
            || self.max_requests == 0
            || self.max_requests > Semaphore::MAX_PERMITS
            || self.session_idle_timeout.is_zero()
            || self.body_timeout.is_zero()
        {
            bail!("MCP HTTP connection/session/admission limits and timeouts must be positive and supported");
        }
        if matches!(self.access, Access::AppLoopback)
            && (!self.bind.ip().is_loopback() || !self.allowed_origins.is_empty())
        {
            bail!("app loopback MCP HTTP requires a loopback IP bind and no allowed origins");
        }
        Server::with_limits(&[], self.limits)?;
        for origin in &self.allowed_origins {
            let parsed = reqwest::Url::parse(origin).context("parse allowed MCP HTTP origin")?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.origin().ascii_serialization() != *origin
            {
                bail!("MCP HTTP origins must be exact HTTP(S) origins without paths or wildcards");
            }
        }
        Ok(())
    }
}

/// Reserve before calling the underlying accept: clients waiting in the
/// kernel backlog must not become unbounded accepted process descriptors.
/// Axum can cancel this future on shutdown, releasing any reserved permit.
struct ConnectionListener<L> {
    inner: L,
    admission: Arc<Semaphore>,
    metrics: ConnectionMetrics,
}

/// One endpoint's accepted stream lifetimes, not requests, sessions, reserved
/// permits, kernel backlog, or all process sockets. Cloning only shares the
/// gauge. Reads allocate nothing and acquire no locks; they are not atomic
/// with an OS descriptor census. No addresses or descriptor numbers are kept.
#[derive(Clone)]
pub struct ConnectionMetrics(Arc<AtomicUsize>);

impl ConnectionMetrics {
    fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    pub fn accepted_streams(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    fn enter(&self) -> LiveConnection {
        // Admission bounds this by Semaphore::MAX_PERMITS, well below usize.
        self.0.fetch_add(1, Ordering::Relaxed);
        LiveConnection(self.clone())
    }
}

struct LiveConnection(ConnectionMetrics);

impl Drop for LiveConnection {
    fn drop(&mut self) {
        self.0 .0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<L: Listener> Listener for ConnectionListener<L> {
    type Io = AdmittedIo<L::Io>;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let admission = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .expect("private connection semaphore is never closed");
        // Retain Axum's ordinary socket-error retry behavior. Cancellation at
        // either await drops the permit; successful accept moves it with I/O.
        let (io, addr) = self.inner.accept().await;
        (
            AdmittedIo {
                io,
                _live: self.metrics.enter(),
                _admission: admission,
            },
            addr,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// The stream, not a request/session/handler, owns connection admission.
/// Field order closes I/O before releasing its slot. Keepalive requests keep
/// the same permit; every connection exit path drops it with the stream.
struct AdmittedIo<T> {
    io: T,
    // Field order closes I/O, decrements the gauge, then releases admission.
    _live: LiveConnection,
    _admission: OwnedSemaphorePermit,
}

impl<T: AsyncRead + Unpin> AsyncRead for AdmittedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for AdmittedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// Bind separately from serving so embedders/tests can select an OS-assigned
/// port, inspect its address, and supply their own shutdown signal.
pub struct Endpoint {
    listener: TcpListener,
    config: Config,
    metrics: ConnectionMetrics,
}

impl Endpoint {
    pub fn bind(config: Config) -> Result<Self> {
        config.validate()?;
        let listener = TcpListener::bind(config.bind).context("bind MCP HTTP listener")?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            config,
            metrics: ConnectionMetrics::new(),
        })
    }

    /// Observe this exact endpoint, including after it moves into serve_until.
    /// A retained handle does not keep its listener or streams alive.
    pub fn connection_metrics(&self) -> ConnectionMetrics {
        self.metrics.clone()
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .context("read MCP HTTP listening address")
    }

    pub fn serve(self, faculties: &[&dyn Faculty]) -> Result<()> {
        self.serve_until(faculties, shutdown_signal())
    }

    /// Graceful shutdown stops accepting connections and drains admitted work.
    /// Native calls are not forcibly interruptible; effects may already exist.
    /// Invoke outside a Tokio runtime, as for the blocking stdio frontend.
    pub fn serve_until(
        self,
        faculties: &[&dyn Faculty],
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        Server::with_limits(faculties, self.config.limits)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create MCP HTTP I/O runtime")?;
        let (tx, mut rx) = mpsc::channel(self.config.max_requests);
        let access = match self.config.access {
            Access::Bearer(token) => RequestAccess::Bearer(token),
            Access::AppLoopback => {
                // The actual bound address, so an OS-assigned port is exact.
                let local = self
                    .listener
                    .local_addr()
                    .context("read MCP HTTP listening address")?;
                RequestAccess::AppLoopback {
                    authorities: [local.to_string(), format!("localhost:{}", local.port())],
                }
            }
        };
        let state = Arc::new(HttpState {
            access,
            origins: self.config.allowed_origins,
            body_timeout: self.config.body_timeout,
            response_bytes: self.config.limits.response_bytes,
            admission: Arc::new(Semaphore::new(self.config.max_requests)),
            tx,
        });
        let app = Router::new()
            .route("/", any(handle))
            .layer(DefaultBodyLimit::max(self.config.limits.request_bytes))
            .with_state(state);
        let mut worker = Worker {
            faculties: faculties.to_vec(),
            limits: self.config.limits,
            sessions: HashMap::new(),
            max_sessions: self.config.max_sessions,
            idle_timeout: self.config.session_idle_timeout,
        };
        std::thread::scope(|scope| {
            let io = scope.spawn(move || {
                runtime.block_on(async move {
                    let listener = ConnectionListener {
                        inner: tokio::net::TcpListener::from_std(self.listener)?,
                        admission: Arc::new(Semaphore::new(self.config.max_connections)),
                        metrics: self.metrics,
                    };
                    axum::serve(listener, app)
                        .with_graceful_shutdown(shutdown)
                        .await
                        .context("serve MCP HTTP")
                })
            });
            while let Some(work) = rx.blocking_recv() {
                // A closed reply receiver is not cancellation. Never skip or
                // repeat an admitted write because the HTTP client went away.
                let response = worker.dispatch(work.command, work.session);
                let _ = work.reply.send((response, work.admission));
            }
            io.join()
                .map_err(|_| anyhow::anyhow!("MCP HTTP I/O thread panicked"))?
        })
    }
}

pub fn serve(faculties: &[&dyn Faculty], config: Config) -> Result<()> {
    let bearer = matches!(config.access, Access::Bearer(_));
    let endpoint = Endpoint::bind(config)?;
    if bearer {
        eprintln!("faculties MCP HTTP: http://{}/ (authenticated internal hop; public TLS/OAuth belong to the hosting edge)", endpoint.local_addr()?);
    } else {
        eprintln!(
            "faculties MCP HTTP: http://{}/ (machine-local app endpoint without a credential)",
            endpoint.local_addr()?
        );
    }
    endpoint.serve(faculties)
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = interrupt => {}, _ = terminate => {} }
}

enum RequestAccess {
    Bearer(BearerToken),
    /// The listener's own `ip:port` and `localhost:port`.
    AppLoopback {
        authorities: [String; 2],
    },
}

struct HttpState {
    access: RequestAccess,
    origins: Vec<String>,
    body_timeout: Duration,
    response_bytes: usize,
    admission: Arc<Semaphore>,
    tx: mpsc::Sender<Work>,
}

enum Command {
    Post(anybytes::Bytes),
    Get,
    Delete,
}

struct Work {
    command: Command,
    session: Option<String>,
    reply: oneshot::Sender<(Response, OwnedSemaphorePermit)>,
    admission: OwnedSemaphorePermit,
}

async fn handle(State(state): State<Arc<HttpState>>, request: Request) -> Response {
    let mut response = handle_request(state, request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn handle_request(state: Arc<HttpState>, request: Request) -> Response {
    let headers = request.headers();
    match &state.access {
        RequestAccess::Bearer(token) => {
            match single_header(headers, header::ORIGIN.as_str()) {
                Ok(None) => {}
                Ok(Some(origin)) if state.origins.iter().any(|allowed| allowed == origin) => {}
                _ => return error(StatusCode::FORBIDDEN, "Origin is not allowed"),
            }
            let authorized = single_header(headers, header::AUTHORIZATION.as_str())
                .ok()
                .flatten()
                .and_then(|value| value.split_once(' '))
                .is_some_and(|(scheme, value)| {
                    scheme.eq_ignore_ascii_case("Bearer")
                        && token.accepts(value.trim_start_matches(' '))
                });
            if !authorized {
                let mut response = error(
                    StatusCode::UNAUTHORIZED,
                    "MCP HTTP bearer authentication required",
                );
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                return response;
            }
        }
        RequestAccess::AppLoopback { authorities } => {
            if !local_authority(&request, authorities) {
                return error(StatusCode::FORBIDDEN, "Host is not this local endpoint");
            }
            // Browsers attach Origin to every POST and DELETE; no local
            // client of this endpoint sends one. A null or duplicate one too.
            if headers.contains_key(header::ORIGIN) {
                return error(StatusCode::FORBIDDEN, "Origin is not allowed");
            }
        }
    }
    match single_header(headers, VERSION_HEADER) {
        Ok(None) | Ok(Some(PROTOCOL_VERSION)) => {}
        _ => return error(StatusCode::BAD_REQUEST, "Unsupported MCP-Protocol-Version"),
    }
    let session = match single_header(headers, SESSION_HEADER) {
        Ok(None) => None,
        Ok(Some(value))
            if !value.is_empty()
                && value.len() <= 128
                && value.bytes().all(|b| (0x21..=0x7e).contains(&b)) =>
        {
            Some(value.to_owned())
        }
        _ => return error(StatusCode::BAD_REQUEST, "Invalid Mcp-Session-Id header"),
    };
    let method = request.method().clone();
    if method != Method::POST && method != Method::GET && method != Method::DELETE {
        return method_not_allowed();
    }
    if method == Method::POST {
        let json = single_header(headers, header::CONTENT_TYPE.as_str())
            .ok()
            .flatten()
            .and_then(|value| value.parse::<mime::Mime>().ok())
            .is_some_and(|value| value.essence_str() == "application/json");
        if !json
            || !matches!(
                single_header(headers, header::CONTENT_ENCODING.as_str()),
                Ok(None) | Ok(Some("identity"))
            )
        {
            return error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "MCP POST requires uncompressed application/json",
            );
        }
        if !accepts(headers, "application/json") || !accepts(headers, "text/event-stream") {
            return error(
                StatusCode::NOT_ACCEPTABLE,
                "MCP POST Accept must include application/json and text/event-stream",
            );
        }
    }
    let admission = match state.admission.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "MCP HTTP admission limit reached; request was not dispatched",
            )
        }
    };
    let command = if method == Method::POST {
        let body = match tokio::time::timeout(state.body_timeout, Bytes::from_request(request, &()))
            .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(rejection)) => return rejection.into_response(),
            Err(_) => {
                return error(
                    StatusCode::REQUEST_TIMEOUT,
                    "MCP HTTP request body timed out",
                )
            }
        };
        Command::Post(body.to_vec().into())
    } else if method == Method::GET {
        Command::Get
    } else {
        Command::Delete
    };
    let (reply, response) = oneshot::channel();
    if state
        .tx
        .try_send(Work {
            command,
            session,
            reply,
            admission,
        })
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP HTTP worker is unavailable; request was not dispatched",
        );
    }
    match response.await {
        Ok((response, admission)) => {
            let (parts, body) = response.into_parts();
            // Worker responses are bounded, resident bodies. Carry admission
            // in the Bytes owner so it lasts through slow socket writes and
            // clones of the outgoing frame, not just until this handler returns.
            let bytes = match to_bytes(body, state.response_bytes).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "MCP HTTP response exceeded transport limits",
                    )
                }
            };
            let bytes = Bytes::from_owner(AdmittedBody {
                bytes,
                _admission: admission,
            });
            Response::from_parts(parts, Body::from(bytes))
        }
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP HTTP worker stopped; operation outcome may be unknown; it was not retried",
        ),
    }
}

struct AdmittedBody {
    bytes: Bytes,
    _admission: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for AdmittedBody {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Exactly one Host naming this listener, and an absolute-form target, if
/// present, naming the same authority. Hostnames compare case-insensitively.
fn local_authority(request: &Request, authorities: &[String; 2]) -> bool {
    let expected = |value: &str| authorities.iter().any(|a| a.eq_ignore_ascii_case(value));
    let Ok(Some(host)) = single_header(request.headers(), header::HOST.as_str()) else {
        return false;
    };
    if !expected(host) {
        return false;
    }
    let uri = request.uri();
    match uri.authority() {
        None => uri.scheme().is_none(),
        Some(authority) => {
            uri.scheme_str() == Some("http") && authority.as_str().eq_ignore_ascii_case(host)
        }
    }
}

fn single_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
) -> std::result::Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    first
        .map(|value| value.to_str().map_err(|_| ()))
        .transpose()
}

fn accepts(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| value.trim().parse::<mime::Mime>().ok())
        .any(|value| {
            value.essence_str() == expected
                && value.get_param("q").is_none_or(|q| {
                    q.as_str()
                        .parse::<f32>()
                        .is_ok_and(|q| q.is_finite() && q > 0.0 && q <= 1.0)
                })
        })
}

struct Session<'a> {
    rpc: Server<'a>,
    touched: Instant,
}

struct Worker<'a> {
    faculties: Vec<&'a dyn Faculty>,
    limits: Limits,
    sessions: HashMap<String, Session<'a>>,
    max_sessions: usize,
    idle_timeout: Duration,
}

impl Worker<'_> {
    fn dispatch(&mut self, command: Command, session_id: Option<String>) -> Response {
        let now = Instant::now();
        self.sessions
            .retain(|_, session| now.duration_since(session.touched) < self.idle_timeout);
        if let Some(id) = &session_id {
            if !self.sessions.contains_key(id) {
                return error(
                    StatusCode::NOT_FOUND,
                    "MCP session is unknown or expired; initialize a new session",
                );
            }
        }
        match command {
            Command::Get => method_not_allowed(),
            Command::Delete => match session_id {
                Some(id) => {
                    self.sessions.remove(&id);
                    StatusCode::NO_CONTENT.into_response()
                }
                None => error(StatusCode::BAD_REQUEST, "Mcp-Session-Id is required"),
            },
            Command::Post(bytes) => {
                if let Some(id) = session_id {
                    let session = self.sessions.get_mut(&id).expect("validated above");
                    let response = session.rpc.dispatch(bytes);
                    session.touched = Instant::now();
                    match response {
                        Ok(response) => rpc_response(response),
                        Err(_) => {
                            self.sessions.remove(&id);
                            error(
                                StatusCode::BAD_REQUEST,
                                "MCP request exceeded protocol limits; initialize a new session",
                            )
                        }
                    }
                } else {
                    self.initialize(bytes)
                }
            }
        }
    }

    fn initialize(&mut self, bytes: anybytes::Bytes) -> Response {
        let frame = match envelope(bytes.clone(), self.limits.json_depth) {
            Ok(frame) => frame,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid JSON-RPC message"),
        };
        if frame
            .method
            .and_then(|method| decoded(method).ok())
            .as_deref()
            != Some("initialize")
        {
            return error(
                StatusCode::BAD_REQUEST,
                "Mcp-Session-Id is required after initialization",
            );
        }
        if self.sessions.len() >= self.max_sessions {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "MCP session limit reached; close or expire an existing session",
            );
        }
        let mut rpc = match Server::with_limits(&self.faculties, self.limits) {
            Ok(rpc) => rpc,
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "MCP registry is unavailable",
                )
            }
        };
        let response = match rpc.dispatch(bytes) {
            Ok(response) => response,
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "MCP initialization exceeded protocol limits",
                )
            }
        };
        let mut response = rpc_response(response);
        // Invalid params/envelopes and notifications never reserve sessions.
        if rpc.state == super::State::AwaitingInitialized {
            let id = loop {
                let mut entropy = [0_u8; 32];
                if rand_core::OsRng.try_fill_bytes(&mut entropy).is_err() {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Could not allocate MCP session identity",
                    );
                }
                let id = hex::encode(entropy);
                if !self.sessions.contains_key(&id) {
                    break id;
                }
            };
            response.headers_mut().insert(
                SESSION_HEADER,
                HeaderValue::from_str(&id).expect("hex header"),
            );
            self.sessions.insert(
                id,
                Session {
                    rpc,
                    touched: Instant::now(),
                },
            );
        }
        response
    }
}

fn rpc_response(response: Option<String>) -> Response {
    match response {
        Some(body) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": message }).to_string(),
    )
        .into_response()
}

fn method_not_allowed() -> Response {
    let mut response = error(
        StatusCode::METHOD_NOT_ALLOWED,
        "No standalone SSE stream; use POST or DELETE",
    );
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TOKEN: &str = "unit-test-only-token-0123456789abcdef";

    struct CountAccepts {
        calls: Arc<AtomicUsize>,
        pending: bool,
    }

    impl Listener for CountAccepts {
        type Io = tokio::io::DuplexStream;
        type Addr = ();

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.pending {
                std::future::pending::<()>().await;
            }
            (tokio::io::duplex(64).0, ())
        }

        fn local_addr(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn connection_admission_precedes_accept_and_lives_until_io_drop() {
        let calls = Arc::new(AtomicUsize::new(0));
        let admission = Arc::new(Semaphore::new(2));
        let metrics = ConnectionMetrics::new();
        let mut listener = ConnectionListener {
            inner: CountAccepts {
                calls: calls.clone(),
                pending: false,
            },
            admission: admission.clone(),
            metrics: metrics.clone(),
        };
        let (first, ()) = listener.accept().await;
        let (second, ()) = listener.accept().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(admission.available_permits(), 0);
        assert_eq!(metrics.accepted_streams(), 2);
        assert_eq!(metrics.clone().accepted_streams(), 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        // In particular, a third stream was not accepted and then parked
        // awaiting a permit: the underlying listener was never called.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(first);
        assert_eq!(metrics.accepted_streams(), 1);
        let (third, ()) = listener.accept().await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(admission.available_permits(), 0);
        drop((second, third));
        assert_eq!(admission.available_permits(), 2);
        assert_eq!(metrics.accepted_streams(), 0);
    }

    #[tokio::test]
    async fn cancelling_an_accept_releases_its_reserved_connection_slot() {
        let calls = Arc::new(AtomicUsize::new(0));
        let admission = Arc::new(Semaphore::new(1));
        let metrics = ConnectionMetrics::new();
        let mut listener = ConnectionListener {
            inner: CountAccepts {
                calls: calls.clone(),
                pending: true,
            },
            admission: admission.clone(),
            metrics: metrics.clone(),
        };
        let mut accepting = Box::pin(listener.accept());
        std::future::poll_fn(|cx| {
            assert!(accepting.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(admission.available_permits(), 0);
        assert_eq!(
            metrics.accepted_streams(),
            0,
            "reserved pre-accept slot is not a stream"
        );
        drop(accepting);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.available_permits(), 1);
        assert_eq!(metrics.accepted_streams(), 0);
    }

    #[tokio::test]
    async fn connection_gauge_survives_endpoint_drop_and_releases_on_io_error_unwind() {
        let endpoint = Endpoint::bind(Config::new(
            "127.0.0.1:0".parse().unwrap(),
            BearerToken::new(TOKEN).unwrap(),
        ))
        .unwrap();
        let retained = endpoint.connection_metrics();
        drop(endpoint);
        assert_eq!(retained.accepted_streams(), 0);
        let metrics = ConnectionMetrics::new();
        let admission = Arc::new(Semaphore::new(1));
        let mut listener = ConnectionListener {
            inner: CountAccepts {
                calls: Arc::new(AtomicUsize::new(0)),
                pending: false,
            },
            admission: admission.clone(),
            metrics: metrics.clone(),
        };
        let (mut stream, ()) = listener.accept().await;
        // CountAccepts drops the duplex peer; writing must fail, but only
        // dropping the owned I/O (not an error/request) ends its lifetime.
        use tokio::io::AsyncWriteExt;
        assert!(stream.write_all(b"x").await.is_err());
        assert_eq!(metrics.accepted_streams(), 1);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _stream = stream;
                panic!("synthetic connection task unwind");
            }))
            .is_err()
        );
        assert_eq!(metrics.accepted_streams(), 0);
        assert_eq!(admission.available_permits(), 1);
    }

    #[tokio::test]
    async fn connection_io_drops_before_gauge_and_permit() {
        struct CheckDrop(ConnectionMetrics, Arc<Semaphore>);
        impl Drop for CheckDrop {
            fn drop(&mut self) {
                assert_eq!(self.0.accepted_streams(), 1);
                assert_eq!(self.1.available_permits(), 0);
            }
        }
        let metrics = ConnectionMetrics::new();
        let admission = Arc::new(Semaphore::new(1));
        let stream = AdmittedIo {
            io: CheckDrop(metrics.clone(), admission.clone()),
            _live: metrics.enter(),
            _admission: admission.clone().acquire_owned().await.unwrap(),
        };
        drop(stream);
        assert_eq!(metrics.accepted_streams(), 0);
        assert_eq!(admission.available_permits(), 1);
    }

    #[test]
    fn connection_default_is_per_endpoint_sixty_four() {
        let config = Config::new(
            "127.0.0.1:0".parse().unwrap(),
            BearerToken::new(TOKEN).unwrap(),
        );
        assert_eq!(config.max_connections, 64);
    }

    #[test]
    fn bearer_file_is_bounded_and_accepts_only_one_final_line_ending() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("http.token");
        for ending in ["", "\n", "\r\n"] {
            std::fs::write(&path, format!("{TOKEN}{ending}")).unwrap();
            let token = BearerToken::from_file(&path).unwrap();
            assert!(token.accepts(TOKEN));
            assert!(!token.accepts("different-token-0123456789abcdef"));
        }
        for invalid in [
            "short".to_owned(),
            format!("{TOKEN}\n\n"),
            format!(" {TOKEN}"),
            "x".repeat(4096),
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(BearerToken::from_file(&path).is_err());
        }
        std::fs::write(&path, [0xff; 32]).unwrap();
        assert!(BearerToken::from_file(&path).is_err());
    }

    #[test]
    fn output_admission_lives_with_every_clone_of_the_outgoing_bytes() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().unwrap();
        let bytes = Bytes::from_owner(AdmittedBody {
            bytes: Bytes::from_static(b"native-response"),
            _admission: permit,
        });
        let outgoing = bytes.clone();
        drop(bytes);
        assert_eq!(semaphore.available_permits(), 0);
        assert_eq!(&outgoing[..], b"native-response");
        drop(outgoing);
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[test]
    fn invalid_configuration_fails_before_binding() {
        let config = || {
            Config::new(
                "127.0.0.1:0".parse().unwrap(),
                BearerToken::new(TOKEN).unwrap(),
            )
        };
        for origin in [
            "*",
            "null",
            "https://example.test/",
            "https://example.test/path",
            "file:///tmp",
        ] {
            let mut candidate = config();
            candidate.allowed_origins.push(origin.into());
            assert!(Endpoint::bind(candidate).is_err());
        }
        let mut candidate = config();
        candidate.max_connections = 0;
        assert!(Endpoint::bind(candidate).is_err());
        let mut candidate = config();
        candidate.max_connections = usize::MAX;
        assert!(Endpoint::bind(candidate).is_err());
        let mut candidate = config();
        candidate.max_requests = usize::MAX;
        assert!(Endpoint::bind(candidate).is_err());
        let mut candidate = config();
        candidate.max_sessions = 0;
        assert!(Endpoint::bind(candidate).is_err());
        let mut candidate = config();
        candidate.body_timeout = Duration::ZERO;
        assert!(Endpoint::bind(candidate).is_err());
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;

    /// Serve an empty registry until the returned sender is dropped.
    fn spawn(config: Config) -> (SocketAddr, oneshot::Sender<()>, std::thread::JoinHandle<()>) {
        let endpoint = Endpoint::bind(config).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            endpoint
                .serve_until(&[], async move {
                    let _ = stopped.await;
                })
                .unwrap();
        });
        (addr, stop, thread)
    }

    /// One raw HTTP/1.1 exchange; returns the status code and the lowercase head.
    /// Every socket step has a deadline, so a stalled endpoint fails the test.
    fn exchange(addr: SocketAddr, target: &str, headers: &[&str], body: &str) -> (u16, String) {
        use std::io::Write;
        let deadline = Some(Duration::from_secs(10));
        let mut stream = std::net::TcpStream::connect_timeout(&addr, deadline.unwrap()).unwrap();
        stream.set_read_timeout(deadline).unwrap();
        stream.set_write_timeout(deadline).unwrap();
        let mut head = format!("POST {target} HTTP/1.1\r\n");
        for header in headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        head.push_str(&format!(
            "Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        stream.write_all(head.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response[9..12].parse().unwrap();
        let head = response
            .split("\r\n\r\n")
            .next()
            .unwrap()
            .to_ascii_lowercase();
        (status, head)
    }

    const JSON: [&str; 2] = [
        "Content-Type: application/json",
        "Accept: application/json, text/event-stream",
    ];

    #[test]
    fn app_loopback_requires_a_loopback_bind_and_no_origins() {
        assert!(Endpoint::bind(Config::app_loopback("0.0.0.0:0".parse().unwrap())).is_err());
        let mut config = Config::app_loopback("127.0.0.1:0".parse().unwrap());
        config.allowed_origins.push("https://example.test".into());
        assert!(Endpoint::bind(config).is_err());
        let mut config = Config::app_loopback("127.0.0.1:0".parse().unwrap());
        config.bind = "0.0.0.0:0".parse().unwrap();
        assert!(Endpoint::bind(config).is_err());
        assert!(Endpoint::bind(Config::app_loopback("127.0.0.1:0".parse().unwrap())).is_ok());
    }

    #[test]
    fn app_loopback_serves_local_clients_without_a_credential() {
        let (addr, stop, thread) = spawn(Config::app_loopback("127.0.0.1:0".parse().unwrap()));
        let port = addr.port();
        let good = format!("Host: {addr}");
        let named = format!("Host: LocalHost:{port}");
        for (target, host) in [
            ("/".to_owned(), good.clone()),
            ("/".to_owned(), named),
            (format!("http://{addr}/"), good.clone()),
        ] {
            let (status, head) = exchange(addr, &target, &[&host, JSON[0], JSON[1]], INIT);
            assert_eq!(status, 200, "{target} {host}");
            assert!(head.contains("mcp-session-id: "));
            assert!(!head.contains("access-control-"));
        }
        drop(stop);
        thread.join().unwrap();
    }

    #[test]
    fn app_loopback_refuses_browser_shaped_and_misdirected_requests() {
        let (addr, stop, thread) = spawn(Config::app_loopback("127.0.0.1:0".parse().unwrap()));
        let port = addr.port();
        let good = format!("Host: {addr}");
        let refused: Vec<(String, Vec<String>, u16)> = vec![
            ("/".into(), vec![good.clone(), "Origin: null".into()], 403),
            (
                "/".into(),
                vec![good.clone(), "Origin: http://example.test".into()],
                403,
            ),
            (
                "/".into(),
                vec![good.clone(), format!("Origin: http://{addr}")],
                403,
            ),
            ("/".into(), vec![], 403),
            ("/".into(), vec![good.clone(), good.clone()], 403),
            (
                "/".into(),
                vec![format!("Host: 127.0.0.1:{}", port.wrapping_add(1))],
                403,
            ),
            ("/".into(), vec!["Host: 127.0.0.1".into()], 403),
            ("/".into(), vec![format!("Host: example.test:{port}")], 403),
            ("/".into(), vec![format!("Host: localhost.:{port}")], 403),
            ("/".into(), vec![format!("Host: [::1]:{port}")], 403),
            (
                format!("http://example.test:{port}/"),
                vec![good.clone()],
                403,
            ),
            (format!("http://localhost:{port}/"), vec![good.clone()], 403),
            (
                "/".into(),
                vec![
                    good.clone(),
                    "Content-Type: text/plain".into(),
                    JSON[1].into(),
                ],
                415,
            ),
            (
                "/".into(),
                vec![good.clone(), JSON[0].into(), "Accept: */*".into()],
                406,
            ),
        ];
        for (target, mut headers, expected) in refused {
            if expected == 403 {
                headers.extend(JSON.iter().map(|h| h.to_string()));
            }
            let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
            let (status, head) = exchange(addr, &target, &headers, INIT);
            assert_eq!(status, expected, "{target} {headers:?}");
            assert!(!head.contains("mcp-session-id"), "{target} {headers:?}");
            assert!(!head.contains("access-control-"));
        }
        drop(stop);
        thread.join().unwrap();
    }

    #[test]
    fn bearer_endpoint_still_requires_its_token() {
        let mut config = Config::new(
            "127.0.0.1:0".parse().unwrap(),
            BearerToken::new(TOKEN).unwrap(),
        );
        config.allowed_origins.push("https://edge.example".into());
        let (addr, stop, thread) = spawn(config);
        let host = format!("Host: {addr}");
        let bearer = format!("Authorization: Bearer {TOKEN}");
        let (status, head) = exchange(addr, "/", &[&host, JSON[0], JSON[1]], INIT);
        assert_eq!(status, 401);
        assert!(head.contains("www-authenticate: bearer"));
        let wrong = "Authorization: Bearer wrong-token-0123456789abcdef0123456789";
        assert_eq!(
            exchange(addr, "/", &[&host, wrong, JSON[0], JSON[1]], INIT).0,
            401
        );
        let origin = "Origin: http://example.test";
        assert_eq!(
            exchange(addr, "/", &[&host, &bearer, origin, JSON[0], JSON[1]], INIT).0,
            403
        );
        assert_eq!(
            exchange(addr, "/", &[&host, &bearer, JSON[0], JSON[1]], INIT).0,
            200
        );
        let allowed = "Origin: https://edge.example";
        assert_eq!(
            exchange(
                addr,
                "/",
                &[&host, &bearer, allowed, JSON[0], JSON[1]],
                INIT
            )
            .0,
            200
        );
        assert_eq!(
            exchange(addr, "/", &[&host, allowed, JSON[0], JSON[1]], INIT).0,
            401
        );
        // Bearer endpoints keep their unrestricted Host (a hosting edge forwards).
        assert_eq!(
            exchange(
                addr,
                "/",
                &["Host: edge.example:443", &bearer, JSON[0], JSON[1]],
                INIT
            )
            .0,
            200
        );
        drop(stop);
        thread.join().unwrap();
    }

    #[test]
    fn stdio_still_rejects_embedded_carriage_returns_at_its_framing_boundary() {
        let mut output = Vec::new();
        let input = std::io::Cursor::new(b"{\r\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
        assert!(super::super::serve(&[], Limits::default(), input, &mut output).is_err());
        assert!(output.is_empty());
    }
}
