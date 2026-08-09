use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use agent_client_protocol::schema::v1::{HttpHeader, McpServer, McpServerHttp};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, ORIGIN};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::agent_error::AgentStartupFailure;
#[cfg(feature = "agent-test-support")]
use crate::agent_launch::TestOpeningGate;

const MCP_REVISION: &str = "2025-11-25";
const MCP_PATH: &str = "/mcp";
const RESULT_TOOL: &str = "troupe_submit_result";
const MAX_CONNECTIONS: usize = 65_536;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum ServiceState {
    Unstarted,
    Starting,
    Ready { endpoint: String, origin: String },
    Failed(AgentStartupFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RoutePhase {
    New,
    InitializeWriting,
    Initialized,
    InitializedNotificationWriting,
    ClientInitialized,
    ToolsListWriting,
    ReadyPending,
    Ready,
    Revoked,
}

#[cfg(feature = "agent-test-support")]
impl RoutePhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::InitializeWriting => "initialize_writing",
            Self::Initialized => "initialized",
            Self::InitializedNotificationWriting => "initialized_notification_writing",
            Self::ClientInitialized => "client_initialized",
            Self::ToolsListWriting => "tools_list_writing",
            Self::ReadyPending => "ready_pending",
            Self::Ready => "ready",
            Self::Revoked => "revoked",
        }
    }
}

pub(crate) struct ResultRoute {
    token: String,
    pub(crate) server_name: String,
    pub(crate) generation: u64,
    endpoint: String,
    phase: Mutex<RoutePhase>,
    changed: Notify,
    connection_cancellation: CancellationToken,
    connections: Mutex<usize>,
    connections_changed: Notify,
    #[cfg(feature = "agent-test-support")]
    ready_gate: Option<Arc<TestOpeningGate>>,
}

impl ResultRoute {
    pub(crate) fn mcp_server(&self) -> McpServer {
        McpServer::Http(
            McpServerHttp::new(self.server_name.clone(), self.endpoint.clone()).headers(vec![
                HttpHeader::new("Authorization", format!("Bearer {}", self.token)),
            ]),
        )
    }

    pub(crate) async fn wait_ready(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), AgentStartupFailure> {
        loop {
            let changed = self.changed.notified();
            match *lock(&self.phase) {
                RoutePhase::Ready => return Ok(()),
                RoutePhase::Revoked => {
                    return Err(AgentStartupFailure::start(
                        "result_channel_unavailable",
                        "mcp_ready",
                        "agent result channel became unavailable",
                    ));
                }
                RoutePhase::New
                | RoutePhase::InitializeWriting
                | RoutePhase::Initialized
                | RoutePhase::InitializedNotificationWriting
                | RoutePhase::ClientInitialized
                | RoutePhase::ToolsListWriting
                | RoutePhase::ReadyPending => {}
            }
            tokio::select! {
                () = changed => {}
                () = cancellation.cancelled() => {
                    return Err(AgentStartupFailure::start(
                        "result_channel_unavailable",
                        "mcp_ready",
                        "agent result channel was closed",
                    ));
                }
            }
        }
    }

    fn revoke(&self) {
        *lock(&self.phase) = RoutePhase::Revoked;
        self.connection_cancellation.cancel();
        self.changed.notify_waiters();
        #[cfg(feature = "agent-test-support")]
        if let Some(gate) = &self.ready_gate {
            gate.release();
        }
    }

    fn publish_ready(self: &Arc<Self>) {
        #[cfg(feature = "agent-test-support")]
        if let Some(gate) = self.ready_gate.as_ref().map(Arc::clone) {
            let route = Arc::clone(self);
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                gate.wait().await;
                route.commit_ready();
                gate.mark_completed();
            });
            return;
        }
        self.commit_ready();
    }

    fn commit_ready(&self) {
        let mut phase = lock(&self.phase);
        if *phase == RoutePhase::ReadyPending {
            *phase = RoutePhase::Ready;
            self.changed.notify_waiters();
        }
    }

    fn acquire_connection(self: &Arc<Self>) -> Option<RouteConnectionLease> {
        let phase = lock(&self.phase);
        if *phase == RoutePhase::Revoked {
            return None;
        }
        *lock(&self.connections) += 1;
        drop(phase);
        Some(RouteConnectionLease {
            route: Arc::clone(self),
        })
    }

    async fn wait_connections_closed(&self) {
        loop {
            let changed = self.connections_changed.notified();
            if *lock(&self.connections) == 0 {
                return;
            }
            changed.await;
        }
    }

    async fn wait_stable_phase(&self) -> Option<RoutePhase> {
        loop {
            let changed = self.changed.notified();
            let phase = *lock(&self.phase);
            match phase {
                RoutePhase::InitializeWriting
                | RoutePhase::InitializedNotificationWriting
                | RoutePhase::ToolsListWriting => changed.await,
                RoutePhase::Revoked => return None,
                RoutePhase::New
                | RoutePhase::Initialized
                | RoutePhase::ClientInitialized
                | RoutePhase::ReadyPending
                | RoutePhase::Ready => return Some(phase),
            }
        }
    }
}

#[cfg(any(test, feature = "agent-test-support"))]
fn route_for_test(token: &str, generation: u64) -> Arc<ResultRoute> {
    Arc::new(ResultRoute {
        token: token.to_owned(),
        server_name: "test-server".to_owned(),
        generation,
        endpoint: "http://127.0.0.1:1/mcp".to_owned(),
        phase: Mutex::new(RoutePhase::New),
        changed: Notify::new(),
        connection_cancellation: CancellationToken::new(),
        connections: Mutex::new(0),
        connections_changed: Notify::new(),
        #[cfg(feature = "agent-test-support")]
        ready_gate: None,
    })
}

struct RouteConnectionLease {
    route: Arc<ResultRoute>,
}

impl Drop for RouteConnectionLease {
    fn drop(&mut self) {
        let mut connections = lock(&self.route.connections);
        *connections = connections
            .checked_sub(1)
            .expect("a route connection lease is released once");
        if *connections == 0 {
            self.route.connections_changed.notify_waiters();
        }
    }
}

struct ConnectionControl {
    route: Mutex<Option<RouteConnectionLease>>,
    transition: Mutex<Option<ResponseTransition>>,
    route_bound: Notify,
}

impl ConnectionControl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            route: Mutex::new(None),
            transition: Mutex::new(None),
            route_bound: Notify::new(),
        })
    }

    fn bind_route(&self, route: &Arc<ResultRoute>) -> bool {
        let mut bound = lock(&self.route);
        if let Some(lease) = bound.as_ref() {
            return Arc::ptr_eq(&lease.route, route);
        }
        let Some(lease) = route.acquire_connection() else {
            return false;
        };
        *bound = Some(lease);
        drop(bound);
        self.route_bound.notify_waiters();
        true
    }

    fn assert_request_can_begin(&self) {
        assert!(
            lock(&self.transition).is_none(),
            "Hyper must flush the previous HTTP/1.1 response before dispatching another request"
        );
    }

    fn set_transition(&self, transition: Option<ResponseTransition>) {
        let previous = std::mem::replace(&mut *lock(&self.transition), transition);
        assert!(
            previous.is_none(),
            "an HTTP connection carries one in-flight response"
        );
    }

    async fn route_cancelled(&self) {
        loop {
            let bound = self.route_bound.notified();
            let cancellation = lock(&self.route)
                .as_ref()
                .map(|lease| lease.route.connection_cancellation.clone());
            if let Some(cancellation) = cancellation {
                cancellation.cancelled().await;
                return;
            }
            bound.await;
        }
    }

    fn finish(&self, response_written: bool) {
        if let Some(transition) = lock(&self.transition).take() {
            transition.finish(response_written);
        }
        lock(&self.route).take();
    }

    fn response_flushed(&self) {
        if let Some(transition) = lock(&self.transition).take() {
            transition.finish(true);
        }
    }

    fn response_flush_failed(&self) {
        if let Some(transition) = lock(&self.transition).take() {
            transition.finish(false);
        }
    }
}

struct ResponseTrackedIo<T> {
    inner: T,
    control: Arc<ConnectionControl>,
}

impl<T> ResponseTrackedIo<T> {
    fn new(inner: T, control: Arc<ConnectionControl>) -> Self {
        Self { inner, control }
    }
}

impl<T> tokio::io::AsyncRead for ResponseTrackedIo<T>
where
    T: tokio::io::AsyncRead + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl<T> tokio::io::AsyncWrite for ResponseTrackedIo<T>
where
    T: tokio::io::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match std::pin::Pin::new(&mut self.inner).poll_flush(cx) {
            std::task::Poll::Ready(Ok(())) => {
                self.control.response_flushed();
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(error)) => {
                self.control.response_flush_failed();
                std::task::Poll::Ready(Err(error))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write_vectored(cx, buffers)
    }
}

async fn complete_connection<F, E>(
    connection: F,
    control: Arc<ConnectionControl>,
    shutdown: &CancellationToken,
) where
    F: Future<Output = Result<(), E>>,
{
    let response_written = {
        tokio::pin!(connection);
        tokio::select! {
            result = &mut connection => result.is_ok(),
            () = control.route_cancelled() => false,
            () = shutdown.cancelled() => false,
        }
    };
    control.finish(response_written);
}

pub(crate) struct ResultMcpService {
    state: Mutex<ServiceState>,
    changed: Notify,
    routes: Mutex<HashMap<String, Arc<ResultRoute>>>,
    shutdown: CancellationToken,
    connections: Arc<Semaphore>,
    accept_complete: AtomicBool,
    accept_changed: Notify,
    connection_tasks: AtomicUsize,
    connection_tasks_changed: Notify,
}

struct ConnectionTaskLease {
    service: Arc<ResultMcpService>,
}

impl Drop for ConnectionTaskLease {
    fn drop(&mut self) {
        let previous = self.service.connection_tasks.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "an accepted connection task is released once");
        if previous == 1 {
            self.service.connection_tasks_changed.notify_waiters();
        }
    }
}

impl ResultMcpService {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ServiceState::Unstarted),
            changed: Notify::new(),
            routes: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            accept_complete: AtomicBool::new(false),
            accept_changed: Notify::new(),
            connection_tasks: AtomicUsize::new(0),
            connection_tasks_changed: Notify::new(),
        })
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub(crate) async fn shutdown_and_wait(&self) {
        self.shutdown();
        loop {
            let changed = self.accept_changed.notified();
            let accept_started = matches!(*lock(&self.state), ServiceState::Ready { .. });
            if !accept_started || self.accept_complete.load(Ordering::Acquire) {
                break;
            }
            changed.await;
        }
        loop {
            let changed = self.connection_tasks_changed.notified();
            if self.connection_tasks.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }

    fn begin_connection_task(self: &Arc<Self>) -> ConnectionTaskLease {
        self.connection_tasks.fetch_add(1, Ordering::AcqRel);
        ConnectionTaskLease {
            service: Arc::clone(self),
        }
    }

    pub(crate) async fn ensure_ready(self: &Arc<Self>) -> Result<String, AgentStartupFailure> {
        loop {
            let changed = self.changed.notified();
            let initialize = {
                let mut state = lock(&self.state);
                match &*state {
                    ServiceState::Ready { endpoint, .. } => return Ok(endpoint.clone()),
                    ServiceState::Failed(failure) => return Err(failure.clone()),
                    ServiceState::Starting => false,
                    ServiceState::Unstarted => {
                        *state = ServiceState::Starting;
                        true
                    }
                }
            };
            if !initialize {
                changed.await;
                continue;
            }

            let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await {
                Ok(listener) => listener,
                Err(_) => {
                    let failure = AgentStartupFailure::start(
                        "result_channel_unavailable",
                        "preparation",
                        "agent result service could not start",
                    );
                    *lock(&self.state) = ServiceState::Failed(failure.clone());
                    self.changed.notify_waiters();
                    return Err(failure);
                }
            };
            let address = match listener.local_addr() {
                Ok(address) => address,
                Err(_) => {
                    let failure = AgentStartupFailure::start(
                        "result_channel_unavailable",
                        "preparation",
                        "agent result service address is unavailable",
                    );
                    *lock(&self.state) = ServiceState::Failed(failure.clone());
                    self.changed.notify_waiters();
                    return Err(failure);
                }
            };
            let origin = format!("http://127.0.0.1:{}", address.port());
            let endpoint = format!("{origin}{MCP_PATH}");
            *lock(&self.state) = ServiceState::Ready {
                endpoint: endpoint.clone(),
                origin,
            };
            self.changed.notify_waiters();
            let service = Arc::clone(self);
            let completion = Arc::clone(self);
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                service.accept(listener).await;
                completion.accept_complete.store(true, Ordering::Release);
                completion.accept_changed.notify_waiters();
            });
            return Ok(endpoint);
        }
    }

    pub(crate) fn register_route(
        &self,
        generation: u64,
        #[cfg(feature = "agent-test-support")] ready_gate: Option<Arc<TestOpeningGate>>,
    ) -> Result<Arc<ResultRoute>, AgentStartupFailure> {
        let endpoint = match &*lock(&self.state) {
            ServiceState::Ready { endpoint, .. } => endpoint.clone(),
            _ => {
                return Err(AgentStartupFailure::start(
                    "result_channel_unavailable",
                    "session_new",
                    "agent result service is not ready",
                ));
            }
        };
        let mut random = [0_u8; 44];
        getrandom::fill(&mut random).map_err(|_| {
            AgentStartupFailure::start(
                "preparation_failed",
                "session_new",
                "agent result route could not be created",
            )
        })?;
        let token = URL_SAFE_NO_PAD.encode(&random[..32]);
        let suffix = URL_SAFE_NO_PAD.encode(&random[32..]);
        let route = Arc::new(ResultRoute {
            token: token.clone(),
            server_name: format!("troupe-result-{suffix}"),
            generation,
            endpoint,
            phase: Mutex::new(RoutePhase::New),
            changed: Notify::new(),
            connection_cancellation: CancellationToken::new(),
            connections: Mutex::new(0),
            connections_changed: Notify::new(),
            #[cfg(feature = "agent-test-support")]
            ready_gate,
        });
        lock(&self.routes).insert(token, Arc::clone(&route));
        Ok(route)
    }

    pub(crate) async fn revoke_route(&self, route: &ResultRoute) {
        {
            let mut routes = lock(&self.routes);
            route.revoke();
            routes.remove(&route.token);
        }
        route.wait_connections_closed().await;
    }

    async fn accept(self: Arc<Self>, listener: TcpListener) {
        loop {
            let accepted = tokio::select! {
                () = self.shutdown.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, address)) = accepted else {
                break;
            };
            if !address.ip().is_loopback() {
                continue;
            }
            let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                continue;
            };
            let service = Arc::clone(&self);
            let connection_task = service.begin_connection_task();
            let control = ConnectionControl::new();
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                let io = TokioIo::new(ResponseTrackedIo::new(stream, Arc::clone(&control)));
                let service_for_request = Arc::clone(&service);
                let control_for_request = Arc::clone(&control);
                let connection = http1::Builder::new().serve_connection(
                    io,
                    service_fn(move |request| {
                        let service = Arc::clone(&service_for_request);
                        let control = Arc::clone(&control_for_request);
                        async move { service.handle(request, control).await }
                    }),
                );
                complete_connection(connection, control, &service.shutdown).await;
                drop(permit);
                drop(connection_task);
            });
        }
    }

    async fn handle(
        &self,
        request: Request<Incoming>,
        control: Arc<ConnectionControl>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        if request.version() != Version::HTTP_11 {
            return Ok(empty(StatusCode::HTTP_VERSION_NOT_SUPPORTED));
        }
        control.assert_request_can_begin();
        let outcome = self.handle_inner(request, &control).await;
        control.set_transition(outcome.transition);
        let response = outcome.response;
        Ok(response)
    }

    async fn handle_inner(
        &self,
        request: Request<Incoming>,
        control: &ConnectionControl,
    ) -> DispatchOutcome {
        if request.uri().path() != MCP_PATH {
            return DispatchOutcome::plain(empty(StatusCode::NOT_FOUND));
        }
        if request.method() != Method::POST {
            return DispatchOutcome::plain(empty(StatusCode::METHOD_NOT_ALLOWED));
        }
        if !common_transport_headers_valid(self, request.headers()) {
            return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
        }
        let Some(route) = self.bind_authorized_route(request.headers(), control) else {
            return DispatchOutcome::plain(empty(StatusCode::UNAUTHORIZED));
        };
        let Some(phase) = route.wait_stable_phase().await else {
            return DispatchOutcome::plain(empty(StatusCode::GONE));
        };
        if !protocol_header_valid(request.headers(), phase) {
            return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
        }
        let body = match request.into_body().collect().await {
            Ok(body) => body.to_bytes(),
            Err(_) => return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST)),
        };
        let message: Value = match serde_json::from_slice(&body) {
            Ok(message) => message,
            Err(_) => return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST)),
        };
        dispatch(&route, message)
    }

    fn bind_authorized_route(
        &self,
        headers: &hyper::HeaderMap,
        control: &ConnectionControl,
    ) -> Option<Arc<ResultRoute>> {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let value = values.next()?.to_str().ok()?;
        if values.next().is_some() {
            return None;
        }
        let token = value.strip_prefix("Bearer ")?;
        let routes = lock(&self.routes);
        let route = routes.get(token).map(Arc::clone)?;
        if !control.bind_route(&route) {
            return None;
        }
        Some(route)
    }
}

impl Drop for ResultMcpService {
    fn drop(&mut self) {
        self.shutdown.cancel();
        let routes: Vec<_> = lock(&self.routes).drain().map(|(_, route)| route).collect();
        for route in routes {
            route.revoke();
        }
    }
}

fn common_transport_headers_valid(service: &ResultMcpService, headers: &hyper::HeaderMap) -> bool {
    let mut content_types = headers.get_all(CONTENT_TYPE).iter();
    let content_type = content_types.next().and_then(|value| value.to_str().ok());
    if content_types.next().is_some()
        || !content_type.is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return false;
    }
    let mut accepts = headers.get_all(ACCEPT).iter();
    let Some(accept) = accepts.next().and_then(|value| value.to_str().ok()) else {
        return false;
    };
    if accepts.next().is_some() {
        return false;
    }
    let accepted: Vec<_> = accept
        .split(',')
        .map(|value| value.split(';').next().unwrap_or_default().trim())
        .collect();
    if !accepted.contains(&"application/json") || !accepted.contains(&"text/event-stream") {
        return false;
    }
    if headers.contains_key("mcp-session-id") {
        return false;
    }
    if let Some(origin) = headers.get(ORIGIN) {
        let expected = match &*lock(&service.state) {
            ServiceState::Ready { origin, .. } => origin.clone(),
            _ => return false,
        };
        if origin.to_str().ok() != Some(&expected) {
            return false;
        }
    }
    true
}

fn protocol_header_valid(headers: &hyper::HeaderMap, phase: RoutePhase) -> bool {
    let versions: Vec<_> = headers.get_all("mcp-protocol-version").iter().collect();
    match phase {
        RoutePhase::New => versions.is_empty(),
        RoutePhase::Initialized
        | RoutePhase::ClientInitialized
        | RoutePhase::ReadyPending
        | RoutePhase::Ready => {
            versions.len() == 1 && versions[0].to_str().ok() == Some(MCP_REVISION)
        }
        RoutePhase::InitializeWriting
        | RoutePhase::InitializedNotificationWriting
        | RoutePhase::ToolsListWriting
        | RoutePhase::Revoked => false,
    }
}

struct DispatchOutcome {
    response: Response<Full<Bytes>>,
    transition: Option<ResponseTransition>,
}

impl DispatchOutcome {
    fn plain(response: Response<Full<Bytes>>) -> Self {
        Self {
            response,
            transition: None,
        }
    }
}

struct ResponseTransition {
    route: Arc<ResultRoute>,
    writing: RoutePhase,
    previous: RoutePhase,
    success: RoutePhase,
    finished: bool,
}

impl ResponseTransition {
    fn finish(mut self, response_written: bool) {
        self.apply(response_written);
        self.finished = true;
    }

    fn apply(&self, response_written: bool) {
        let mut phase = lock(&self.route.phase);
        if *phase != self.writing {
            return;
        }
        *phase = if response_written {
            self.success
        } else {
            self.previous
        };
        let publish_ready = response_written && self.success == RoutePhase::ReadyPending;
        self.route.changed.notify_waiters();
        drop(phase);
        if publish_ready {
            self.route.publish_ready();
        }
    }
}

impl Drop for ResponseTransition {
    fn drop(&mut self) {
        if !self.finished {
            self.apply(false);
        }
    }
}

fn dispatch(route: &Arc<ResultRoute>, message: Value) -> DispatchOutcome {
    if message.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST));
    }
    let method = message.get("method").and_then(Value::as_str);
    let request_id = message.get("id").cloned();
    let mut phase = lock(&route.phase);
    match (method, *phase, request_id) {
        (Some("initialize"), RoutePhase::New, Some(id)) => {
            let revision = message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str);
            if revision != Some(MCP_REVISION) {
                return DispatchOutcome::plain(json_rpc_error(
                    id,
                    -32602,
                    "unsupported protocol version",
                ));
            }
            *phase = RoutePhase::InitializeWriting;
            let response = json_response(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": MCP_REVISION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "troupe", "version": env!("CARGO_PKG_VERSION")}
                }
            }));
            drop(phase);
            DispatchOutcome {
                response,
                transition: Some(ResponseTransition {
                    route: Arc::clone(route),
                    writing: RoutePhase::InitializeWriting,
                    previous: RoutePhase::New,
                    success: RoutePhase::Initialized,
                    finished: false,
                }),
            }
        }
        (Some("notifications/initialized"), RoutePhase::Initialized, None) => {
            *phase = RoutePhase::InitializedNotificationWriting;
            drop(phase);
            DispatchOutcome {
                response: empty(StatusCode::ACCEPTED),
                transition: Some(ResponseTransition {
                    route: Arc::clone(route),
                    writing: RoutePhase::InitializedNotificationWriting,
                    previous: RoutePhase::Initialized,
                    success: RoutePhase::ClientInitialized,
                    finished: false,
                }),
            }
        }
        (Some("tools/list"), RoutePhase::ClientInitialized, Some(id)) => {
            *phase = RoutePhase::ToolsListWriting;
            let response = json_response(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": RESULT_TOOL,
                        "description": "Submit the structured result for the current Actor turn.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"value": {}},
                            "required": ["value"],
                            "additionalProperties": false
                        }
                    }]
                }
            }));
            drop(phase);
            DispatchOutcome {
                response,
                transition: Some(ResponseTransition {
                    route: Arc::clone(route),
                    writing: RoutePhase::ToolsListWriting,
                    previous: RoutePhase::ClientInitialized,
                    success: RoutePhase::ReadyPending,
                    finished: false,
                }),
            }
        }
        _ => match message.get("id").cloned() {
            Some(id) => {
                DispatchOutcome::plain(json_rpc_error(id, -32600, "invalid MCP lifecycle request"))
            }
            None => DispatchOutcome::plain(empty(StatusCode::BAD_REQUEST)),
        },
    }
}

fn json_rpc_error(id: Value, code: i32, message: &str) -> Response<Full<Bytes>> {
    json_response(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    }))
}

fn json_response(value: Value) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(
        serde_json::to_vec(&value).expect("JSON-RPC values must serialize"),
    )));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = status;
    response
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_result_generation_isolation")]
pub(crate) fn result_generation_isolation_for_test(
    py: pyo3::Python<'_>,
) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
    use pyo3::types::{PyDict, PyDictMethods as _};

    let service = ResultMcpService::new();
    let old = route_for_test("old-token", 1);
    let successor = route_for_test("successor-token", 2);
    lock(&service.routes).insert(old.token.clone(), Arc::clone(&old));
    let stale_transition = dispatch(
        &old,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": MCP_REVISION}
        }),
    )
    .transition
    .expect("initialize creates a lifecycle transition");
    {
        let mut routes = lock(&service.routes);
        old.revoke();
        routes.remove(&old.token);
        routes.insert(successor.token.clone(), Arc::clone(&successor));
    }

    let mut stale_headers = hyper::HeaderMap::new();
    stale_headers.insert(
        AUTHORIZATION,
        hyper::header::HeaderValue::from_static("Bearer old-token"),
    );
    let stale_control = ConnectionControl::new();
    let stale_bearer_bound = service
        .bind_authorized_route(&stale_headers, &stale_control)
        .is_some();
    stale_transition.finish(true);

    let mut successor_headers = hyper::HeaderMap::new();
    successor_headers.insert(
        AUTHORIZATION,
        hyper::header::HeaderValue::from_static("Bearer successor-token"),
    );
    let successor_control = ConnectionControl::new();
    let successor_bearer_bound = service
        .bind_authorized_route(&successor_headers, &successor_control)
        .is_some();
    drop(successor_control);

    let snapshot = PyDict::new(py);
    snapshot.set_item("old_generation", old.generation)?;
    snapshot.set_item("old_phase", lock(&old.phase).as_str())?;
    snapshot.set_item("stale_bearer_bound", stale_bearer_bound)?;
    snapshot.set_item("successor_generation", successor.generation)?;
    snapshot.set_item("successor_phase", lock(&successor.phase).as_str())?;
    snapshot.set_item("successor_bearer_bound", successor_bearer_bound)?;
    snapshot.set_item(
        "successor_connections_after_release",
        *lock(&successor.connections),
    )?;
    Ok(snapshot.into_any().unbind())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll};

    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    struct PendingConnection {
        route: Arc<ResultRoute>,
        dropped: Arc<AtomicBool>,
    }

    struct FailFlushIo<T>(T);

    impl<T> tokio::io::AsyncRead for FailFlushIo<T>
    where
        T: tokio::io::AsyncRead + Unpin,
    {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buffer)
        }
    }

    impl<T> tokio::io::AsyncWrite for FailFlushIo<T>
    where
        T: tokio::io::AsyncWrite + Unpin,
    {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buffer)
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected response flush failure",
            )))
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl Future for PendingConnection {
        type Output = Result<(), ()>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingConnection {
        fn drop(&mut self) {
            assert_eq!(
                *lock(&self.route.connections),
                1,
                "the route lease must outlive the connection future"
            );
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn route_with(token: &str, generation: u64) -> Arc<ResultRoute> {
        route_for_test(token, generation)
    }

    fn route() -> Arc<ResultRoute> {
        route_with("test-token", 1)
    }

    fn initialize() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": MCP_REVISION}
        })
    }

    fn complete_http_response_length(response: &[u8]) -> Option<usize> {
        let header_end = response.windows(4).position(|part| part == b"\r\n\r\n")?;
        let headers = std::str::from_utf8(&response[..header_end]).ok()?;
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })?;
        Some(header_end + 4 + content_length)
    }

    #[tokio::test]
    async fn lifecycle_transition_commits_when_response_flush_completes() {
        let service = ResultMcpService::new();
        let route = route();
        lock(&service.routes).insert(route.token.clone(), Arc::clone(&route));
        let control = ConnectionControl::new();
        let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
        let io = TokioIo::new(ResponseTrackedIo::new(server_io, Arc::clone(&control)));
        let service_for_request = Arc::clone(&service);
        let control_for_request = Arc::clone(&control);
        let connection = http1::Builder::new().serve_connection(
            io,
            service_fn(move |request| {
                let service = Arc::clone(&service_for_request);
                let control = Arc::clone(&control_for_request);
                async move { service.handle(request, control).await }
            }),
        );
        let server = tokio::spawn({
            let service = Arc::clone(&service);
            let control = Arc::clone(&control);
            async move { complete_connection(connection, control, &service.shutdown).await }
        });

        let body = serde_json::to_vec(&initialize()).expect("initialize serializes");
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        client_io
            .write_all(request.as_bytes())
            .await
            .expect("request headers are written");
        client_io
            .write_all(&body)
            .await
            .expect("request body is written");

        let mut response = Vec::new();
        loop {
            if complete_http_response_length(&response)
                .is_some_and(|length| response.len() >= length)
            {
                break;
            }
            let read = client_io
                .read_buf(&mut response)
                .await
                .expect("response is readable");
            assert_ne!(read, 0, "connection stays live through the response");
        }

        assert_eq!(*lock(&route.phase), RoutePhase::Initialized);
        drop(client_io);
        server.await.expect("server task completes");
    }

    #[tokio::test]
    async fn failed_response_flush_reverts_before_a_pipelined_request_is_dispatched() {
        let service = ResultMcpService::new();
        let route = route();
        lock(&service.routes).insert(route.token.clone(), Arc::clone(&route));
        let control = ConnectionControl::new();
        let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
        let io = TokioIo::new(ResponseTrackedIo::new(
            FailFlushIo(server_io),
            Arc::clone(&control),
        ));
        let service_for_request = Arc::clone(&service);
        let control_for_request = Arc::clone(&control);
        let connection = http1::Builder::new().serve_connection(
            io,
            service_fn(move |request| {
                let service = Arc::clone(&service_for_request);
                let control = Arc::clone(&control_for_request);
                async move { service.handle(request, control).await }
            }),
        );
        let server = tokio::spawn({
            let service = Arc::clone(&service);
            let control = Arc::clone(&control);
            async move { complete_connection(connection, control, &service.shutdown).await }
        });

        let initialize_body = serde_json::to_vec(&initialize()).expect("initialize serializes");
        let initialized_body =
            br#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        let requests = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nMCP-Protocol-Version: {}\r\nContent-Length: {}\r\n\r\n{}",
            initialize_body.len(),
            std::str::from_utf8(&initialize_body).expect("JSON is UTF-8"),
            MCP_REVISION,
            initialized_body.len(),
            std::str::from_utf8(initialized_body).expect("JSON is UTF-8"),
        );
        client_io
            .write_all(requests.as_bytes())
            .await
            .expect("pipelined requests fit in the test transport");

        server
            .await
            .expect("flush failure is handled by the server");
        assert_eq!(*lock(&route.phase), RoutePhase::New);
    }

    #[tokio::test]
    async fn auth_route_drain_drops_connection_before_releasing_its_route_lease() {
        let route = route();
        let control = ConnectionControl::new();
        assert!(control.bind_route(&route));
        route.revoke();
        let dropped = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();

        complete_connection(
            PendingConnection {
                route: Arc::clone(&route),
                dropped: Arc::clone(&dropped),
            },
            Arc::clone(&control),
            &shutdown,
        )
        .await;

        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(*lock(&route.connections), 0);
    }

    #[tokio::test]
    async fn shutdown_waits_for_every_accepted_connection_task() {
        let service = ResultMcpService::new();
        let connection_task = service.begin_connection_task();
        let shutdown = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.shutdown_and_wait().await }
        });
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());

        drop(connection_task);

        shutdown.await.expect("shutdown task completes");
    }

    #[tokio::test]
    async fn revoked_generation_cannot_bind_or_complete_its_successor() {
        let service = ResultMcpService::new();
        let old = route_with("old-token", 1);
        let successor = route_with("successor-token", 2);
        lock(&service.routes).insert(old.token.clone(), Arc::clone(&old));
        let stale_transition = dispatch(&old, initialize())
            .transition
            .expect("initialize has a response transition");
        service.revoke_route(&old).await;
        lock(&service.routes).insert(successor.token.clone(), Arc::clone(&successor));

        let mut stale_headers = hyper::HeaderMap::new();
        stale_headers.insert(
            AUTHORIZATION,
            hyper::header::HeaderValue::from_static("Bearer old-token"),
        );
        let stale_control = ConnectionControl::new();
        assert!(
            service
                .bind_authorized_route(&stale_headers, &stale_control)
                .is_none()
        );
        stale_transition.finish(true);
        assert_eq!(*lock(&old.phase), RoutePhase::Revoked);
        assert_eq!(*lock(&successor.phase), RoutePhase::New);
        assert_eq!(*lock(&successor.connections), 0);

        let mut successor_headers = hyper::HeaderMap::new();
        successor_headers.insert(
            AUTHORIZATION,
            hyper::header::HeaderValue::from_static("Bearer successor-token"),
        );
        let successor_control = ConnectionControl::new();
        let bound = service
            .bind_authorized_route(&successor_headers, &successor_control)
            .expect("the successor bearer remains live");
        assert!(Arc::ptr_eq(&bound, &successor));
        assert_eq!(*lock(&successor.connections), 1);
        drop(successor_control);
        assert_eq!(*lock(&successor.connections), 0);
    }

    #[test]
    fn lifecycle_transition_commits_only_after_response_write_success() {
        let route = route();

        let failed = dispatch(&route, initialize());
        assert_eq!(*lock(&route.phase), RoutePhase::InitializeWriting);
        failed
            .transition
            .expect("initialize has a response transition")
            .finish(false);
        assert_eq!(*lock(&route.phase), RoutePhase::New);

        let initialized = dispatch(&route, initialize());
        initialized
            .transition
            .expect("initialize has a response transition")
            .finish(true);
        assert_eq!(*lock(&route.phase), RoutePhase::Initialized);

        let notification = dispatch(
            &route,
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }),
        );
        notification
            .transition
            .expect("initialized notification has a response transition")
            .finish(true);
        assert_eq!(*lock(&route.phase), RoutePhase::ClientInitialized);

        let list = || {
            dispatch(
                &route,
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
            )
        };
        let failed = list();
        assert_eq!(*lock(&route.phase), RoutePhase::ToolsListWriting);
        failed
            .transition
            .expect("tools/list has a response transition")
            .finish(false);
        assert_eq!(*lock(&route.phase), RoutePhase::ClientInitialized);

        list()
            .transition
            .expect("tools/list has a response transition")
            .finish(true);
        assert_eq!(*lock(&route.phase), RoutePhase::Ready);
    }
}
