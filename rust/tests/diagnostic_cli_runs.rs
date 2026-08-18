use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    registry::{
        codec::encode_registry_entry,
        discover::{
            CandidateClassification, ProcessIdentityProbe, ServerIdentityProbe, ServerProbeError,
        },
        model::{BindEndpoint, RegistryEntry},
        process_identity::{ObservedProcessIdentity, ProcessIdentity},
    },
    store::connection::{DiagnosticStore, InitialStoreMetadata},
};

#[path = "../src/application/diagnostic_cli/args.rs"]
mod args;
#[path = "../src/application/diagnostic_cli/http_client.rs"]
mod http_client;
#[path = "../src/application/diagnostic_cli/runs.rs"]
mod runs;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use args::{DocumentFormat, RunsArgs};
use runs::{RunsErrorCode, execute, list_runs_with};

const HUMAN_FIXTURE: &str = include_str!("../../tests/fixtures/diagnostics/cli/runs-human.txt");
const JSON_FIXTURE: &str = include_str!("../../tests/fixtures/diagnostics/cli/runs-v1.json");

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestProduction {
    root: PathBuf,
}

impl TestProduction {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "troupe-d08-{label}-{}-{sequence}",
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
    ProcessIdentity::new("test", &format!("boot-a:{pid}")).unwrap()
}

fn other_process_identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity::new("test", &format!("boot-b:{pid}")).unwrap()
}

fn create_archive(
    production: &TestProduction,
    run_id: CanonicalUuid,
    completed_outcome: Option<&str>,
) {
    let run_directory = production.run_directory(run_id);
    fs::create_dir(&run_directory).unwrap();
    let _active_lease = ActiveArchiveLease::acquire(&run_directory).unwrap();
    let store = DiagnosticStore::create(
        &run_directory,
        &InitialStoreMetadata::new(run_id, "2026-08-16T00:00:00Z", "configuration-sha256:d08"),
    )
    .unwrap();
    if let Some(outcome) = completed_outcome {
        store
            .connection()
            .execute(
                "UPDATE run_metadata SET ended_at = ?1, production_outcome = ?2, \
                 clean_shutdown = 1 WHERE singleton = 1",
                ["2026-08-16T00:00:01Z", outcome],
            )
            .unwrap();
    }
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

fn server_identity(entry: &RegistryEntry, run_id: CanonicalUuid) -> Vec<u8> {
    let bind = entry.bind();
    format!(
        concat!(
            "{{",
            "\"identity_schema_version\":1,",
            "\"server_protocol_version\":1,",
            "\"event_schema_version\":1,",
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
        run_id,
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
    observations: Mutex<HashMap<u32, ObservedProcessIdentity>>,
}

impl FakeProcesses {
    fn set(&self, pid: u32, observation: ObservedProcessIdentity) {
        self.observations.lock().unwrap().insert(pid, observation);
    }

    fn alive(&self, entry: &RegistryEntry) {
        self.set(
            entry.owner_pid(),
            ObservedProcessIdentity::Alive(entry.process_identity().clone()),
        );
    }
}

impl ProcessIdentityProbe for FakeProcesses {
    fn observe(&self, pid: u32) -> ObservedProcessIdentity {
        self.observations
            .lock()
            .unwrap()
            .get(&pid)
            .cloned()
            .unwrap_or(ObservedProcessIdentity::Unknown)
    }
}

#[derive(Default)]
struct FakeServers {
    responses: Mutex<HashMap<u32, Result<Vec<u8>, String>>>,
    calls: AtomicUsize,
}

impl FakeServers {
    fn identity(&self, entry: &RegistryEntry, run_id: CanonicalUuid) {
        self.responses
            .lock()
            .unwrap()
            .insert(entry.owner_pid(), Ok(server_identity(entry, run_id)));
    }

    fn matching(&self, entry: &RegistryEntry) {
        self.identity(entry, entry.run_id());
    }

    fn unreachable(&self, entry: &RegistryEntry) {
        self.responses.lock().unwrap().insert(
            entry.owner_pid(),
            Err("connection refused by test server".to_owned()),
        );
    }
}

impl ServerIdentityProbe for FakeServers {
    fn probe_identity(&self, entry: &RegistryEntry) -> Result<Vec<u8>, ServerProbeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.responses.lock().unwrap().get(&entry.owner_pid()) {
            Some(Ok(bytes)) => Ok(bytes.clone()),
            Some(Err(detail)) => Err(ServerProbeError::new(detail.clone())),
            None => Err(ServerProbeError::new("no fake identity endpoint")),
        }
    }
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        let mut children = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            let relative = child.strip_prefix(root).unwrap().to_path_buf();
            if child.is_dir() {
                entries.insert(relative, None);
                visit(root, &child, entries);
            } else {
                entries.insert(relative, Some(fs::read(&child).unwrap()));
            }
        }
    }

    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn complete_matrix() -> (TestProduction, FakeProcesses, FakeServers) {
    let production = TestProduction::new("matrix");
    let processes = FakeProcesses::default();
    let servers = FakeServers::default();

    let active = registry_entry(&production, run_id(1), 8_001);
    let gone = registry_entry(&production, run_id(2), 8_002);
    let reused = registry_entry(&production, run_id(3), 8_003);
    let unhealthy = registry_entry(&production, run_id(4), 8_004);
    let mismatch = registry_entry(&production, run_id(5), 8_005);
    for entry in [&active, &gone, &reused, &unhealthy, &mismatch] {
        create_archive(&production, entry.run_id(), None);
        publish_entry(&production, entry);
    }

    processes.alive(&active);
    servers.matching(&active);
    processes.set(gone.owner_pid(), ObservedProcessIdentity::DefinitelyGone);
    processes.set(
        reused.owner_pid(),
        ObservedProcessIdentity::Alive(other_process_identity(reused.owner_pid())),
    );
    processes.alive(&unhealthy);
    servers.unreachable(&unhealthy);
    processes.alive(&mismatch);
    servers.identity(&mismatch, run_id(999));

    let invalid_id = run_id(6);
    create_archive(&production, invalid_id, Some("completed"));
    fs::write(production.instance_path(invalid_id), b"not-json").unwrap();

    let incompatible_id = run_id(7);
    create_archive(&production, incompatible_id, None);
    fs::write(
        production.instance_path(incompatible_id),
        br#"{"registry_schema_version":2}"#,
    )
    .unwrap();

    create_archive(&production, run_id(8), Some("failed"));
    create_archive(&production, run_id(9), None);
    fs::write(production.instances().join("untrusted\"entry.json"), b"{}").unwrap();

    (production, processes, servers)
}

fn normalized(document: String, production: &TestProduction) -> String {
    document.replace(
        production.root().to_str().expect("temporary path is UTF-8"),
        "/srv/troupe/production",
    )
}

#[test]
fn listing_projects_the_complete_discovery_matrix_without_selecting_or_mutating_candidates() {
    let (production, processes, servers) = complete_matrix();
    let diagnostics = production.root().join(".troupe/diagnostics");
    let before = snapshot_tree(&diagnostics);

    let listing = list_runs_with(production.root(), &processes, &servers).unwrap();
    let repeated = list_runs_with(production.root(), &processes, &servers).unwrap();

    assert_eq!(listing.production(), production.root());
    assert_eq!(listing, repeated);
    assert!(
        listing
            .candidates()
            .windows(2)
            .all(|window| window[0].path() < window[1].path())
    );
    let counts = listing.candidates().iter().fold(
        BTreeMap::<CandidateClassification, usize>::new(),
        |mut counts, candidate| {
            *counts.entry(candidate.classification()).or_default() += 1;
            counts
        },
    );
    assert_eq!(counts[&CandidateClassification::Active], 1);
    assert_eq!(counts[&CandidateClassification::DefiniteStale], 2);
    assert_eq!(counts[&CandidateClassification::Unhealthy], 1);
    assert_eq!(counts[&CandidateClassification::IdentityMismatch], 1);
    assert_eq!(counts[&CandidateClassification::Invalid], 2);
    assert_eq!(counts[&CandidateClassification::Incompatible], 1);
    assert_eq!(counts[&CandidateClassification::Completed], 1);
    assert_eq!(counts[&CandidateClassification::Incomplete], 1);

    let untrusted = listing
        .candidates()
        .iter()
        .find(|candidate| candidate.path().ends_with("untrusted\"entry.json"))
        .unwrap();
    assert_eq!(untrusted.source(), "instance");
    assert_eq!(untrusted.run_id(), None);
    assert_eq!(untrusted.path_run_id(), None);
    let invalid_with_archive = listing
        .candidates()
        .iter()
        .find(|candidate| candidate.path_run_id() == Some(run_id(6)))
        .unwrap();
    assert_eq!(invalid_with_archive.run_id(), None);
    assert!(invalid_with_archive.archive_present());
    let failed_archive = listing
        .candidates()
        .iter()
        .find(|candidate| candidate.path_run_id() == Some(run_id(8)))
        .unwrap();
    assert_eq!(failed_archive.source(), "archive");
    assert_eq!(
        failed_archive.classification(),
        CandidateClassification::Completed
    );
    let incomplete_archive = listing
        .candidates()
        .iter()
        .find(|candidate| candidate.path_run_id() == Some(run_id(9)))
        .unwrap();
    assert_eq!(
        incomplete_archive.classification(),
        CandidateClassification::Incomplete
    );
    assert_eq!(servers.calls.load(Ordering::SeqCst), 6);

    assert_eq!(snapshot_tree(&diagnostics), before);
}

#[test]
fn human_and_json_documents_match_the_canonical_complete_matrix_fixtures() {
    let (production, processes, servers) = complete_matrix();
    let listing = list_runs_with(production.root(), &processes, &servers).unwrap();

    let human = normalized(listing.render(DocumentFormat::Human), &production);
    let json = normalized(listing.render(DocumentFormat::Json), &production);

    assert_eq!(human, HUMAN_FIXTURE);
    assert_eq!(json, JSON_FIXTURE);
    assert_eq!(json.bytes().filter(|byte| *byte == b'\n').count(), 1);
    assert!(json.ends_with('\n'));
}

#[test]
fn zero_candidates_is_a_successful_newline_terminated_empty_result() {
    let production = TestProduction::new("empty");
    let listing = list_runs_with(
        production.root(),
        &FakeProcesses::default(),
        &FakeServers::default(),
    )
    .unwrap();

    assert!(listing.candidates().is_empty());
    assert_eq!(
        listing.render(DocumentFormat::Human),
        format!(
            "production: {}\ncandidate_count: 0\n",
            production.root().display()
        )
    );
    assert_eq!(
        listing.render(DocumentFormat::Json),
        format!(
            "{{\"runs_schema_version\":1,\"production\":\"{}\",\"candidate_count\":\"0\",\"candidates\":[]}}\n",
            production.root().display()
        )
    );
}

#[tokio::test]
async fn async_command_seam_returns_only_the_requested_empty_document() {
    let production = TestProduction::new("execute-empty");

    let document = execute(RunsArgs {
        production: production.root().to_path_buf(),
        format: DocumentFormat::Json,
    })
    .await
    .unwrap();

    assert_eq!(
        document,
        format!(
            "{{\"runs_schema_version\":1,\"production\":\"{}\",\"candidate_count\":\"0\",\"candidates\":[]}}\n",
            production.root().display()
        )
    );
}

#[test]
fn invalid_production_is_an_operation_error_without_a_partial_document() {
    let production = TestProduction::new("missing");
    let missing = production.root().join("does-not-exist");
    let error =
        list_runs_with(&missing, &FakeProcesses::default(), &FakeServers::default()).unwrap_err();

    assert_eq!(error.code(), RunsErrorCode::InvalidProductionRoot);
    assert!(error.to_string().contains(&missing.display().to_string()));

    let regular_file = production.root().join("not-a-production-root");
    fs::write(&regular_file, b"not a directory").unwrap();
    let error = list_runs_with(
        &regular_file,
        &FakeProcesses::default(),
        &FakeServers::default(),
    )
    .unwrap_err();
    assert_eq!(error.code(), RunsErrorCode::InvalidProductionRoot);
    assert!(error.to_string().contains("not a directory"));
}
