use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use serde_json::{Value, json};
use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::lease::ActiveArchiveLease,
    registry::{
        codec::encode_registry_entry,
        discover::{
            CandidateClassification, CandidateSource, ProcessIdentityProbe,
            ServerIdentityDecodeErrorCode, ServerIdentityProbe, ServerProbeError,
            decode_server_identity, discover_with,
        },
        model::{BindEndpoint, RegistryEntry},
        process_identity::{ObservedProcessIdentity, ProcessIdentity},
        revalidate::{RevalidationStatus, revalidate_for_cleanup, revalidate_for_use},
    },
    store::connection::{DiagnosticStore, InitialStoreMetadata},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestProduction {
    root: PathBuf,
}

impl TestProduction {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "troupe-r02-{label}-{}-{sequence}",
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

fn create_archive(production: &TestProduction, run_id: CanonicalUuid, complete: bool) {
    let run_directory = production.run_directory(run_id);
    fs::create_dir(&run_directory).unwrap();
    let _active_lease = ActiveArchiveLease::acquire(&run_directory).unwrap();
    let store = DiagnosticStore::create(
        &run_directory,
        &InitialStoreMetadata::new(run_id, "2026-08-16T00:00:00Z", "configuration-sha256:test"),
    )
    .unwrap();
    if complete {
        store
            .connection()
            .execute(
                "UPDATE run_metadata SET ended_at = ?1, production_outcome = 'completed', \
                 clean_shutdown = 1 WHERE singleton = 1",
                ["2026-08-16T00:00:01Z"],
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

fn server_identity(entry: &RegistryEntry) -> Value {
    let bind = entry.bind();
    json!({
        "identity_schema_version": 1,
        "server_protocol_version": 1,
        "event_schema_version": 1,
        "api_schema_version": 1,
        "run_id": entry.run_id(),
        "owner_pid": entry.owner_pid(),
        "process_identity": entry.process_identity(),
        "bind_host": bind.host(),
        "port": bind.port(),
        "local_endpoint": entry.local_endpoint(),
        "advertise_url": entry.advertise_url(),
        "base_path": "/",
        "api_base_path": "/api/v1",
        "identity_path": "/api/v1/identity",
        "security_scope": "trusted_network",
        "operational_limits": {"max_page_rows": "500"},
    })
}

#[test]
fn strict_server_identity_decoder_is_reusable_without_a_registry_entry() {
    let production = TestProduction::new("typed-server-identity");
    let entry = registry_entry(&production, run_id(42), 8_042);
    let bytes = serde_json::to_vec(&server_identity(&entry)).unwrap();

    let identity = decode_server_identity(&bytes).expect("decode typed server identity");
    assert_eq!(identity.identity_schema_version(), 1);
    assert_eq!(identity.server_protocol_version(), 1);
    assert_eq!(identity.event_schema_version(), 1);
    assert_eq!(identity.api_schema_version(), 1);
    assert_eq!(identity.run_id(), entry.run_id());
    assert_eq!(identity.owner_pid(), entry.owner_pid());
    assert_eq!(identity.process_identity(), entry.process_identity());
    assert_eq!(identity.local_endpoint(), entry.local_endpoint());
    assert_eq!(identity.identity_path(), "/api/v1/identity");
    assert_eq!(identity.operational_limits()["max_page_rows"].get(), 500);

    let mut future = server_identity(&entry);
    future["identity_schema_version"] = json!(2);
    let error = decode_server_identity(&serde_json::to_vec(&future).unwrap())
        .expect_err("reject a newer identity schema before full decode");
    assert_eq!(error.code(), ServerIdentityDecodeErrorCode::Incompatible);

    let mut extra = server_identity(&entry);
    extra["unexpected"] = json!(true);
    let error = decode_server_identity(&serde_json::to_vec(&extra).unwrap())
        .expect_err("strict decoding rejects unknown fields");
    assert_eq!(error.code(), ServerIdentityDecodeErrorCode::Invalid);
}

#[derive(Default)]
struct FakeProcesses {
    observations: Mutex<HashMap<u32, VecDeque<ObservedProcessIdentity>>>,
}

impl FakeProcesses {
    fn set(&self, pid: u32, observations: impl IntoIterator<Item = ObservedProcessIdentity>) {
        self.observations
            .lock()
            .unwrap()
            .insert(pid, observations.into_iter().collect());
    }

    fn alive(&self, entry: &RegistryEntry) {
        self.set(
            entry.owner_pid(),
            [ObservedProcessIdentity::Alive(
                entry.process_identity().clone(),
            )],
        );
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

enum FakeServerResponse {
    Identity(Vec<u8>),
    Unreachable(String),
}

#[derive(Default)]
struct FakeServers {
    responses: Mutex<HashMap<u32, FakeServerResponse>>,
    calls: AtomicUsize,
}

impl FakeServers {
    fn identity(&self, entry: &RegistryEntry, identity: Value) {
        self.responses.lock().unwrap().insert(
            entry.owner_pid(),
            FakeServerResponse::Identity(serde_json::to_vec(&identity).unwrap()),
        );
    }

    fn matching(&self, entry: &RegistryEntry) {
        self.identity(entry, server_identity(entry));
    }

    fn unreachable(&self, entry: &RegistryEntry) {
        self.responses.lock().unwrap().insert(
            entry.owner_pid(),
            FakeServerResponse::Unreachable("connection refused".to_owned()),
        );
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ServerIdentityProbe for FakeServers {
    fn probe_identity(&self, entry: &RegistryEntry) -> Result<Vec<u8>, ServerProbeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.responses.lock().unwrap().get(&entry.owner_pid()) {
            Some(FakeServerResponse::Identity(bytes)) => Ok(bytes.clone()),
            Some(FakeServerResponse::Unreachable(detail)) => {
                Err(ServerProbeError::new(detail.clone()))
            }
            None => Err(ServerProbeError::new("no fake endpoint")),
        }
    }
}

#[test]
fn discovery_merges_instances_and_runs_into_the_complete_deterministic_state_matrix() {
    let production = TestProduction::new("matrix");
    let processes = FakeProcesses::default();
    let servers = FakeServers::default();

    let active = registry_entry(&production, run_id(1), 8_001);
    let gone = registry_entry(&production, run_id(2), 8_002);
    let reused = registry_entry(&production, run_id(3), 8_003);
    let unhealthy = registry_entry(&production, run_id(4), 8_004);
    let mismatch = registry_entry(&production, run_id(5), 8_005);
    for entry in [&active, &gone, &reused, &unhealthy, &mismatch] {
        create_archive(&production, entry.run_id(), false);
        publish_entry(&production, entry);
    }
    let _active_archive_lease =
        ActiveArchiveLease::acquire(&production.run_directory(active.run_id())).unwrap();

    processes.alive(&active);
    servers.matching(&active);
    processes.set(gone.owner_pid(), [ObservedProcessIdentity::DefinitelyGone]);
    processes.set(
        reused.owner_pid(),
        [ObservedProcessIdentity::Alive(other_process_identity(
            reused.owner_pid(),
        ))],
    );
    processes.alive(&unhealthy);
    servers.unreachable(&unhealthy);
    processes.alive(&mismatch);
    let mut wrong_identity = server_identity(&mismatch);
    wrong_identity["run_id"] = json!(run_id(999));
    servers.identity(&mismatch, wrong_identity);

    let invalid_id = run_id(6);
    create_archive(&production, invalid_id, true);
    fs::write(production.instance_path(invalid_id), b"not-json").unwrap();

    let incompatible_id = run_id(7);
    create_archive(&production, incompatible_id, false);
    fs::write(
        production.instance_path(incompatible_id),
        br#"{"registry_schema_version":2}"#,
    )
    .unwrap();

    let completed_id = run_id(8);
    create_archive(&production, completed_id, true);
    let incomplete_id = run_id(9);
    create_archive(&production, incomplete_id, false);
    fs::write(production.instances().join("untrusted.json"), b"{}").unwrap();

    let candidates = discover_with(production.root(), &processes, &servers).unwrap();
    assert!(
        candidates
            .windows(2)
            .all(|window| window[0].path() < window[1].path()),
        "candidate order must be path-deterministic"
    );
    let classifications =
        candidates
            .iter()
            .fold(BTreeMap::<_, usize>::new(), |mut counts, candidate| {
                *counts.entry(candidate.classification()).or_default() += 1;
                counts
            });
    assert_eq!(classifications[&CandidateClassification::Active], 1);
    assert_eq!(classifications[&CandidateClassification::DefiniteStale], 2);
    assert_eq!(classifications[&CandidateClassification::Unhealthy], 1);
    assert_eq!(
        classifications[&CandidateClassification::IdentityMismatch],
        1
    );
    assert_eq!(classifications[&CandidateClassification::Invalid], 2);
    assert_eq!(classifications[&CandidateClassification::Incompatible], 1);
    assert_eq!(classifications[&CandidateClassification::Completed], 1);
    assert_eq!(classifications[&CandidateClassification::Incomplete], 1);

    let active_candidate = candidates
        .iter()
        .find(|candidate| candidate.path_run_id() == Some(active.run_id()))
        .unwrap();
    assert_eq!(
        active_candidate.classification(),
        CandidateClassification::Active
    );
    assert_eq!(active_candidate.source(), CandidateSource::Instance);
    assert!(active_candidate.archive_present());
    assert_eq!(
        candidates
            .iter()
            .filter(|candidate| candidate.path_run_id() == Some(active.run_id()))
            .count(),
        1,
        "an active instance and its Run directory must merge into one candidate"
    );

    let invalid = candidates
        .iter()
        .find(|candidate| candidate.path_run_id() == Some(invalid_id))
        .unwrap();
    assert_eq!(invalid.run_id(), None);
    assert!(invalid.archive_present());
    assert!(invalid.is_potentially_live());
    let untrusted = candidates
        .iter()
        .find(|candidate| candidate.path().ends_with("untrusted.json"))
        .unwrap();
    assert_eq!(untrusted.run_id(), None);
    assert_eq!(untrusted.path_run_id(), None);
    assert_eq!(untrusted.source(), CandidateSource::Instance);

    let completed = candidates
        .iter()
        .find(|candidate| candidate.path_run_id() == Some(completed_id))
        .unwrap();
    assert_eq!(completed.source(), CandidateSource::Archive);
    assert!(!completed.is_potentially_live());
    assert_eq!(servers.calls(), 3, "stale entries must not probe a server");

    let repeated = discover_with(production.root(), &processes, &servers).unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| (candidate.path(), candidate.classification()))
            .collect::<Vec<_>>(),
        repeated
            .iter()
            .map(|candidate| (candidate.path(), candidate.classification()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn active_requires_process_and_strict_server_run_process_and_protocol_identity() {
    let production = TestProduction::new("identity");
    let processes = FakeProcesses::default();
    let servers = FakeServers::default();

    let unknown_process = registry_entry(&production, run_id(20), 8_020);
    let wrong_process = registry_entry(&production, run_id(21), 8_021);
    let wrong_protocol = registry_entry(&production, run_id(22), 8_022);
    let malformed_identity = registry_entry(&production, run_id(23), 8_023);
    for entry in [
        &unknown_process,
        &wrong_process,
        &wrong_protocol,
        &malformed_identity,
    ] {
        create_archive(&production, entry.run_id(), false);
        publish_entry(&production, entry);
    }

    processes.alive(&wrong_process);
    let mut process_mismatch = server_identity(&wrong_process);
    process_mismatch["process_identity"] = json!(other_process_identity(wrong_process.owner_pid()));
    servers.identity(&wrong_process, process_mismatch);

    processes.alive(&wrong_protocol);
    let mut protocol_mismatch = server_identity(&wrong_protocol);
    protocol_mismatch["server_protocol_version"] = json!(2);
    servers.identity(&wrong_protocol, protocol_mismatch);

    processes.alive(&malformed_identity);
    servers.responses.lock().unwrap().insert(
        malformed_identity.owner_pid(),
        FakeServerResponse::Identity(b"not-json".to_vec()),
    );

    let candidates = discover_with(production.root(), &processes, &servers).unwrap();
    let classification = |entry: &RegistryEntry| {
        candidates
            .iter()
            .find(|candidate| candidate.path_run_id() == Some(entry.run_id()))
            .unwrap()
            .classification()
    };
    assert_eq!(
        classification(&unknown_process),
        CandidateClassification::Unhealthy
    );
    assert_eq!(
        classification(&wrong_process),
        CandidateClassification::IdentityMismatch
    );
    assert_eq!(
        classification(&wrong_protocol),
        CandidateClassification::Incompatible
    );
    assert_eq!(
        classification(&malformed_identity),
        CandidateClassification::Unhealthy
    );
    assert_eq!(servers.calls(), 3);
}

#[test]
fn revalidation_denies_locator_and_process_identity_races_before_use_or_cleanup() {
    let production = TestProduction::new("revalidation");
    let processes = FakeProcesses::default();
    let servers = FakeServers::default();

    let active = registry_entry(&production, run_id(30), 8_030);
    let stale = registry_entry(&production, run_id(31), 8_031);
    for entry in [&active, &stale] {
        create_archive(&production, entry.run_id(), false);
        publish_entry(&production, entry);
        servers.matching(entry);
    }
    processes.alive(&active);
    processes.set(stale.owner_pid(), [ObservedProcessIdentity::DefinitelyGone]);

    let candidates = discover_with(production.root(), &processes, &servers).unwrap();
    let active_candidate = candidates
        .iter()
        .find(|candidate| candidate.path_run_id() == Some(active.run_id()))
        .unwrap()
        .clone();
    let stale_candidate = candidates
        .iter()
        .find(|candidate| candidate.path_run_id() == Some(stale.run_id()))
        .unwrap()
        .clone();

    assert!(revalidate_for_use(&active_candidate, &processes, &servers).is_authorized());
    assert!(
        revalidate_for_cleanup(&stale_candidate, &processes, &servers).is_authorized(),
        "a still-gone owner is the only cleanup authorization"
    );

    processes.set(
        stale.owner_pid(),
        [ObservedProcessIdentity::Alive(other_process_identity(
            stale.owner_pid(),
        ))],
    );
    assert!(
        revalidate_for_cleanup(&stale_candidate, &processes, &servers).is_authorized(),
        "a still-reused PID is also a cleanup authorization"
    );

    processes.alive(&stale);
    let revived = revalidate_for_cleanup(&stale_candidate, &processes, &servers);
    assert_eq!(revived.status(), RevalidationStatus::ClassificationChanged);
    assert_eq!(
        revived.observed_classification(),
        Some(CandidateClassification::Active)
    );
    assert!(!revived.is_authorized());
    assert!(revived.candidate().is_none());
    assert!(revived.observed_candidate().is_some());

    processes.set(
        active.owner_pid(),
        [
            ObservedProcessIdentity::Alive(active.process_identity().clone()),
            ObservedProcessIdentity::Alive(other_process_identity(active.owner_pid())),
        ],
    );
    let reused_during_probe = revalidate_for_use(&active_candidate, &processes, &servers);
    assert_eq!(
        reused_during_probe.status(),
        RevalidationStatus::ClassificationChanged
    );
    assert_eq!(
        reused_during_probe.observed_classification(),
        Some(CandidateClassification::DefiniteStale)
    );

    processes.alive(&active);
    let locator_path = production.instance_path(active.run_id());
    let replacement_path = production.instances().join("replacement.tmp");
    let original_bytes = fs::read(&locator_path).unwrap();
    fs::write(&replacement_path, original_bytes).unwrap();
    fs::rename(&replacement_path, &locator_path).unwrap();
    let replaced = revalidate_for_use(&active_candidate, &processes, &servers);
    assert_eq!(replaced.status(), RevalidationStatus::LocatorChanged);
    assert!(!replaced.is_authorized());
}
