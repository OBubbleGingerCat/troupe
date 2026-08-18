#![allow(dead_code)] // D07 wires this foreground command into the application entry point.

use std::{fmt, fs, path::Path, process::Command, time::Duration};

use serde::Serialize;
use tokio::io::AsyncWrite;
use tokio_util::sync::CancellationToken;
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};
use troupe_diagnostics_perfetto::{
    collect::{PERFETTO_EXPORTER_SCHEMA_VERSION, TRACE_CONTENT_WARNING},
    dump::dump_captured_prefix_with_version,
};
use troupe_diagnostics_runtime::{
    query::reader::CapturedEventSource,
    registry::process_identity::current_process_identity,
    server::{
        assembly::ArchiveRouteAssembly,
        dump::{
            CapturedPrefixDumpProducer, DumpEndpoints, DumpProducerError, DumpProducerFuture,
            DumpProducerMetadata,
        },
        query::QueryEndpoints,
        runtime::{DiagnosticServer, ServerConfig},
    },
};

use super::{
    archive_target::ArchiveTarget,
    args::ServeArgs,
    resolver::{ResolvedDiagnosticTarget, ResolverError, resolve},
    target::{DiagnosticTarget, ServeTarget},
};

pub(crate) const ARCHIVE_READY_PREFIX: &str = "troupe: diagnostic archive ready ";
const ARCHIVE_WARNING_PREFIX: &str = "troupe: diagnostic archive warning ";
const LOCATOR_SCHEMA_VERSION: u8 = 1;
const WARNING_SCHEMA_VERSION: u8 = 1;
const LOOPBACK_BIND_HOST: &str = "127.0.0.1";
const SERVER_HEALTH_POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServeErrorCode {
    Resolve,
    ActiveTarget,
    ArchivePath,
    ArchiveRead,
    RouteAssembly,
    ProcessIdentity,
    ServerStart,
    ServerCore,
    ServerShutdown,
    Output,
}

impl ServeErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "diagnostic_serve.resolve",
            Self::ActiveTarget => "diagnostic_serve.active_target",
            Self::ArchivePath => "diagnostic_serve.archive_path",
            Self::ArchiveRead => "diagnostic_serve.archive_read",
            Self::RouteAssembly => "diagnostic_serve.route_assembly",
            Self::ProcessIdentity => "diagnostic_serve.process_identity",
            Self::ServerStart => "diagnostic_serve.server_start",
            Self::ServerCore => "diagnostic_serve.server_core",
            Self::ServerShutdown => "diagnostic_serve.server_shutdown",
            Self::Output => "diagnostic_serve.output",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServeError {
    code: ServeErrorCode,
    detail: String,
}

impl ServeError {
    fn new(code: ServeErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn resolver(error: ResolverError) -> Self {
        Self::new(ServeErrorCode::Resolve, error.to_string())
    }

    fn output(error: impl fmt::Display) -> Self {
        Self::new(ServeErrorCode::Output, error.to_string())
    }

    pub(crate) const fn code(&self) -> ServeErrorCode {
        self.code
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for ServeError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ArchiveServeLocator {
    locator_schema_version: u8,
    run_id: CanonicalUuid,
    local_url: String,
    archive_directory: String,
    clean_shutdown: bool,
}

impl ArchiveServeLocator {
    pub(crate) const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub(crate) fn local_url(&self) -> &str {
        &self.local_url
    }

    pub(crate) fn archive_directory(&self) -> &Path {
        Path::new(&self.archive_directory)
    }

    pub(crate) const fn clean_shutdown(&self) -> bool {
        self.clean_shutdown
    }

    pub(crate) fn ready_line(&self) -> Result<String, ServeError> {
        serde_json::to_string(self)
            .map(|encoded| format!("{ARCHIVE_READY_PREFIX}{encoded}\n"))
            .map_err(|error| {
                ServeError::new(
                    ServeErrorCode::Output,
                    format!("archive locator encoding failed: {error}"),
                )
            })
    }
}

pub(crate) trait ServeOutput {
    type Error: fmt::Display;

    fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error>;
}

pub(crate) trait BrowserLauncher {
    fn launch(&self, url: &str) -> Result<(), String>;
}

struct SystemBrowserLauncher;

impl BrowserLauncher for SystemBrowserLauncher {
    fn launch(&self, url: &str) -> Result<(), String> {
        system_browser_command(url)?
            .spawn()
            .map(|_child| ())
            .map_err(|error| format!("failed to launch the system browser: {error}"))
    }
}

#[cfg(target_os = "linux")]
fn system_browser_command(url: &str) -> Result<Command, String> {
    let mut command = Command::new("xdg-open");
    command.arg(url);
    Ok(command)
}

#[cfg(target_os = "macos")]
fn system_browser_command(url: &str) -> Result<Command, String> {
    let mut command = Command::new("open");
    command.arg(url);
    Ok(command)
}

#[cfg(target_os = "windows")]
fn system_browser_command(url: &str) -> Result<Command, String> {
    let mut command = Command::new("cmd");
    command.args(["/C", "start", "", url]);
    Ok(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn system_browser_command(_url: &str) -> Result<Command, String> {
    Err("the current platform has no supported system browser launcher".to_owned())
}

#[derive(Serialize)]
struct BrowserWarning<'detail> {
    warning_schema_version: u8,
    code: &'static str,
    detail: &'detail str,
}

fn browser_warning_line(detail: &str) -> Result<String, ServeError> {
    let warning = BrowserWarning {
        warning_schema_version: WARNING_SCHEMA_VERSION,
        code: "browser_launch_failed",
        detail,
    };
    serde_json::to_string(&warning)
        .map(|encoded| format!("{ARCHIVE_WARNING_PREFIX}{encoded}\n"))
        .map_err(|error| {
            ServeError::new(
                ServeErrorCode::Output,
                format!("archive warning encoding failed: {error}"),
            )
        })
}

struct PerfettoDumpProducer {
    metadata: DumpProducerMetadata,
}

impl PerfettoDumpProducer {
    fn new() -> Self {
        Self {
            metadata: DumpProducerMetadata::new(
                PERFETTO_EXPORTER_SCHEMA_VERSION,
                env!("CARGO_PKG_VERSION"),
                TRACE_CONTENT_WARNING,
            )
            .expect("built-in Perfetto dump metadata is valid"),
        }
    }
}

impl CapturedPrefixDumpProducer for PerfettoDumpProducer {
    fn metadata(&self) -> &DumpProducerMetadata {
        &self.metadata
    }

    fn dump<'operation>(
        &'operation self,
        source: &'operation CapturedEventSource<'_>,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
        through: Option<SchemaU64>,
    ) -> DumpProducerFuture<'operation> {
        Box::pin(async move {
            dump_captured_prefix_with_version(
                source,
                writer,
                through,
                self.metadata.troupe_version(),
            )
            .await
            .map(|_summary| ())
            .map_err(|error| DumpProducerError::new("perfetto_dump_failed", error.to_string()))
        })
    }
}

pub(crate) struct ArchiveServeSession {
    server: Option<DiagnosticServer>,
    archive: ArchiveTarget,
    locator: ArchiveServeLocator,
}

impl fmt::Debug for ArchiveServeSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArchiveServeSession")
            .field("server", &self.server)
            .field("archive", &self.archive)
            .field("locator", &self.locator)
            .finish_non_exhaustive()
    }
}

impl ArchiveServeSession {
    pub(crate) const fn locator(&self) -> &ArchiveServeLocator {
        &self.locator
    }

    pub(crate) fn shutdown(mut self) -> Result<(), ServeError> {
        self.shutdown_server()
    }

    pub(crate) async fn run_until_cancelled(
        mut self,
        cancellation: &CancellationToken,
    ) -> Result<ServeTermination, ServeError> {
        let core_failure = loop {
            tokio::select! {
                () = cancellation.cancelled() => break None,
                () = tokio::time::sleep(SERVER_HEALTH_POLL) => {
                    let server = self.server.as_ref().expect("a live session owns its server");
                    if let Some(failure) = server.try_core_failure() {
                        break Some(ServeError::new(
                            ServeErrorCode::ServerCore,
                            failure.to_string(),
                        ));
                    }
                }
            }
        };
        let shutdown = self.shutdown_server();
        if let Some(failure) = core_failure {
            return Err(failure);
        }
        shutdown?;
        Ok(ServeTermination::Interrupted)
    }

    #[doc(hidden)]
    pub(crate) fn trigger_server_exit_for_test(&self) {
        self.server
            .as_ref()
            .expect("a live session owns its server")
            .trigger_context_exit_for_test();
    }

    fn shutdown_server(&mut self) -> Result<(), ServeError> {
        let Some(server) = self.server.take() else {
            return Ok(());
        };
        server
            .shutdown()
            .map_err(|error| ServeError::new(ServeErrorCode::ServerShutdown, error.to_string()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServeTermination {
    Interrupted,
}

impl ServeTermination {
    pub(crate) const fn exit_code(self) -> u8 {
        match self {
            Self::Interrupted => 130,
        }
    }
}

pub(crate) async fn execute<O>(
    arguments: ServeArgs,
    output: &mut O,
    cancellation: CancellationToken,
) -> Result<ServeTermination, ServeError>
where
    O: ServeOutput,
{
    execute_with_launcher(arguments, output, cancellation, &SystemBrowserLauncher).await
}

pub(crate) async fn execute_with_launcher<O, B>(
    arguments: ServeArgs,
    output: &mut O,
    cancellation: CancellationToken,
    browser: &B,
) -> Result<ServeTermination, ServeError>
where
    O: ServeOutput,
    B: BrowserLauncher,
{
    let (target, port, open) = arguments.into_parts();
    let archive = tokio::select! {
        () = cancellation.cancelled() => return Ok(ServeTermination::Interrupted),
        target = resolve_archive(target) => target?,
    };
    let session = start_archive(archive, port.get())?;
    let ready_line = session.locator().ready_line()?;
    if let Err(error) = output.write_stderr(&ready_line) {
        let failure = ServeError::output(error);
        let _ = session.shutdown();
        return Err(failure);
    }
    if open && let Err(detail) = browser.launch(session.locator().local_url()) {
        let warning = browser_warning_line(&detail)?;
        if let Err(error) = output.write_stderr(&warning) {
            let failure = ServeError::output(error);
            let _ = session.shutdown();
            return Err(failure);
        }
    }
    session.run_until_cancelled(&cancellation).await
}

async fn resolve_archive(target: ServeTarget) -> Result<ArchiveTarget, ServeError> {
    let target = match target {
        ServeTarget::Production { production, run } => DiagnosticTarget::Production {
            production,
            run: Some(run),
        },
        ServeTarget::Archive(run_directory) => DiagnosticTarget::Archive(run_directory),
    };
    match resolve(target).await.map_err(ServeError::resolver)? {
        ResolvedDiagnosticTarget::Archive(archive) => Ok(archive),
        ResolvedDiagnosticTarget::Live(_) => Err(ServeError::new(
            ServeErrorCode::ActiveTarget,
            "diagnostic serve accepts only inactive archives",
        )),
    }
}

pub(crate) fn start_archive(
    archive: ArchiveTarget,
    port: u16,
) -> Result<ArchiveServeSession, ServeError> {
    let run_id = archive.run_id();
    let run_directory = fs::canonicalize(archive.run_directory()).map_err(|error| {
        ServeError::new(
            ServeErrorCode::ArchivePath,
            format!(
                "cannot normalize archive directory {}: {error}",
                archive.run_directory().display()
            ),
        )
    })?;
    let mut archive = ArchiveTarget::open_expected(&run_directory, run_id)
        .map_err(|error| ServeError::new(ServeErrorCode::ArchiveRead, error.to_string()))?;
    let clean_shutdown = archive
        .capture()
        .map_err(|error| ServeError::new(ServeErrorCode::ArchiveRead, error.to_string()))?
        .metadata()
        .clean_shutdown();
    let archive_directory = run_directory
        .to_str()
        .ok_or_else(|| {
            ServeError::new(
                ServeErrorCode::ArchivePath,
                "archive directory is not valid UTF-8 and cannot be represented in the locator",
            )
        })?
        .to_owned();

    let assembly = ArchiveRouteAssembly::new(
        QueryEndpoints::archive(run_id, &run_directory),
        DumpEndpoints::archive(run_id, &run_directory, PerfettoDumpProducer::new()),
    )
    .map_err(|error| ServeError::new(ServeErrorCode::RouteAssembly, error.to_string()))?;
    let routes = assembly
        .route_definitions()
        .map_err(|error| ServeError::new(ServeErrorCode::RouteAssembly, error.to_string()))?;
    let process_identity = current_process_identity().map_err(|error| {
        ServeError::new(
            ServeErrorCode::ProcessIdentity,
            format!("cannot identify the archive server process: {error}"),
        )
    })?;
    let server = DiagnosticServer::start(
        ServerConfig::new(run_id, std::process::id(), process_identity)
            .with_bind(LOOPBACK_BIND_HOST, port),
        routes,
    )
    .map_err(|error| ServeError::new(ServeErrorCode::ServerStart, error.to_string()))?;
    let locator = ArchiveServeLocator {
        locator_schema_version: LOCATOR_SCHEMA_VERSION,
        run_id,
        local_url: server.identity().local_endpoint().as_str().to_owned(),
        archive_directory,
        clean_shutdown,
    };
    Ok(ArchiveServeSession {
        server: Some(server),
        archive,
        locator,
    })
}
