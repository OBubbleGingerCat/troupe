use std::{
    convert::Infallible,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use clap::Parser;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;
use troupe_diagnostics_core::{
    event::{CounterSampled, DiagnosticEvent, DiagnosticEventHeader, DiagnosticScope},
    hub::{
        AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
        DeliveryFailure, EventIdentity, LiveEventNotifier, MandatoryDurableReserver,
        ProductionDiagnosticHub,
    },
    id::CanonicalUuid,
    kinds::CounterKind,
    scalar::SchemaU64,
    time::ElapsedNs,
};
use troupe_diagnostics_perfetto::collect::{
    PERFETTO_EXPORTER_SCHEMA_VERSION, TRACE_CONTENT_WARNING,
};
use troupe_diagnostics_perfetto::dump::dump_captured_prefix_with_version;
use troupe_diagnostics_runtime::{
    archive::lease::{ActiveArchiveLease, ArchiveLeaseErrorCode, CleanupArchiveLease},
    query::reader::CapturedEventSource,
    registry::process_identity::ProcessIdentity,
    server::{
        dump::{
            CapturedPrefixDumpProducer, DUMP_API_SCHEMA_VERSION, DUMP_API_SCHEMA_VERSION_HEADER,
            DUMP_CAPTURED_WATERMARK_HEADER, DUMP_CLEAN_SHUTDOWN_HEADER,
            DUMP_CONTENT_WARNING_HEADER, DUMP_EVENT_SCHEMA_VERSION_HEADER,
            DUMP_EXPORTED_THROUGH_HEADER, DUMP_EXPORTER_SCHEMA_VERSION_HEADER,
            DUMP_PRODUCTION_OUTCOME_HEADER, DUMP_RUN_ID_HEADER, DUMP_TROUPE_VERSION_HEADER,
            DumpEndpoints, DumpProducerError, DumpProducerFuture, DumpProducerMetadata,
            PERFETTO_TRACE_MIME,
        },
        runtime::{DiagnosticServer, ServerConfig},
    },
    store::{
        batch::EventBatch,
        connection::{DiagnosticStore, InitialStoreMetadata},
        writer::TransactionalWriter,
    },
};

#[path = "../src/application/diagnostic_cli/archive_target.rs"]
mod archive_target;
#[path = "../src/application/diagnostic_cli/args.rs"]
mod args;
#[path = "../src/application/diagnostic_cli/dump.rs"]
mod dump;
#[path = "../src/application/diagnostic_cli/http_client.rs"]
mod http_client;
#[path = "../src/application/diagnostic_cli/resolver.rs"]
mod resolver;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use args::{DiagnosticCommand, DumpArgs, TroupeArgs, TroupeInvocation};
use dump::{DumpErrorCode, DumpOutput, DumpTermination, execute, validate_remote_metadata};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const STARTED_AT: &str = "2026-08-16T00:00:00Z";
const CONFIGURATION_IDENTITY: &str = "configuration-sha256:d06";
const REPORT_PREFIX: &str = "troupe: diagnostic dump ";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-d06-dump-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

#[derive(Clone, Copy, Debug, Default)]
struct AcceptAll;

#[derive(Debug)]
struct AcceptedReservation;

impl AdmissionReservation for AcceptedReservation {
    fn commit(self, _event: AcceptedDiagnosticEvent) {}
}

impl AdmissionReserver for AcceptAll {
    type Error = Infallible;
    type Reservation = AcceptedReservation;

    fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
        Ok(AcceptedReservation)
    }
}

impl MandatoryDurableReserver for AcceptAll {}

#[derive(Debug)]
struct IgnoreLive;

impl LiveEventNotifier for IgnoreLive {
    fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
        Ok(())
    }
}

fn create_store(directory: &Path) -> DiagnosticStore {
    DiagnosticStore::create(
        directory,
        &InitialStoreMetadata::new(run_id(), STARTED_AT, CONFIGURATION_IDENTITY),
    )
    .expect("create diagnostic store")
}

fn diagnostic_hub() -> ProductionDiagnosticHub<AcceptAll> {
    ProductionDiagnosticHub::production(run_id(), AcceptAll, Box::new(IgnoreLive))
}

fn accepted_event(hub: &ProductionDiagnosticHub<AcceptAll>) -> AcceptedDiagnosticEvent {
    hub.admit(
        |identity: EventIdentity| {
            let header = DiagnosticEventHeader::new(
                identity.run_id(),
                identity.sequence(),
                ElapsedNs::new(identity.sequence().get() * 10),
                DiagnosticScope::new(None, None, None, None, None, None, None),
                Vec::new(),
            )
            .expect("valid event header");
            DiagnosticEvent::CounterSampled(CounterSampled::new(
                header,
                CounterKind::AgentTurnActive,
                identity.sequence(),
            ))
        },
        None,
    )
    .expect("admit diagnostic event")
    .accepted()
    .clone()
}

fn append_events(writer: &mut TransactionalWriter<()>, count: usize) {
    let hub = diagnostic_hub();
    let events = (0..count).map(|_| accepted_event(&hub)).collect::<Vec<_>>();
    writer
        .commit_batch(&EventBatch::new(events).expect("nonempty event batch"))
        .expect("commit diagnostic events");
}

fn create_archive(label: &str, event_count: usize) -> TestDirectory {
    let directory = TestDirectory::new(label);
    let lease = ActiveArchiveLease::acquire(directory.path()).expect("active archive lease");
    let mut writer =
        TransactionalWriter::new(create_store(directory.path()), ()).expect("create writer");
    append_events(&mut writer, event_count);
    drop(writer);
    drop(lease);
    directory
}

struct ActiveRun {
    directory: TestDirectory,
    lease: Arc<ActiveArchiveLease>,
    _writer: TransactionalWriter<()>,
}

impl ActiveRun {
    fn new(label: &str, event_count: usize) -> Self {
        let directory = TestDirectory::new(label);
        let lease =
            Arc::new(ActiveArchiveLease::acquire(directory.path()).expect("active archive lease"));
        let mut writer =
            TransactionalWriter::new(create_store(directory.path()), ()).expect("create writer");
        append_events(&mut writer, event_count);
        Self {
            directory,
            lease,
            _writer: writer,
        }
    }
}

fn parse_archive_dump(
    archive: &Path,
    output: &Path,
    through: Option<u64>,
    force: bool,
) -> DumpArgs {
    let mut argv = vec![
        "troupe".to_owned(),
        "diagnostic".to_owned(),
        "dump".to_owned(),
        "--archive".to_owned(),
        archive.display().to_string(),
        "--output".to_owned(),
        output.display().to_string(),
    ];
    if let Some(through) = through {
        argv.extend(["--through".to_owned(), through.to_string()]);
    }
    if force {
        argv.push("--force".to_owned());
    }
    parse_dump(argv)
}

fn parse_url_dump(url: &str, output: &Path) -> DumpArgs {
    parse_dump([
        "troupe".to_owned(),
        "diagnostic".to_owned(),
        "dump".to_owned(),
        "--url".to_owned(),
        url.to_owned(),
        "--output".to_owned(),
        output.display().to_string(),
    ])
}

fn parse_dump(argv: impl IntoIterator<Item = String>) -> DumpArgs {
    match TroupeArgs::try_parse_from(argv)
        .expect("valid dump arguments")
        .into_invocation()
    {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Dump(arguments)) => arguments,
        _ => panic!("expected diagnostic dump invocation"),
    }
}

#[derive(Default)]
struct CapturedOutput {
    stderr: String,
}

impl CapturedOutput {
    fn record(&self) -> Value {
        assert_eq!(self.stderr.lines().count(), 1);
        serde_json::from_str(
            self.stderr
                .strip_prefix(REPORT_PREFIX)
                .expect("dump report prefix")
                .trim_end(),
        )
        .expect("valid dump report JSON")
    }
}

impl DumpOutput for CapturedOutput {
    type Error = Infallible;

    fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error> {
        self.stderr.push_str(text);
        Ok(())
    }
}

fn assert_no_publication_residue(directory: &Path) {
    let residue = fs::read_dir(directory)
        .expect("read output directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".troupe-pftrace-"))
        .collect::<Vec<_>>();
    assert!(residue.is_empty(), "publication residue: {residue:?}");
}

fn assert_success_record(
    output: &CapturedOutput,
    output_path: &Path,
    captured: &str,
    exported: &str,
) {
    let record = output.record();
    assert_eq!(record["report_schema_version"], 1);
    assert_eq!(record["publication"], "published");
    assert_eq!(record["phase"], "complete");
    assert_eq!(record["run_id"], RUN_ID);
    assert_eq!(record["captured_watermark"], captured);
    assert_eq!(record["exported_through"], exported);
    assert_eq!(record["event_count"], exported);
    assert_eq!(record["output"], output_path.display().to_string());
    assert_eq!(record["content_warning"], TRACE_CONTENT_WARNING);
    assert!(record.get("failure").is_none());
    assert!(record.get("manual_check_paths").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_archive_default_and_through_zero_publish_real_residue_free_traces() {
    let archive = create_archive("local", 2);
    let default_path = archive.path().join("default trace.pftrace");
    let mut default_output = CapturedOutput::default();

    let termination = execute(
        parse_archive_dump(archive.path(), &default_path, None, false),
        &mut default_output,
        CancellationToken::new(),
    )
    .await
    .expect("publish default captured head");
    assert_eq!(termination, DumpTermination::Published);
    assert!(!fs::read(&default_path).expect("read trace").is_empty());
    assert_success_record(&default_output, &default_path, "2", "2");
    assert_no_publication_residue(archive.path());

    let zero_path = archive.path().join("zero.pftrace");
    let mut zero_output = CapturedOutput::default();
    execute(
        parse_archive_dump(archive.path(), &zero_path, Some(0), false),
        &mut zero_output,
        CancellationToken::new(),
    )
    .await
    .expect("publish descriptor-only trace");
    assert!(!fs::read(&zero_path).expect("read zero trace").is_empty());
    assert_success_record(&zero_output, &zero_path, "2", "0");
    assert_no_publication_residue(archive.path());

    let future_path = archive.path().join("future.pftrace");
    let mut future_output = CapturedOutput::default();
    let error = execute(
        parse_archive_dump(archive.path(), &future_path, Some(3), false),
        &mut future_output,
        CancellationToken::new(),
    )
    .await
    .expect_err("reject future watermark");
    assert_eq!(error.code(), DumpErrorCode::PublicationFailed);
    assert_eq!(future_output.record()["publication"], "not_published");
    assert!(!future_path.exists());
    assert_no_publication_residue(archive.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_file_force_directory_and_symlink_follow_atomic_publisher_policy() {
    let archive = create_archive("output-policy", 1);
    let output_path = archive.path().join("trace.pftrace");
    fs::write(&output_path, b"old trace").expect("write old trace");

    let mut refused_output = CapturedOutput::default();
    let error = execute(
        parse_archive_dump(archive.path(), &output_path, None, false),
        &mut refused_output,
        CancellationToken::new(),
    )
    .await
    .expect_err("default must not replace an existing output");
    assert_eq!(error.code(), DumpErrorCode::PublicationFailed);
    assert_eq!(
        refused_output.record()["failure_code"],
        "target_already_exists"
    );
    assert_eq!(fs::read(&output_path).unwrap(), b"old trace");

    let mut forced_output = CapturedOutput::default();
    execute(
        parse_archive_dump(archive.path(), &output_path, None, true),
        &mut forced_output,
        CancellationToken::new(),
    )
    .await
    .expect("force replaces a regular file");
    assert_ne!(fs::read(&output_path).unwrap(), b"old trace");
    assert_success_record(&forced_output, &output_path, "1", "1");

    let directory_path = archive.path().join("directory.pftrace");
    fs::create_dir(&directory_path).unwrap();
    let mut directory_output = CapturedOutput::default();
    let directory_error = execute(
        parse_archive_dump(archive.path(), &directory_path, None, true),
        &mut directory_output,
        CancellationToken::new(),
    )
    .await
    .expect_err("force rejects a directory");
    assert_eq!(directory_error.code(), DumpErrorCode::PublicationFailed);
    assert_eq!(
        directory_output.record()["failure_code"],
        "target_type_rejected"
    );
    assert!(directory_path.is_dir());

    let symlink_target = archive.path().join("symlink-target");
    let symlink_path = archive.path().join("symlink.pftrace");
    fs::write(&symlink_target, b"unchanged").unwrap();
    symlink(&symlink_target, &symlink_path).unwrap();
    let mut symlink_output = CapturedOutput::default();
    let symlink_error = execute(
        parse_archive_dump(archive.path(), &symlink_path, None, true),
        &mut symlink_output,
        CancellationToken::new(),
    )
    .await
    .expect_err("force rejects a symlink");
    assert_eq!(symlink_error.code(), DumpErrorCode::PublicationFailed);
    assert_eq!(
        symlink_output.record()["failure_code"],
        "target_type_rejected"
    );
    assert_eq!(fs::read(&symlink_target).unwrap(), b"unchanged");
    assert!(
        fs::symlink_metadata(&symlink_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_no_publication_residue(archive.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_before_publication_reports_130_and_leaves_no_partial_target() {
    let archive = create_archive("cancel-before", 1);
    let output_path = archive.path().join("cancelled.pftrace");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut output = CapturedOutput::default();

    let termination = execute(
        parse_archive_dump(archive.path(), &output_path, None, false),
        &mut output,
        cancellation,
    )
    .await
    .expect("cancellation is a command termination, not operation failure");
    assert_eq!(termination, DumpTermination::Interrupted);
    assert_eq!(termination.exit_code(), 130);
    assert_eq!(output.record()["publication"], "not_published");
    assert!(!output_path.exists());
    assert_no_publication_residue(archive.path());
}

#[derive(Clone)]
struct FixedProducer {
    metadata: DumpProducerMetadata,
    mode: FixedProducerMode,
    first_write_completed: Arc<AtomicBool>,
}

#[derive(Clone)]
enum FixedProducerMode {
    Real,
    Bytes(Vec<u8>),
    PauseAfterMatchingMetadata,
}

impl FixedProducer {
    fn new(pause_after_first_write: bool) -> Self {
        Self::with_mode(if pause_after_first_write {
            FixedProducerMode::PauseAfterMatchingMetadata
        } else {
            FixedProducerMode::Real
        })
    }

    fn with_body(body: Vec<u8>) -> Self {
        Self::with_mode(FixedProducerMode::Bytes(body))
    }

    fn with_mode(mode: FixedProducerMode) -> Self {
        Self {
            metadata: DumpProducerMetadata::new(
                PERFETTO_EXPORTER_SCHEMA_VERSION,
                env!("CARGO_PKG_VERSION"),
                TRACE_CONTENT_WARNING,
            )
            .expect("valid producer metadata"),
            mode,
            first_write_completed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl CapturedPrefixDumpProducer for FixedProducer {
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
            match &self.mode {
                FixedProducerMode::Real => {
                    dump_captured_prefix_with_version(
                        source,
                        writer,
                        through,
                        self.metadata.troupe_version(),
                    )
                    .await
                    .map_err(|error| DumpProducerError::new("test_write", error.to_string()))?;
                }
                FixedProducerMode::Bytes(body) => {
                    writer
                        .write_all(body)
                        .await
                        .map_err(|error| DumpProducerError::new("test_write", error.to_string()))?;
                }
                FixedProducerMode::PauseAfterMatchingMetadata => {
                    let body = matching_metadata_trace(source, through);
                    writer
                        .write_all(&body)
                        .await
                        .map_err(|error| DumpProducerError::new("test_write", error.to_string()))?;
                    self.first_write_completed.store(true, Ordering::Release);
                    std::thread::sleep(Duration::from_millis(300));
                    writer
                        .write_all(b"\x0a\x00")
                        .await
                        .map_err(|error| DumpProducerError::new("test_write", error.to_string()))?;
                }
            }
            if !matches!(&self.mode, FixedProducerMode::PauseAfterMatchingMetadata) {
                self.first_write_completed.store(true, Ordering::Release);
            }
            Ok(())
        })
    }
}

fn matching_metadata_trace(
    source: &CapturedEventSource<'_>,
    through: Option<SchemaU64>,
) -> Vec<u8> {
    let metadata = source.metadata();
    let captured = source.captured_watermark();
    let exported = through.unwrap_or(captured);
    let outcome = metadata.production_outcome().unwrap_or("unavailable");
    let clean_shutdown = metadata.production_outcome().map_or("unavailable", |_| {
        if metadata.clean_shutdown() {
            "true"
        } else {
            "false"
        }
    });
    let name = trace_metadata_name(
        metadata.run_id(),
        captured.get(),
        exported.get(),
        outcome,
        clean_shutdown,
    );
    trace_with_metadata_name(metadata.run_id(), &name)
}

fn trace_metadata_name(
    run_id: CanonicalUuid,
    captured: u64,
    exported: u64,
    outcome: &str,
    clean_shutdown: &str,
) -> String {
    format!(
        "Troupe metadata | exporter_schema={} | event_schema={} | run_id={} | \
         captured_watermark={} | exported_through={} | troupe_version={} | outcome={} | \
         clean_shutdown={} | content_warning={}",
        PERFETTO_EXPORTER_SCHEMA_VERSION,
        troupe_diagnostics_core::event::EVENT_SCHEMA_VERSION,
        run_id,
        captured,
        exported,
        env!("CARGO_PKG_VERSION"),
        outcome,
        clean_shutdown,
        TRACE_CONTENT_WARNING,
    )
}

fn trace_with_metadata_name(run_id: CanonicalUuid, metadata_name: &str) -> Vec<u8> {
    let mut trace = Vec::new();
    push_trace_packet(
        &mut trace,
        &track_descriptor_packet(1, &format!("Troupe Production {run_id}"), None),
    );
    push_trace_packet(
        &mut trace,
        &track_descriptor_packet(2, metadata_name, Some(1)),
    );
    trace
}

fn track_descriptor_packet(uuid: u64, name: &str, parent_uuid: Option<u64>) -> Vec<u8> {
    let mut descriptor = Vec::new();
    push_varint_field(&mut descriptor, 1, uuid);
    push_length_delimited(&mut descriptor, 2, name.as_bytes());
    if let Some(parent_uuid) = parent_uuid {
        push_varint_field(&mut descriptor, 5, parent_uuid);
    }

    let mut packet = Vec::new();
    push_varint_field(&mut packet, 10, 1);
    push_length_delimited(&mut packet, 60, &descriptor);
    packet
}

fn push_trace_packet(trace: &mut Vec<u8>, packet: &[u8]) {
    push_length_delimited(trace, 1, packet);
}

fn push_varint_field(output: &mut Vec<u8>, field_number: u64, value: u64) {
    push_varint(output, field_number << 3);
    push_varint(output, value);
}

fn push_length_delimited(output: &mut Vec<u8>, field_number: u64, value: &[u8]) {
    push_varint(output, (field_number << 3) | 2);
    push_varint(output, value.len() as u64);
    output.extend_from_slice(value);
}

fn push_varint(output: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

fn process_identity() -> ProcessIdentity {
    ProcessIdentity::new("test", "d06:4242").expect("valid process identity")
}

fn start_active_server(run: &ActiveRun, producer: FixedProducer) -> DiagnosticServer {
    let endpoint = DumpEndpoints::active(run_id(), Arc::clone(&run.lease), producer);
    DiagnosticServer::start(
        ServerConfig::new(run_id(), std::process::id(), process_identity())
            .with_bind("127.0.0.1", 0),
        endpoint.route_definitions().expect("valid dump route"),
    )
    .expect("start diagnostic server")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_url_uses_h05_stream_and_publishes_on_the_callers_filesystem() {
    let run = ActiveRun::new("active-url", 2);
    let server = start_active_server(&run, FixedProducer::new(false));
    let output_directory = TestDirectory::new("remote-output");
    let output_path = output_directory.path().join("remote trace.pftrace");
    let mut output = CapturedOutput::default();

    let termination = execute(
        parse_url_dump(server.identity().local_endpoint().as_str(), &output_path),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .expect("publish remote H05 stream");
    assert_eq!(termination, DumpTermination::Published);
    let trace = fs::read(&output_path).unwrap();
    assert!(String::from_utf8_lossy(&trace).contains("captured_watermark=2 | exported_through=2"));
    assert_success_record(&output, &output_path, "2", "2");
    assert_no_publication_residue(output_directory.path());

    let contended = CleanupArchiveLease::acquire(run.directory.path()).unwrap_err();
    assert_eq!(contended.code(), ArchiveLeaseErrorCode::Contended);
    assert!(server.try_core_failure().is_none());
    server.shutdown().expect("clean server shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_remote_body_is_not_published() {
    let run = ActiveRun::new("malformed-body", 1);
    let server = start_active_server(&run, FixedProducer::with_body(b"garbage".to_vec()));
    let output_directory = TestDirectory::new("malformed-output");
    let output_path = output_directory.path().join("malformed.pftrace");
    let mut output = CapturedOutput::default();

    let error = execute(
        parse_url_dump(server.identity().local_endpoint().as_str(), &output_path),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .expect_err("malformed remote trace must fail publication");
    assert_eq!(error.code(), DumpErrorCode::PublicationFailed);
    assert_eq!(output.record()["publication"], "not_published");
    assert!(!output_path.exists());
    assert_no_publication_residue(output_directory.path());
    server.shutdown().expect("clean server shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_remote_trace_packet_is_not_published() {
    let run = ActiveRun::new("empty-packet", 1);
    let metadata = trace_metadata_name(run_id(), 1, 1, "unavailable", "unavailable");
    let mut body = trace_with_metadata_name(run_id(), &metadata);
    body.extend_from_slice(&[0x0a, 0x00]);
    let server = start_active_server(&run, FixedProducer::with_body(body));
    let output_directory = TestDirectory::new("empty-packet-output");
    let output_path = output_directory.path().join("empty-packet.pftrace");
    let mut output = CapturedOutput::default();

    let error = execute(
        parse_url_dump(server.identity().local_endpoint().as_str(), &output_path),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .expect_err("empty remote TracePacket must fail publication");
    assert_eq!(error.code(), DumpErrorCode::PublicationFailed);
    let record = output.record();
    assert_eq!(record["publication"], "not_published");
    assert_eq!(record["failure_code"], "body_invalid");
    assert!(!output_path.exists());
    assert_no_publication_residue(output_directory.path());
    server.shutdown().expect("clean server shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_trace_metadata_mismatch_is_not_published() {
    let run = ActiveRun::new("metadata-mismatch", 1);
    let mismatched = trace_with_metadata_name(
        run_id(),
        &trace_metadata_name(run_id(), 0, 0, "unavailable", "unavailable"),
    );
    let server = start_active_server(&run, FixedProducer::with_body(mismatched));
    let output_directory = TestDirectory::new("metadata-mismatch-output");
    let output_path = output_directory.path().join("mismatched.pftrace");
    let mut output = CapturedOutput::default();

    let error = execute(
        parse_url_dump(server.identity().local_endpoint().as_str(), &output_path),
        &mut output,
        CancellationToken::new(),
    )
    .await
    .expect_err("trace metadata that differs from HTTP headers must fail publication");
    assert_eq!(error.code(), DumpErrorCode::PublicationFailed);
    let record = output.record();
    assert_eq!(record["publication"], "not_published");
    assert_eq!(record["failure_code"], "body_metadata_mismatch");
    assert!(!output_path.exists());
    assert_no_publication_residue(output_directory.path());
    server.shutdown().expect("clean server shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_during_remote_body_waits_for_atomic_cleanup_and_keeps_server_alive() {
    let run = ActiveRun::new("active-cancel", 1);
    let producer = FixedProducer::new(true);
    let first_write_completed = Arc::clone(&producer.first_write_completed);
    let server = start_active_server(&run, producer);
    let output_directory = TestDirectory::new("cancel-output");
    let output_path = output_directory.path().join("cancelled.pftrace");
    let cancellation = CancellationToken::new();
    let cancel_from_probe = cancellation.clone();
    let cancel_task = tokio::spawn(async move {
        while !first_write_completed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        cancel_from_probe.cancel();
    });
    let mut output = CapturedOutput::default();

    let termination = execute(
        parse_url_dump(server.identity().local_endpoint().as_str(), &output_path),
        &mut output,
        cancellation,
    )
    .await
    .expect("SIGINT has a stable termination");
    cancel_task.await.unwrap();
    assert_eq!(termination, DumpTermination::Interrupted);
    assert_eq!(output.record()["publication"], "not_published");
    assert!(!output_path.exists());
    assert_no_publication_residue(output_directory.path());
    assert!(server.try_core_failure().is_none());
    server
        .shutdown()
        .expect("dump cancellation does not stop Production server");
}

fn valid_remote_headers(captured: &str, exported: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        (reqwest::header::CONTENT_TYPE.as_str(), PERFETTO_TRACE_MIME),
        (DUMP_RUN_ID_HEADER, RUN_ID),
        (DUMP_CAPTURED_WATERMARK_HEADER, captured),
        (DUMP_EXPORTED_THROUGH_HEADER, exported),
        (DUMP_API_SCHEMA_VERSION_HEADER, "1"),
        (DUMP_EVENT_SCHEMA_VERSION_HEADER, "1"),
        (DUMP_EXPORTER_SCHEMA_VERSION_HEADER, "1"),
        (DUMP_TROUPE_VERSION_HEADER, env!("CARGO_PKG_VERSION")),
        (DUMP_PRODUCTION_OUTCOME_HEADER, "unavailable"),
        (DUMP_CLEAN_SHUTDOWN_HEADER, "unavailable"),
        (DUMP_CONTENT_WARNING_HEADER, TRACE_CONTENT_WARNING),
    ] {
        headers.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

#[test]
fn remote_metadata_rejects_identity_schema_watermark_and_transport_mismatch() {
    let headers = valid_remote_headers("2", "2");
    let metadata = validate_remote_metadata(&headers, run_id(), None).expect("valid metadata");
    assert_eq!(metadata.run_id, run_id());
    assert_eq!(metadata.captured_watermark.get(), 2);
    assert_eq!(metadata.exported_through.get(), 2);

    let mut wrong_run = headers.clone();
    wrong_run.insert(
        DUMP_RUN_ID_HEADER,
        HeaderValue::from_static("87654321-4321-4321-8321-cba987654321"),
    );
    assert_eq!(
        validate_remote_metadata(&wrong_run, run_id(), None)
            .unwrap_err()
            .code,
        "metadata_mismatch"
    );

    let mut wrong_schema = headers.clone();
    wrong_schema.insert(
        DUMP_API_SCHEMA_VERSION_HEADER,
        HeaderValue::from_static("2"),
    );
    assert_eq!(
        validate_remote_metadata(&wrong_schema, run_id(), None)
            .unwrap_err()
            .code,
        "metadata_mismatch"
    );

    let mut wrong_watermark = headers.clone();
    wrong_watermark.insert(DUMP_EXPORTED_THROUGH_HEADER, HeaderValue::from_static("1"));
    assert_eq!(
        validate_remote_metadata(&wrong_watermark, run_id(), None)
            .unwrap_err()
            .code,
        "metadata_mismatch"
    );

    let mut encoded = headers;
    encoded.insert(
        reqwest::header::CONTENT_ENCODING,
        HeaderValue::from_static("gzip"),
    );
    assert_eq!(
        validate_remote_metadata(&encoded, run_id(), None)
            .unwrap_err()
            .code,
        "metadata_mismatch"
    );
    assert_eq!(DUMP_API_SCHEMA_VERSION, 1);
}
