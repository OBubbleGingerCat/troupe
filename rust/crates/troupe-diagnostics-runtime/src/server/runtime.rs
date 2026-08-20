use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use bytes::Bytes;
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, runtime::Builder, task::JoinSet};
use tokio_util::sync::CancellationToken;
use troupe_diagnostics_core::id::CanonicalUuid;

use crate::registry::{
    model::{BindEndpoint, WebBaseUrl},
    process_identity::ProcessIdentity,
};

use super::{
    error::{
        ServerCoreFailure, ServerCoreFailureCode, ServerShutdownError, ServerStartError,
        ServerStartErrorCode,
    },
    identity::{OperationalLimits, ServerIdentity},
    routes::{RouteDefinition, Router, validate_route_definitions},
    service::handle_request,
};

const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct ServerConfig {
    run_id: CanonicalUuid,
    owner_pid: u32,
    process_identity: ProcessIdentity,
    bind_host: String,
    port: u16,
    advertise_url: Option<WebBaseUrl>,
    operational_limits: OperationalLimits,
}

impl ServerConfig {
    pub fn new(run_id: CanonicalUuid, owner_pid: u32, process_identity: ProcessIdentity) -> Self {
        Self {
            run_id,
            owner_pid,
            process_identity,
            bind_host: "0.0.0.0".to_owned(),
            port: 0,
            advertise_url: None,
            operational_limits: OperationalLimits::default(),
        }
    }

    pub fn with_bind(mut self, host: impl Into<String>, port: u16) -> Self {
        self.bind_host = host.into();
        self.port = port;
        self
    }

    pub fn with_advertise_url(mut self, advertise_url: Option<WebBaseUrl>) -> Self {
        self.advertise_url = advertise_url;
        self
    }

    pub fn with_operational_limits(mut self, operational_limits: OperationalLimits) -> Self {
        self.operational_limits = operational_limits;
        self
    }
}

pub struct DiagnosticServer {
    identity: Arc<ServerIdentity>,
    local_addr: SocketAddr,
    connect_addr: SocketAddr,
    accepted_connections_at_ready: u64,
    shutdown: CancellationToken,
    unexpected_exit: CancellationToken,
    core_failures: Receiver<ServerCoreFailure>,
    thread: Option<JoinHandle<()>>,
}

impl DiagnosticServer {
    pub fn start(
        config: ServerConfig,
        routes: Vec<RouteDefinition>,
    ) -> Result<Self, ServerStartError> {
        validate_config(&config, &routes)?;
        let shutdown = CancellationToken::new();
        let unexpected_exit = CancellationToken::new();
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let (fatal_sender, core_failures) = mpsc::sync_channel(1);
        let ready = Arc::new(AtomicBool::new(false));

        let thread_shutdown = shutdown.clone();
        let thread_unexpected_exit = unexpected_exit.clone();
        let thread_ready = Arc::clone(&ready);
        let thread = thread::Builder::new()
            .name("troupe-diagnostic-http".to_owned())
            .spawn(move || {
                run_execution_context(
                    config,
                    routes,
                    thread_shutdown,
                    thread_unexpected_exit,
                    startup_sender,
                    fatal_sender,
                    thread_ready,
                );
            })
            .map_err(|error| {
                ServerStartError::new(
                    ServerStartErrorCode::ContextSpawnFailed,
                    format!("failed to spawn diagnostic server context: {error}"),
                )
            })?;

        match startup_receiver.recv() {
            Ok(StartupMessage::Ready(ready)) => Ok(Self {
                identity: ready.identity,
                local_addr: ready.local_addr,
                connect_addr: ready.connect_addr,
                accepted_connections_at_ready: ready.accepted_connections,
                shutdown,
                unexpected_exit,
                core_failures,
                thread: Some(thread),
            }),
            Ok(StartupMessage::Failed(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(ServerStartError::new(
                    ServerStartErrorCode::ContextExitedBeforeReady,
                    "diagnostic server context exited before readiness",
                ))
            }
        }
    }

    pub fn identity(&self) -> &ServerIdentity {
        &self.identity
    }

    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub const fn connect_addr(&self) -> SocketAddr {
        self.connect_addr
    }

    pub const fn accepted_connections_at_ready(&self) -> u64 {
        self.accepted_connections_at_ready
    }

    pub fn try_core_failure(&self) -> Option<ServerCoreFailure> {
        self.core_failures.try_recv().ok()
    }

    pub fn wait_for_core_failure(
        &self,
        timeout: Duration,
    ) -> Result<ServerCoreFailure, RecvTimeoutError> {
        self.core_failures.recv_timeout(timeout)
    }

    #[doc(hidden)]
    pub fn trigger_context_exit_for_test(&self) {
        self.unexpected_exit.cancel();
    }

    pub fn shutdown(mut self) -> Result<(), ServerShutdownError> {
        self.shutdown.cancel();
        self.join_context()
    }

    fn join_context(&mut self) -> Result<(), ServerShutdownError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| ServerShutdownError::ExecutionContextPanicked)
    }
}

impl fmt::Debug for DiagnosticServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticServer")
            .field("identity", &self.identity)
            .field("local_addr", &self.local_addr)
            .field("connect_addr", &self.connect_addr)
            .field(
                "accepted_connections_at_ready",
                &self.accepted_connections_at_ready,
            )
            .finish_non_exhaustive()
    }
}

impl Drop for DiagnosticServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        let _ = self.join_context();
    }
}

fn validate_config(
    config: &ServerConfig,
    routes: &[RouteDefinition],
) -> Result<(), ServerStartError> {
    if config.owner_pid == 0 {
        return Err(ServerStartError::new(
            ServerStartErrorCode::InvalidConfiguration,
            "diagnostic server owner PID must be nonzero",
        ));
    }
    BindEndpoint::new(&config.bind_host, 1).map_err(|error| {
        ServerStartError::new(
            ServerStartErrorCode::InvalidConfiguration,
            format!("diagnostic bind host is invalid: {error}"),
        )
    })?;
    validate_route_definitions(routes).map_err(|error| {
        ServerStartError::new(ServerStartErrorCode::InvalidRoutes, error.to_string())
    })
}

#[allow(clippy::too_many_arguments)]
fn run_execution_context(
    config: ServerConfig,
    routes: Vec<RouteDefinition>,
    shutdown: CancellationToken,
    unexpected_exit: CancellationToken,
    startup_sender: SyncSender<StartupMessage>,
    fatal_sender: SyncSender<ServerCoreFailure>,
    ready: Arc<AtomicBool>,
) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                ContextOutcome::StartupFailed(ServerStartError::new(
                    ServerStartErrorCode::ContextInitializationFailed,
                    format!("failed to initialize diagnostic server runtime: {error}"),
                ))
            })?;
        runtime.block_on(run_server(
            config,
            routes,
            shutdown,
            unexpected_exit,
            startup_sender.clone(),
            Arc::clone(&ready),
        ))
    }));

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(ContextOutcome::StartupFailed(error))) => {
            let _ = startup_sender.send(StartupMessage::Failed(error));
        }
        Ok(Err(ContextOutcome::Fatal(failure))) => {
            let _ = fatal_sender.try_send(failure);
        }
        Err(_) if ready.load(Ordering::Acquire) => {
            let _ = fatal_sender.try_send(ServerCoreFailure::new(
                ServerCoreFailureCode::ExecutionContextPanicked,
                "diagnostic server execution context panicked",
            ));
        }
        Err(_) => {
            let _ = startup_sender.send(StartupMessage::Failed(ServerStartError::new(
                ServerStartErrorCode::ContextInitializationFailed,
                "diagnostic server execution context panicked during startup",
            )));
        }
    }
}

async fn run_server(
    config: ServerConfig,
    routes: Vec<RouteDefinition>,
    shutdown: CancellationToken,
    unexpected_exit: CancellationToken,
    startup_sender: SyncSender<StartupMessage>,
    ready: Arc<AtomicBool>,
) -> Result<(), ContextOutcome> {
    let listener = TcpListener::bind((config.bind_host.as_str(), config.port))
        .await
        .map_err(|error| {
            ContextOutcome::StartupFailed(ServerStartError::new(
                ServerStartErrorCode::BindFailed,
                format!("failed to bind diagnostic HTTP listener: {error}"),
            ))
        })?;
    let local_addr = listener.local_addr().map_err(|error| {
        ContextOutcome::StartupFailed(ServerStartError::new(
            ServerStartErrorCode::BindFailed,
            format!("failed to inspect diagnostic HTTP listener: {error}"),
        ))
    })?;
    let bind = BindEndpoint::new(&config.bind_host, local_addr.port()).map_err(|error| {
        ContextOutcome::StartupFailed(ServerStartError::new(
            ServerStartErrorCode::InvalidConfiguration,
            format!("bound diagnostic endpoint is invalid: {error}"),
        ))
    })?;
    let identity = Arc::new(
        ServerIdentity::new(
            config.run_id,
            config.owner_pid,
            config.process_identity,
            bind,
            config.advertise_url,
            config.operational_limits,
        )
        .map_err(|error| {
            ContextOutcome::StartupFailed(ServerStartError::new(
                ServerStartErrorCode::InvalidConfiguration,
                error.to_string(),
            ))
        })?,
    );
    let identity_bytes = Bytes::from(identity.encoded().map_err(|error| {
        ContextOutcome::StartupFailed(ServerStartError::new(
            ServerStartErrorCode::ContextInitializationFailed,
            format!("failed to encode diagnostic server identity: {error}"),
        ))
    })?);
    let router = Arc::new(
        Router::new(&identity, identity_bytes, routes).map_err(|error| {
            ContextOutcome::StartupFailed(ServerStartError::new(
                ServerStartErrorCode::InvalidRoutes,
                error.to_string(),
            ))
        })?,
    );

    let connect_addr = connectable_addr(local_addr);
    complete_readiness_probe(&listener, connect_addr).await?;
    ready.store(true, Ordering::Release);
    if startup_sender
        .send(StartupMessage::Ready(ReadyState {
            identity,
            local_addr,
            connect_addr,
            accepted_connections: 1,
        }))
        .is_err()
    {
        return Ok(());
    }

    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                stop_connections(&mut connections).await;
                return Ok(());
            }
            _ = unexpected_exit.cancelled() => {
                stop_connections(&mut connections).await;
                return Err(ContextOutcome::Fatal(ServerCoreFailure::new(
                    ServerCoreFailureCode::ExecutionContextExited,
                    "diagnostic server execution context exited unexpectedly",
                )));
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|error| {
                    ContextOutcome::Fatal(ServerCoreFailure::new(
                        ServerCoreFailureCode::ListenerFailed,
                        format!("diagnostic HTTP listener failed: {error}"),
                    ))
                })?;
                let router = Arc::clone(&router);
                connections.spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |request| {
                        handle_request(Arc::clone(&router), request)
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                let _ = completed;
            }
        }
    }
}

async fn complete_readiness_probe(
    listener: &TcpListener,
    connect_addr: SocketAddr,
) -> Result<(), ContextOutcome> {
    let connector = tokio::spawn(async move { tokio::net::TcpStream::connect(connect_addr).await });
    let accepted = tokio::time::timeout(READY_PROBE_TIMEOUT, listener.accept())
        .await
        .map_err(|_| readiness_error("listener did not accept its readiness probe"))?
        .map_err(|error| readiness_error(format!("listener readiness accept failed: {error}")))?;
    let connected = tokio::time::timeout(READY_PROBE_TIMEOUT, connector)
        .await
        .map_err(|_| readiness_error("readiness probe connection timed out"))?
        .map_err(|error| readiness_error(format!("readiness probe task failed: {error}")))?
        .map_err(|error| readiness_error(format!("readiness probe connect failed: {error}")))?;
    drop(connected);
    drop(accepted);
    Ok(())
}

fn readiness_error(message: impl Into<String>) -> ContextOutcome {
    ContextOutcome::StartupFailed(ServerStartError::new(
        ServerStartErrorCode::ReadinessProbeFailed,
        message,
    ))
}

async fn stop_connections(connections: &mut JoinSet<()>) {
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

fn connectable_addr(local_addr: SocketAddr) -> SocketAddr {
    match local_addr.ip() {
        IpAddr::V4(address) if address.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_addr.port())
        }
        IpAddr::V6(address) if address.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), local_addr.port())
        }
        _ => local_addr,
    }
}

enum StartupMessage {
    Ready(ReadyState),
    Failed(ServerStartError),
}

struct ReadyState {
    identity: Arc<ServerIdentity>,
    local_addr: SocketAddr,
    connect_addr: SocketAddr,
    accepted_connections: u64,
}

enum ContextOutcome {
    StartupFailed(ServerStartError),
    Fatal(ServerCoreFailure),
}
