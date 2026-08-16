use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::{
        constants::ARCHIVE_LEASE_ANCHOR_FILENAME,
        lease::{ActiveArchiveLease, SharedArchiveLease},
    },
    registry::{
        codec::encode_registry_entry,
        discover::{ProcessIdentityProbe, ServerIdentityProbe, ServerProbeError},
        model::{BindEndpoint, RegistryEntry},
        process_identity::{ObservedProcessIdentity, ProcessIdentity},
    },
    store::{
        connection::{DiagnosticStore, InitialStoreMetadata},
        schema::DIAGNOSTIC_DATABASE_FILENAME,
    },
};

#[path = "../src/application/diagnostic_cli/archive_target.rs"]
mod archive_target;
#[path = "../src/application/diagnostic_cli/args.rs"]
mod args;
#[path = "../src/application/diagnostic_cli/cleanup_apply.rs"]
mod cleanup_apply;
#[path = "../src/application/diagnostic_cli/cleanup_policy.rs"]
mod cleanup_policy;
#[path = "../src/application/diagnostic_cli/http_client.rs"]
mod http_client;
#[path = "../src/application/diagnostic_cli/resolver.rs"]
mod resolver;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use args::{CleanupPolicy, DocumentFormat};
use cleanup_apply::{
    CleanupApplyCheckpoint, CleanupApplyDisposition, CleanupApplyObserver, CleanupApplyReason,
    RealCleanupApplyObserver, apply_with,
};
use cleanup_policy::RealCleanupLeaseProbe;
use values::{ArchiveAge, RunId};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestProduction {
    root: PathBuf,
}

impl TestProduction {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "troupe-d11-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(root.join(".troupe/diagnostics/instances")).unwrap();
        fs::create_dir_all(root.join(".troupe/diagnostics/runs")).unwrap();
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn runs(&self) -> PathBuf {
        self.root.join(".troupe/diagnostics/runs")
    }

    fn diagnostics(&self) -> PathBuf {
        self.root.join(".troupe/diagnostics")
    }

    fn instances(&self) -> PathBuf {
        self.root.join(".troupe/diagnostics/instances")
    }

    fn run_directory(&self, run_id: CanonicalUuid) -> PathBuf {
        self.runs().join(run_id.to_string())
    }

    fn instance_path(&self, run_id: CanonicalUuid) -> PathBuf {
        self.instances().join(format!("{run_id}.json"))
    }

    fn tombstones(&self) -> Vec<PathBuf> {
        let mut values = fs::read_dir(self.diagnostics())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".troupe-cleanup-v1-"))
            })
            .collect::<Vec<_>>();
        values.sort();
        values
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

fn run_policy(value: u64) -> CleanupPolicy {
    CleanupPolicy::Run(RunId::from_str(&run_id(value).to_string()).unwrap())
}

fn older_than_policy() -> CleanupPolicy {
    CleanupPolicy::OlderThan(ArchiveAge::from_str("1h").unwrap())
}

fn fixed_now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(2_000_000_000)
}

fn create_archive(production: &TestProduction, run_id: CanonicalUuid, completed: bool) -> PathBuf {
    let directory = production.run_directory(run_id);
    fs::create_dir(&directory).unwrap();
    let active = ActiveArchiveLease::acquire(&directory).unwrap();
    let store = DiagnosticStore::create(
        &directory,
        &InitialStoreMetadata::new(run_id, "2026-01-01T00:00:00Z", "configuration-sha256:d11"),
    )
    .unwrap();
    if completed {
        store
            .connection()
            .execute(
                "UPDATE run_metadata SET ended_at = '2026-01-01T00:30:00Z', \
                 production_outcome = 'completed', clean_shutdown = 1 WHERE singleton = 1",
                [],
            )
            .unwrap();
    }
    drop(store);
    drop(active);
    directory
}

fn create_active_archive(
    production: &TestProduction,
    run_id: CanonicalUuid,
) -> (PathBuf, ActiveArchiveLease) {
    let directory = production.run_directory(run_id);
    fs::create_dir(&directory).unwrap();
    let active = ActiveArchiveLease::acquire(&directory).unwrap();
    drop(
        DiagnosticStore::create(
            &directory,
            &InitialStoreMetadata::new(run_id, "2026-01-01T00:00:00Z", "configuration-sha256:d11"),
        )
        .unwrap(),
    );
    (directory, active)
}

fn process_identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity::new("test", &format!("boot-d11-{pid}")).unwrap()
}

fn registry_entry(production: &TestProduction, run_id: CanonicalUuid, pid: u32) -> RegistryEntry {
    RegistryEntry::new(
        run_id,
        &production.run_directory(run_id),
        pid,
        process_identity(pid),
        BindEndpoint::new("127.0.0.1", 30_000 + (pid % 20_000) as u16).unwrap(),
        None,
        "2026-01-01T00:00:00Z",
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
    values: Mutex<HashMap<u32, ObservedProcessIdentity>>,
}

impl FakeProcesses {
    fn alive(&self, entry: &RegistryEntry) {
        self.values.lock().unwrap().insert(
            entry.owner_pid(),
            ObservedProcessIdentity::Alive(entry.process_identity().clone()),
        );
    }

    fn gone(&self, entry: &RegistryEntry) {
        self.values
            .lock()
            .unwrap()
            .insert(entry.owner_pid(), ObservedProcessIdentity::DefinitelyGone);
    }
}

impl ProcessIdentityProbe for FakeProcesses {
    fn observe(&self, pid: u32) -> ObservedProcessIdentity {
        self.values
            .lock()
            .unwrap()
            .get(&pid)
            .cloned()
            .unwrap_or(ObservedProcessIdentity::Unknown)
    }
}

#[derive(Default)]
struct FakeServers {
    values: Mutex<HashMap<u32, Result<Vec<u8>, String>>>,
}

impl FakeServers {
    fn matching(&self, entry: &RegistryEntry) {
        self.values
            .lock()
            .unwrap()
            .insert(entry.owner_pid(), Ok(matching_identity(entry)));
    }
}

impl ServerIdentityProbe for FakeServers {
    fn probe_identity(&self, entry: &RegistryEntry) -> Result<Vec<u8>, ServerProbeError> {
        match self.values.lock().unwrap().get(&entry.owner_pid()) {
            Some(Ok(bytes)) => Ok(bytes.clone()),
            Some(Err(detail)) => Err(ServerProbeError::new(detail.clone())),
            None => Err(ServerProbeError::new("no D11 fake identity endpoint")),
        }
    }
}

struct FailOnceObserver {
    run_id: CanonicalUuid,
    checkpoint: CleanupApplyCheckpoint,
    fired: AtomicBool,
}

impl FailOnceObserver {
    fn new(run_id: CanonicalUuid, checkpoint: CleanupApplyCheckpoint) -> Self {
        Self {
            run_id,
            checkpoint,
            fired: AtomicBool::new(false),
        }
    }
}

impl CleanupApplyObserver for FailOnceObserver {
    fn checkpoint(
        &self,
        run_id: CanonicalUuid,
        checkpoint: CleanupApplyCheckpoint,
    ) -> std::io::Result<()> {
        if run_id == self.run_id
            && checkpoint == self.checkpoint
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            Err(std::io::Error::other(format!(
                "injected D11 {checkpoint:?} failure"
            )))
        } else {
            Ok(())
        }
    }
}

struct ReplaceDatabaseObserver {
    run_id: CanonicalUuid,
    database: PathBuf,
    fired: AtomicBool,
}

impl CleanupApplyObserver for ReplaceDatabaseObserver {
    fn checkpoint(
        &self,
        run_id: CanonicalUuid,
        checkpoint: CleanupApplyCheckpoint,
    ) -> std::io::Result<()> {
        if run_id == self.run_id
            && checkpoint == CleanupApplyCheckpoint::BeforeExclusiveLease
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            let replacement = self.database.with_extension("replacement");
            fs::copy(&self.database, &replacement)?;
            fs::rename(replacement, &self.database)?;
        }
        Ok(())
    }
}

struct ActivateOwnerObserver {
    run_id: CanonicalUuid,
    entry: RegistryEntry,
    processes: Arc<FakeProcesses>,
    fired: AtomicBool,
}

impl CleanupApplyObserver for ActivateOwnerObserver {
    fn checkpoint(
        &self,
        run_id: CanonicalUuid,
        checkpoint: CleanupApplyCheckpoint,
    ) -> std::io::Result<()> {
        if run_id == self.run_id
            && checkpoint == CleanupApplyCheckpoint::AfterExclusiveLease
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            self.processes.alive(&self.entry);
        }
        Ok(())
    }
}

#[cfg(unix)]
struct AddSymlinkObserver {
    run_id: CanonicalUuid,
    run_directory: PathBuf,
    outside: PathBuf,
    fired: AtomicBool,
}

#[cfg(unix)]
impl CleanupApplyObserver for AddSymlinkObserver {
    fn checkpoint(
        &self,
        run_id: CanonicalUuid,
        checkpoint: CleanupApplyCheckpoint,
    ) -> std::io::Result<()> {
        if run_id == self.run_id
            && checkpoint == CleanupApplyCheckpoint::BeforeExclusiveLease
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            std::os::unix::fs::symlink(&self.outside, self.run_directory.join("outside-link"))?;
        }
        Ok(())
    }
}

fn apply_exact(
    production: &TestProduction,
    value: u64,
    processes: &dyn ProcessIdentityProbe,
    servers: &dyn ServerIdentityProbe,
    observer: &dyn CleanupApplyObserver,
) -> cleanup_apply::CleanupApplyReport {
    apply_with(
        production.root(),
        run_policy(value),
        fixed_now(),
        processes,
        servers,
        &RealCleanupLeaseProbe,
        observer,
    )
    .unwrap()
}

#[test]
fn exact_apply_deletes_complete_and_incomplete_whole_run_directories() {
    let production = TestProduction::new("exact");
    let complete = run_id(1);
    let incomplete = run_id(2);
    let complete_path = create_archive(&production, complete, true);
    let incomplete_path = create_archive(&production, incomplete, false);
    fs::create_dir(complete_path.join("nested")).unwrap();
    fs::write(complete_path.join("nested/payload"), b"whole directory").unwrap();
    let processes = FakeProcesses::default();
    let servers = FakeServers::default();

    let first = apply_exact(
        &production,
        1,
        &processes,
        &servers,
        &RealCleanupApplyObserver,
    );
    assert!(first.satisfied());
    assert_eq!(
        first.runs()[0].disposition(),
        CleanupApplyDisposition::Deleted
    );
    assert!(!complete_path.exists());
    assert!(incomplete_path.exists());

    let second = apply_exact(
        &production,
        2,
        &processes,
        &servers,
        &RealCleanupApplyObserver,
    );
    assert!(second.satisfied());
    assert!(!incomplete_path.exists());
    assert!(production.tombstones().is_empty());
}

#[test]
fn active_and_shared_reader_leases_are_never_deleted_by_exact_apply() {
    let production = TestProduction::new("leases");
    let active_id = run_id(10);
    let reader_id = run_id(11);
    let (active_path, active_lease) = create_active_archive(&production, active_id);
    let active_entry = registry_entry(&production, active_id, 41_010);
    publish_entry(&production, &active_entry);
    let reader_path = create_archive(&production, reader_id, true);
    let reader_lease = SharedArchiveLease::acquire(&reader_path).unwrap();
    let processes = FakeProcesses::default();
    processes.alive(&active_entry);
    let servers = FakeServers::default();
    servers.matching(&active_entry);

    let active = apply_exact(
        &production,
        10,
        &processes,
        &servers,
        &RealCleanupApplyObserver,
    );
    assert!(!active.satisfied());
    assert_eq!(
        active.runs()[0].reason(),
        CleanupApplyReason::ProtectedActive
    );
    assert!(active_path.exists());

    let reader = apply_exact(
        &production,
        11,
        &processes,
        &servers,
        &RealCleanupApplyObserver,
    );
    assert!(!reader.satisfied());
    assert_eq!(
        reader.runs()[0].reason(),
        CleanupApplyReason::ProtectedLeased
    );
    assert!(reader_path.exists());
    drop(reader_lease);
    drop(active_lease);
}

#[test]
fn definite_stale_registry_is_revalidated_and_durably_removed_with_archive() {
    let production = TestProduction::new("stale-registry");
    let id = run_id(20);
    let path = create_archive(&production, id, true);
    let entry = registry_entry(&production, id, 41_020);
    publish_entry(&production, &entry);
    let processes = FakeProcesses::default();
    processes.gone(&entry);
    let servers = FakeServers::default();

    let report = apply_exact(
        &production,
        20,
        &processes,
        &servers,
        &RealCleanupApplyObserver,
    );

    assert!(report.satisfied());
    assert!(!path.exists());
    assert!(!production.instance_path(id).exists());
}

#[test]
fn stale_owner_that_becomes_active_after_preview_is_revalidated_and_skipped() {
    let production = TestProduction::new("owner-reactivated");
    let id = run_id(21);
    let path = create_archive(&production, id, true);
    let entry = registry_entry(&production, id, 41_021);
    publish_entry(&production, &entry);
    let processes = Arc::new(FakeProcesses::default());
    processes.gone(&entry);
    let servers = FakeServers::default();
    servers.matching(&entry);
    let observer = ActivateOwnerObserver {
        run_id: id,
        entry: entry.clone(),
        processes: Arc::clone(&processes),
        fired: AtomicBool::new(false),
    };

    let report = apply_exact(&production, 21, processes.as_ref(), &servers, &observer);

    assert!(!report.satisfied());
    assert_eq!(report.runs()[0].reason(), CleanupApplyReason::Active);
    assert!(path.exists());
    assert!(production.instance_path(id).exists());
    assert!(production.tombstones().is_empty());
}

#[test]
fn store_identity_race_is_detected_after_exclusive_lease_upgrade() {
    let production = TestProduction::new("store-race");
    let id = run_id(30);
    let path = create_archive(&production, id, true);
    let observer = ReplaceDatabaseObserver {
        run_id: id,
        database: path.join(DIAGNOSTIC_DATABASE_FILENAME),
        fired: AtomicBool::new(false),
    };

    let report = apply_exact(
        &production,
        30,
        &FakeProcesses::default(),
        &FakeServers::default(),
        &observer,
    );

    assert!(!report.satisfied());
    assert_eq!(report.runs()[0].reason(), CleanupApplyReason::Raced);
    assert!(path.exists());
    assert!(production.tombstones().is_empty());
}

#[cfg(unix)]
#[test]
fn symlink_race_is_rejected_without_following_or_touching_external_target() {
    let production = TestProduction::new("symlink-race");
    let id = run_id(31);
    let path = create_archive(&production, id, true);
    let outside = production.root().join("outside-data");
    fs::write(&outside, b"must remain").unwrap();
    let observer = AddSymlinkObserver {
        run_id: id,
        run_directory: path.clone(),
        outside: outside.clone(),
        fired: AtomicBool::new(false),
    };

    let report = apply_exact(
        &production,
        31,
        &FakeProcesses::default(),
        &FakeServers::default(),
        &observer,
    );

    assert!(!report.satisfied());
    assert_eq!(fs::read(&outside).unwrap(), b"must remain");
    assert!(path.exists());
    assert!(production.tombstones().is_empty());
}

#[test]
fn rename_sync_failure_leaves_a_stable_tombstone_and_next_apply_recovers_it() {
    let production = TestProduction::new("recovery");
    let id = run_id(40);
    let path = create_archive(&production, id, true);
    let failing = FailOnceObserver::new(id, CleanupApplyCheckpoint::TargetParentSync);

    let first = apply_exact(
        &production,
        40,
        &FakeProcesses::default(),
        &FakeServers::default(),
        &failing,
    );
    assert!(!first.satisfied());
    assert_eq!(
        first.runs()[0].reason(),
        CleanupApplyReason::NamespaceSyncFailed
    );
    assert!(!path.exists());
    let tombstones = production.tombstones();
    assert_eq!(tombstones.len(), 1);
    assert!(fs::read_dir(&tombstones[0]).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".troupe-cleanup-intent-v1-")
    }));

    let recovered = apply_exact(
        &production,
        40,
        &FakeProcesses::default(),
        &FakeServers::default(),
        &RealCleanupApplyObserver,
    );
    assert!(recovered.satisfied());
    assert_eq!(
        recovered.runs()[0].reason(),
        CleanupApplyReason::RecoveredDeletion
    );
    assert!(production.tombstones().is_empty());
}

#[test]
fn recovery_fails_closed_when_a_durable_intent_loses_its_lease_anchor() {
    let production = TestProduction::new("recovery-anchor-missing");
    let id = run_id(41);
    create_archive(&production, id, true);
    let failing = FailOnceObserver::new(id, CleanupApplyCheckpoint::TargetParentSync);
    let first = apply_exact(
        &production,
        41,
        &FakeProcesses::default(),
        &FakeServers::default(),
        &failing,
    );
    assert!(!first.satisfied());
    let tombstone = production.tombstones().pop().unwrap();
    fs::remove_file(tombstone.join(ARCHIVE_LEASE_ANCHOR_FILENAME)).unwrap();

    let recovered = apply_exact(
        &production,
        41,
        &FakeProcesses::default(),
        &FakeServers::default(),
        &RealCleanupApplyObserver,
    );

    assert!(!recovered.satisfied());
    assert_eq!(
        recovered.runs()[0].reason(),
        CleanupApplyReason::RecoveryInvalid
    );
    assert!(tombstone.exists());
}

#[test]
fn batch_continues_other_runs_after_delete_failure_and_reports_unsatisfied() {
    let production = TestProduction::new("batch-partial");
    let first_id = run_id(50);
    let second_id = run_id(51);
    let third_id = run_id(52);
    let first = create_archive(&production, first_id, true);
    let second = create_archive(&production, second_id, true);
    let third = create_archive(&production, third_id, true);
    let observer = FailOnceObserver::new(first_id, CleanupApplyCheckpoint::DeleteTree);

    let report = apply_with(
        production.root(),
        older_than_policy(),
        fixed_now(),
        &FakeProcesses::default(),
        &FakeServers::default(),
        &RealCleanupLeaseProbe,
        &observer,
    )
    .unwrap();

    assert!(!report.satisfied());
    assert!(report.runs().iter().any(|run| {
        run.run_id() == Some(first_id) && run.disposition() == CleanupApplyDisposition::Failed
    }));
    assert!(!first.exists());
    assert!(!second.exists());
    assert!(!third.exists());
    assert_eq!(production.tombstones().len(), 1);
    assert_eq!(
        report
            .runs()
            .iter()
            .filter(|run| run.disposition() == CleanupApplyDisposition::Deleted)
            .count(),
        2
    );
}

#[test]
fn exact_deletion_is_isolated_from_a_concurrent_run_and_reports_stable_json() {
    let production = TestProduction::new("isolation-json");
    let deleted_id = run_id(60);
    let retained_id = run_id(61);
    let deleted = create_archive(&production, deleted_id, true);
    let retained = create_archive(&production, retained_id, true);
    let retained_before = fs::read(retained.join(DIAGNOSTIC_DATABASE_FILENAME)).unwrap();

    let report = apply_exact(
        &production,
        60,
        &FakeProcesses::default(),
        &FakeServers::default(),
        &RealCleanupApplyObserver,
    );
    let json = report.render(DocumentFormat::Json);
    let human = report.render(DocumentFormat::Human);

    assert!(report.satisfied());
    assert!(!deleted.exists());
    assert!(retained.exists());
    assert_eq!(
        fs::read(retained.join(DIAGNOSTIC_DATABASE_FILENAME)).unwrap(),
        retained_before
    );
    assert!(json.ends_with('\n'));
    assert!(json.contains("\"cleanup_apply_schema_version\":1"));
    assert!(json.contains(&format!("\"run_id\":\"{deleted_id}\"")));
    assert!(json.contains("\"disposition\":\"deleted\""));
    assert!(human.contains("policy_satisfied: true\n"));
    assert!(human.contains(&format!("  run_id: {deleted_id}\n")));
}

#[test]
fn delete_failure_reason_is_stable_and_does_not_claim_success() {
    let production = TestProduction::new("delete-failure");
    let id = run_id(70);
    create_archive(&production, id, true);
    let observer = FailOnceObserver::new(id, CleanupApplyCheckpoint::DeleteTree);

    let report = apply_exact(
        &production,
        70,
        &FakeProcesses::default(),
        &FakeServers::default(),
        &observer,
    );

    assert!(!report.satisfied());
    assert_eq!(report.runs()[0].reason(), CleanupApplyReason::DeleteFailed);
    assert_eq!(
        report.runs()[0].disposition(),
        CleanupApplyDisposition::Failed
    );
    assert_eq!(production.tombstones().len(), 1);
}
