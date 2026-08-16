use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::lease::{ActiveArchiveLease, ArchiveLeaseErrorCode, CleanupArchiveLease},
    registry::{
        codec::encode_registry_entry,
        discover::{ProcessIdentityProbe, ServerIdentityProbe, ServerProbeError, discover_with},
        model::{BindEndpoint, RegistryEntry, WebBaseUrl},
        process_identity::{ObservedProcessIdentity, ProcessIdentity},
    },
    store::connection::{DiagnosticStore, InitialStoreMetadata},
};

#[path = "../src/application/diagnostic_cli/archive_target.rs"]
mod archive_target;
#[path = "../src/application/diagnostic_cli/http_client.rs"]
mod http_client;
#[path = "../src/application/diagnostic_cli/resolver.rs"]
mod resolver;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use archive_target::ArchiveTarget;
use http_client::{DiagnosticHttpClient, HttpClientErrorCode};
use resolver::{ResolverErrorCode, resolve_discovered_production, resolve_production_with};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestProduction {
    root: PathBuf,
}

impl TestProduction {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "troupe-d01-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(root.join(".troupe/diagnostics/instances")).unwrap();
        fs::create_dir_all(root.join(".troupe/diagnostics/runs")).unwrap();
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn instances(&self) -> PathBuf {
        self.root.join(".troupe/diagnostics/instances")
    }

    fn runs(&self) -> PathBuf {
        self.root.join(".troupe/diagnostics/runs")
    }

    fn run_directory(&self, run_id: CanonicalUuid) -> PathBuf {
        self.runs().join(run_id.to_string())
    }

    fn instance_path(&self, run_id: CanonicalUuid) -> PathBuf {
        self.instances().join(format!("{run_id}.json"))
    }
}

impl Drop for TestProduction {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_id(value: u64) -> CanonicalUuid {
    CanonicalUuid::parse(&format!("00000000-0000-4000-8000-{value:012x}")).unwrap()
}

fn process_identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity::new("test", &format!("boot-a-{pid}")).unwrap()
}

fn other_process_identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity::new("test", &format!("boot-b-{pid}")).unwrap()
}

fn create_archive(production: &TestProduction, run_id: CanonicalUuid) -> PathBuf {
    let run_directory = production.run_directory(run_id);
    fs::create_dir(&run_directory).unwrap();
    let lease = ActiveArchiveLease::acquire(&run_directory).unwrap();
    let store = DiagnosticStore::create(
        &run_directory,
        &InitialStoreMetadata::new(run_id, "2026-08-16T00:00:00Z", "configuration-sha256:d01"),
    )
    .unwrap();
    drop(store);
    drop(lease);
    run_directory
}

fn registry_entry(production: &TestProduction, run_id: CanonicalUuid, pid: u32) -> RegistryEntry {
    RegistryEntry::new(
        run_id,
        &production.run_directory(run_id),
        pid,
        process_identity(pid),
        BindEndpoint::new("127.0.0.1", 30_000 + (pid % 20_000) as u16).unwrap(),
        None,
        "2026-08-16T00:00:00Z",
    )
    .unwrap()
}

fn publish_entry(production: &TestProduction, entry: &RegistryEntry) {
    fs::write(
        production.instance_path(entry.run_id()),
        encode_registry_entry(entry).unwrap(),
    )
    .unwrap();
}

fn matching_identity(entry: &RegistryEntry) -> Vec<u8> {
    let bind = entry.bind();
    format!(
        concat!(
            "{{",
            "\"identity_schema_version\":1,",
            "\"server_protocol_version\":1,",
            "\"event_schema_version\":1,",
            "\"view_schema_version\":1,",
            "\"api_schema_version\":1,",
            "\"run_id\":\"{}\",",
            "\"owner_pid\":{},",
            "\"process_identity\":\"{}\",",
            "\"bind_host\":\"{}\",",
            "\"port\":{},",
            "\"local_endpoint\":\"{}\",",
            "\"advertise_url\":null,",
            "\"base_path\":\"/\",",
            "\"api_base_path\":\"/api/v1\",",
            "\"identity_path\":\"/api/v1/identity\",",
            "\"security_scope\":\"trusted_network\",",
            "\"operational_limits\":{{\"max_page_rows\":\"500\"}}",
            "}}"
        ),
        entry.run_id(),
        entry.owner_pid(),
        entry.process_identity().as_str(),
        bind.host(),
        bind.port(),
        entry.local_endpoint().as_str(),
    )
    .into_bytes()
}

#[derive(Default)]
struct FakeProcesses {
    observations: Mutex<HashMap<u32, VecDeque<ObservedProcessIdentity>>>,
}

impl FakeProcesses {
    fn set(&self, pid: u32, values: impl IntoIterator<Item = ObservedProcessIdentity>) {
        self.observations
            .lock()
            .unwrap()
            .insert(pid, values.into_iter().collect());
    }

    fn alive(&self, entry: &RegistryEntry) {
        self.set(
            entry.owner_pid(),
            [ObservedProcessIdentity::Alive(
                entry.process_identity().clone(),
            )],
        );
    }

    fn gone(&self, entry: &RegistryEntry) {
        self.set(entry.owner_pid(), [ObservedProcessIdentity::DefinitelyGone]);
    }
}

impl ProcessIdentityProbe for FakeProcesses {
    fn observe(&self, pid: u32) -> ObservedProcessIdentity {
        let mut observations = self.observations.lock().unwrap();
        let Some(values) = observations.get_mut(&pid) else {
            return ObservedProcessIdentity::Unknown;
        };
        if values.len() > 1 {
            values.pop_front().unwrap()
        } else {
            values
                .front()
                .cloned()
                .unwrap_or(ObservedProcessIdentity::Unknown)
        }
    }
}

#[derive(Default)]
struct FakeServers {
    responses: Mutex<HashMap<u32, Result<Vec<u8>, String>>>,
}

impl FakeServers {
    fn matching(&self, entry: &RegistryEntry) {
        self.responses
            .lock()
            .unwrap()
            .insert(entry.owner_pid(), Ok(matching_identity(entry)));
    }

    fn unreachable(&self, entry: &RegistryEntry) {
        self.responses
            .lock()
            .unwrap()
            .insert(entry.owner_pid(), Err("connection refused".to_owned()));
    }
}

impl ServerIdentityProbe for FakeServers {
    fn probe_identity(&self, entry: &RegistryEntry) -> Result<Vec<u8>, ServerProbeError> {
        match self.responses.lock().unwrap().get(&entry.owner_pid()) {
            Some(Ok(bytes)) => Ok(bytes.clone()),
            Some(Err(detail)) => Err(ServerProbeError::new(detail.clone())),
            None => Err(ServerProbeError::new("no fake identity endpoint")),
        }
    }
}

#[test]
fn copied_archive_uses_embedded_identity_captures_watermark_and_holds_q00_lease() {
    let production = TestProduction::new("copied-archive");
    let expected_run_id = run_id(1);
    let original = create_archive(&production, expected_run_id);
    let copied = production.runs().join("copied archive with arbitrary name");
    fs::rename(original, &copied).unwrap();

    let mut target = ArchiveTarget::open_identified(&copied).unwrap();
    assert_eq!(target.run_id(), expected_run_id);
    assert_eq!(target.validated_watermark().get(), 0);
    assert_eq!(target.run_directory(), copied);
    assert_eq!(
        CleanupArchiveLease::acquire(&copied)
            .expect_err("the target-owned shared lease must block cleanup")
            .code(),
        ArchiveLeaseErrorCode::Contended
    );
    let captured = target.capture().unwrap();
    assert_eq!(captured.metadata().run_id(), expected_run_id);
    assert_eq!(captured.captured_watermark().get(), 0);
    drop(captured);
    drop(target);
    CleanupArchiveLease::acquire(&copied).expect("dropping the target releases its lease");
}

#[test]
fn implicit_resolution_prefers_one_revalidated_active_run_over_archive_history() {
    let production = TestProduction::new("active-preferred");
    let active_id = run_id(10);
    let historical_id = run_id(11);
    create_archive(&production, active_id);
    create_archive(&production, historical_id);
    let entry = registry_entry(&production, active_id, 8_010);
    publish_entry(&production, &entry);

    let processes = FakeProcesses::default();
    let servers = FakeServers::default();
    processes.alive(&entry);
    servers.matching(&entry);

    let target = resolve_production_with(production.root(), None, &processes, &servers).unwrap();
    assert!(target.is_live());
    assert_eq!(target.run_id(), active_id);
    assert_eq!(
        target.as_http().unwrap().base_url().as_str(),
        entry.local_endpoint().as_str()
    );
}

#[test]
fn potentially_live_ambiguity_and_unhealthy_explicit_run_never_bypass_to_sqlite() {
    let production = TestProduction::new("live-ambiguity");
    let active_id = run_id(20);
    let unhealthy_id = run_id(21);
    create_archive(&production, active_id);
    create_archive(&production, unhealthy_id);
    let active = registry_entry(&production, active_id, 8_020);
    let unhealthy = registry_entry(&production, unhealthy_id, 8_021);
    publish_entry(&production, &active);
    publish_entry(&production, &unhealthy);

    let processes = FakeProcesses::default();
    let servers = FakeServers::default();
    processes.alive(&active);
    processes.alive(&unhealthy);
    servers.matching(&active);
    servers.unreachable(&unhealthy);

    let implicit = resolve_production_with(production.root(), None, &processes, &servers)
        .expect_err("an unhealthy owner remains potentially live");
    assert_eq!(implicit.code(), ResolverErrorCode::AmbiguousLiveTarget);

    let explicit =
        resolve_production_with(production.root(), Some(unhealthy_id), &processes, &servers)
            .expect_err("an unhealthy instance must not be bypassed through its archive");
    assert_eq!(explicit.code(), ResolverErrorCode::UnsafeCandidate);
}

#[test]
fn explicit_definite_stale_run_revalidates_then_opens_only_its_same_id_archive() {
    let production = TestProduction::new("stale-fallback");
    let expected_run_id = run_id(30);
    create_archive(&production, expected_run_id);
    let entry = registry_entry(&production, expected_run_id, 8_030);
    publish_entry(&production, &entry);

    let processes = FakeProcesses::default();
    let servers = FakeServers::default();
    processes.gone(&entry);

    let mut target = resolve_production_with(
        production.root(),
        Some(expected_run_id),
        &processes,
        &servers,
    )
    .unwrap();
    assert!(!target.is_live());
    assert_eq!(target.run_id(), expected_run_id);
    assert_eq!(
        target
            .as_archive_mut()
            .unwrap()
            .capture()
            .unwrap()
            .metadata()
            .run_id(),
        expected_run_id
    );
}

#[test]
fn locator_replacement_between_discovery_and_use_fails_closed() {
    let production = TestProduction::new("locator-race");
    let expected_run_id = run_id(40);
    create_archive(&production, expected_run_id);
    let entry = registry_entry(&production, expected_run_id, 8_040);
    publish_entry(&production, &entry);

    let processes = FakeProcesses::default();
    let servers = FakeServers::default();
    processes.gone(&entry);
    let candidates = discover_with(production.root(), &processes, &servers).unwrap();

    let locator = production.instance_path(expected_run_id);
    let replacement = production.instances().join("replacement.tmp");
    fs::write(&replacement, fs::read(&locator).unwrap()).unwrap();
    fs::rename(&replacement, &locator).unwrap();

    let error =
        resolve_discovered_production(&candidates, Some(expected_run_id), &processes, &servers)
            .expect_err("the stale locator must be revalidated before archive fallback");
    assert_eq!(error.code(), ResolverErrorCode::RevalidationFailed);
}

#[test]
fn implicit_archive_selection_requires_uniqueness_and_never_uses_latest_ordering() {
    let production = TestProduction::new("archive-ambiguity");
    let first_id = run_id(50);
    let second_id = run_id(51);
    create_archive(&production, first_id);
    create_archive(&production, second_id);
    let processes = FakeProcesses::default();
    let servers = FakeServers::default();

    let error = resolve_production_with(production.root(), None, &processes, &servers)
        .expect_err("two valid archives require an explicit Run ID");
    assert_eq!(error.code(), ResolverErrorCode::AmbiguousArchiveTarget);

    let selected =
        resolve_production_with(production.root(), Some(first_id), &processes, &servers).unwrap();
    assert!(!selected.is_live());
    assert_eq!(selected.run_id(), first_id);
}

#[test]
fn implicit_selection_counts_only_q00_validated_stale_archives() {
    let production = TestProduction::new("valid-archive-count");
    let stale_invalid_id = run_id(55);
    let valid_id = run_id(56);
    let stale_directory = create_archive(&production, stale_invalid_id);
    create_archive(&production, valid_id);
    fs::write(stale_directory.join("diagnostics.sqlite3"), b"not sqlite").unwrap();

    let stale = registry_entry(&production, stale_invalid_id, 8_055);
    publish_entry(&production, &stale);
    let processes = FakeProcesses::default();
    let servers = FakeServers::default();
    processes.gone(&stale);

    let target = resolve_production_with(production.root(), None, &processes, &servers)
        .expect("the one Q00-valid archive is the unique implicit target");
    assert!(!target.is_live());
    assert_eq!(target.run_id(), valid_id);
}

fn url_identity(run_id: CanonicalUuid, protocol: u64, base_path: &str) -> Vec<u8> {
    let api_path = if base_path == "/" {
        "/api/v1".to_owned()
    } else {
        format!("{base_path}/api/v1")
    };
    format!(
        concat!(
            "{{",
            "\"identity_schema_version\":1,",
            "\"server_protocol_version\":{},",
            "\"event_schema_version\":1,",
            "\"view_schema_version\":1,",
            "\"api_schema_version\":1,",
            "\"run_id\":\"{}\",",
            "\"owner_pid\":4242,",
            "\"process_identity\":\"test:url-owner\",",
            "\"bind_host\":\"0.0.0.0\",",
            "\"port\":43120,",
            "\"local_endpoint\":\"http://127.0.0.1:43120/\",",
            "\"advertise_url\":\"https://diagnostics.example/troupe\",",
            "\"base_path\":\"{}\",",
            "\"api_base_path\":\"{}\",",
            "\"identity_path\":\"{}/identity\",",
            "\"security_scope\":\"trusted_network\",",
            "\"operational_limits\":{{\"max_page_rows\":\"500\"}}",
            "}}"
        ),
        protocol, run_id, base_path, api_path, api_path,
    )
    .into_bytes()
}

#[test]
fn url_identity_uses_shared_strict_decoder_and_validates_protocol_run_and_base_path() {
    let expected_run_id = run_id(60);
    let base_url = WebBaseUrl::parse("https://diagnostics.example/troupe").unwrap();
    let bytes = url_identity(expected_run_id, 1, "/troupe");
    let client = DiagnosticHttpClient::from_identity_bytes(base_url.clone(), &bytes, None).unwrap();
    assert_eq!(client.run_id(), expected_run_id);
    assert_eq!(
        client.endpoint("/api/v1/status").unwrap().as_str(),
        "https://diagnostics.example/troupe/api/v1/status"
    );

    let wrong_run =
        DiagnosticHttpClient::from_identity_bytes(base_url.clone(), &bytes, Some(run_id(61)))
            .unwrap_err();
    assert_eq!(wrong_run.code(), HttpClientErrorCode::RunIdentityMismatch);

    let wrong_protocol = DiagnosticHttpClient::from_identity_bytes(
        base_url.clone(),
        &url_identity(expected_run_id, 2, "/troupe"),
        None,
    )
    .unwrap_err();
    assert_eq!(
        wrong_protocol.code(),
        HttpClientErrorCode::IncompatibleProtocol
    );

    let wrong_path = DiagnosticHttpClient::from_identity_bytes(
        base_url.clone(),
        &url_identity(expected_run_id, 1, "/other"),
        None,
    )
    .unwrap_err();
    assert_eq!(
        wrong_path.code(),
        HttpClientErrorCode::LocatorIdentityMismatch
    );

    let mut unknown_field = bytes;
    unknown_field.splice(1..1, b"\"future\":true,".iter().copied());
    let invalid =
        DiagnosticHttpClient::from_identity_bytes(base_url, &unknown_field, None).unwrap_err();
    assert_eq!(invalid.code(), HttpClientErrorCode::InvalidIdentity);
}

#[test]
fn validated_registry_target_combines_local_authority_with_advertised_base_path() {
    let production = TestProduction::new("advertised-base-path");
    let expected_run_id = run_id(65);
    create_archive(&production, expected_run_id);
    let entry = RegistryEntry::new(
        expected_run_id,
        &production.run_directory(expected_run_id),
        8_065,
        process_identity(8_065),
        BindEndpoint::new("0.0.0.0", 43_120).unwrap(),
        Some(WebBaseUrl::parse("https://diagnostics.example/troupe").unwrap()),
        "2026-08-16T00:00:00Z",
    )
    .unwrap();

    let client = DiagnosticHttpClient::from_validated_registry_entry(&entry).unwrap();
    assert_eq!(client.run_id(), expected_run_id);
    assert_eq!(client.base_url().as_str(), "http://127.0.0.1:43120/troupe");
    assert_eq!(
        client.endpoint("/api/v1/identity").unwrap().as_str(),
        "http://127.0.0.1:43120/troupe/api/v1/identity"
    );
}

#[test]
fn active_revalidation_rejects_process_identity_change_before_returning_http_target() {
    let production = TestProduction::new("active-race");
    let expected_run_id = run_id(70);
    create_archive(&production, expected_run_id);
    let entry = registry_entry(&production, expected_run_id, 8_070);
    publish_entry(&production, &entry);

    let processes = FakeProcesses::default();
    let servers = FakeServers::default();
    processes.set(
        entry.owner_pid(),
        [
            ObservedProcessIdentity::Alive(entry.process_identity().clone()),
            ObservedProcessIdentity::Alive(entry.process_identity().clone()),
            ObservedProcessIdentity::Alive(entry.process_identity().clone()),
            ObservedProcessIdentity::Alive(other_process_identity(entry.owner_pid())),
        ],
    );
    servers.matching(&entry);

    let error = resolve_production_with(
        production.root(),
        Some(expected_run_id),
        &processes,
        &servers,
    )
    .expect_err("active use must repeat owner identity validation after the HTTP round trip");
    assert_eq!(error.code(), ResolverErrorCode::RevalidationFailed);
}
