use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::lease::{ActiveArchiveLease, CleanupArchiveLease, SharedArchiveLease},
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

#[path = "../src/application/diagnostic_cli/archive_target.rs"]
mod archive_target;
#[path = "../src/application/diagnostic_cli/args.rs"]
mod args;
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
use cleanup_policy::{
    CleanupInventoryRun, CleanupLeaseAvailability, CleanupLeaseProbe, CleanupOperationFailure,
    CleanupPolicyErrorCode, CleanupProtectionReason, CleanupSelectionReason, CleanupSkipReason,
    RealCleanupLeaseProbe, preview_from_inventory, preview_with, protection_for_classification,
};
use values::{ArchiveAge, ByteSize, Count, RunId};

const HUMAN_FIXTURE: &str =
    include_str!("../../tests/fixtures/diagnostics/cli/cleanup-preview-human.txt");
const JSON_FIXTURE: &str =
    include_str!("../../tests/fixtures/diagnostics/cli/cleanup-preview-v1.json");

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestProduction {
    root: PathBuf,
}

impl TestProduction {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "troupe-d05-{label}-{}-{sequence}",
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

    fn instances(&self) -> PathBuf {
        self.root.join(".troupe/diagnostics/instances")
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

fn run_policy(value: u64) -> CleanupPolicy {
    CleanupPolicy::Run(RunId::from_str(&run_id(value).to_string()).unwrap())
}

fn archive_path(value: u64) -> PathBuf {
    PathBuf::from(format!(
        "/srv/troupe/production/.troupe/diagnostics/runs/{}",
        run_id(value)
    ))
}

fn fixed_now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(9 * 86_400)
}

fn create_archive(
    production: &TestProduction,
    run_id: CanonicalUuid,
    started_at: &str,
    ended_at: Option<&str>,
) -> PathBuf {
    let directory = production.run_directory(run_id);
    fs::create_dir(&directory).unwrap();
    let active = ActiveArchiveLease::acquire(&directory).unwrap();
    let store = DiagnosticStore::create(
        &directory,
        &InitialStoreMetadata::new(run_id, started_at, "configuration-sha256:d05"),
    )
    .unwrap();
    if let Some(ended_at) = ended_at {
        store
            .connection()
            .execute(
                "UPDATE run_metadata SET ended_at = ?1, production_outcome = 'completed', \
                 clean_shutdown = 1 WHERE singleton = 1",
                [ended_at],
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
            &InitialStoreMetadata::new(run_id, "2026-08-16T00:00:00Z", "configuration-sha256:d05"),
        )
        .unwrap(),
    );
    (directory, active)
}

fn process_identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity::new("test", &format!("boot-d05-{pid}")).unwrap()
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
    values: Mutex<HashMap<u32, ObservedProcessIdentity>>,
}

impl FakeProcesses {
    fn alive(&self, entry: &RegistryEntry) {
        self.values.lock().unwrap().insert(
            entry.owner_pid(),
            ObservedProcessIdentity::Alive(entry.process_identity().clone()),
        );
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
            None => Err(ServerProbeError::new("no D05 fake identity endpoint")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct AvailableLease;

impl CleanupLeaseProbe for AvailableLease {
    fn probe(&self, _run_directory: &Path) -> CleanupLeaseAvailability {
        CleanupLeaseAvailability::Available
    }
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (String, Vec<u8>)> {
    fn visit(root: &Path, directory: &Path, snapshot: &mut BTreeMap<PathBuf, (String, Vec<u8>)>) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.file_type().is_symlink() {
                snapshot.insert(
                    relative,
                    (
                        "symlink".to_owned(),
                        fs::read_link(&path)
                            .unwrap()
                            .to_string_lossy()
                            .as_bytes()
                            .to_vec(),
                    ),
                );
            } else if metadata.is_dir() {
                snapshot.insert(relative, ("directory".to_owned(), Vec::new()));
                visit(root, &path, snapshot);
            } else {
                snapshot.insert(relative, ("file".to_owned(), fs::read(&path).unwrap()));
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn assert_tree_unchanged(
    before: &BTreeMap<PathBuf, (String, Vec<u8>)>,
    after: &BTreeMap<PathBuf, (String, Vec<u8>)>,
) {
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "preview changed the filesystem entry set"
    );
    for (path, (before_kind, before_bytes)) in before {
        let (after_kind, after_bytes) = &after[path];
        assert_eq!(after_kind, before_kind, "entry kind changed at {path:?}");
        if after_bytes != before_bytes {
            let first_difference = before_bytes
                .iter()
                .zip(after_bytes)
                .position(|(before, after)| before != after);
            panic!(
                "entry bytes changed at {path:?}: before_len={}, after_len={}, first_difference={first_difference:?}",
                before_bytes.len(),
                after_bytes.len()
            );
        }
    }
}

#[test]
fn exact_policy_selects_an_incomplete_archive_but_not_a_protected_target() {
    let incomplete =
        CleanupInventoryRun::incomplete(run_id(1), archive_path(1), 120, "2026-08-10T00:00:00Z")
            .unwrap();
    let protected = CleanupInventoryRun::protected(
        Some(run_id(2)),
        archive_path(2),
        Some(80),
        CleanupProtectionReason::Active,
    );

    let selected = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        run_policy(1),
        fixed_now(),
        vec![incomplete.clone(), protected.clone()],
    )
    .unwrap();
    assert_eq!(selected.runs().len(), 1);
    assert!(selected.runs()[0].selected());
    assert_eq!(
        selected.runs()[0].selection_reason(),
        Some(CleanupSelectionReason::ExactRun)
    );

    let blocked = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        run_policy(2),
        fixed_now(),
        vec![incomplete, protected],
    )
    .unwrap();
    assert!(!blocked.runs()[0].selected());
    assert_eq!(
        blocked.runs()[0].protection_reason(),
        Some(CleanupProtectionReason::Active)
    );
}

#[test]
fn exact_policy_reports_a_missing_run_as_a_typed_operation_error() {
    let error = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        run_policy(99),
        fixed_now(),
        Vec::new(),
    )
    .unwrap_err();
    assert_eq!(error.code(), CleanupPolicyErrorCode::TargetNotFound);
}

#[test]
fn unsafe_discovery_classifications_map_to_closed_protection_reasons() {
    for (classification, expected) in [
        (
            CandidateClassification::Active,
            Some(CleanupProtectionReason::Active),
        ),
        (
            CandidateClassification::Unhealthy,
            Some(CleanupProtectionReason::AmbiguousOwner),
        ),
        (
            CandidateClassification::IdentityMismatch,
            Some(CleanupProtectionReason::AmbiguousOwner),
        ),
        (
            CandidateClassification::Invalid,
            Some(CleanupProtectionReason::InvalidArchive),
        ),
        (
            CandidateClassification::Incompatible,
            Some(CleanupProtectionReason::IncompatibleArchive),
        ),
        (CandidateClassification::DefiniteStale, None),
        (CandidateClassification::Completed, None),
        (CandidateClassification::Incomplete, None),
    ] {
        assert_eq!(protection_for_classification(classification), expected);
    }
}

#[test]
fn older_than_uses_a_strict_cutoff_and_never_auto_selects_incomplete() {
    let old = CleanupInventoryRun::completed(
        run_id(1),
        archive_path(1),
        10,
        "1970-01-01T00:00:00Z",
        "1970-01-08T23:59:59.999999999Z",
    )
    .unwrap();
    let boundary = CleanupInventoryRun::completed(
        run_id(2),
        archive_path(2),
        20,
        "1970-01-02T00:00:00Z",
        "1970-01-09T00:00:00Z",
    )
    .unwrap();
    let incomplete =
        CleanupInventoryRun::incomplete(run_id(3), archive_path(3), 30, "1970-01-03T00:00:00Z")
            .unwrap();
    let preview = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        CleanupPolicy::OlderThan(ArchiveAge::from_str("1d").unwrap()),
        fixed_now(),
        vec![boundary, incomplete, old],
    )
    .unwrap();

    assert_eq!(preview.runs()[0].run_id(), Some(run_id(1)));
    assert!(preview.runs()[0].selected());
    assert_eq!(
        preview.runs()[1].skipped_reason(),
        Some(CleanupSkipReason::NewerThanCutoff)
    );
    assert_eq!(
        preview.runs()[2].skipped_reason(),
        Some(CleanupSkipReason::IncompleteArchive)
    );
}

#[test]
fn keep_runs_orders_by_ended_started_then_run_id_and_honors_zero() {
    let first = CleanupInventoryRun::completed(
        run_id(3),
        archive_path(3),
        30,
        "2026-08-10T00:00:00Z",
        "2026-08-11T00:00:00Z",
    )
    .unwrap();
    let tied_low_id = CleanupInventoryRun::completed(
        run_id(1),
        archive_path(1),
        10,
        "2026-08-12T00:00:00Z",
        "2026-08-13T00:00:00Z",
    )
    .unwrap();
    let earlier_started = CleanupInventoryRun::completed(
        run_id(4),
        archive_path(4),
        40,
        "2026-08-11T00:00:00Z",
        "2026-08-13T00:00:00Z",
    )
    .unwrap();
    let tied_high_id = CleanupInventoryRun::completed(
        run_id(2),
        archive_path(2),
        20,
        "2026-08-12T00:00:00Z",
        "2026-08-13T00:00:00Z",
    )
    .unwrap();
    let runs = vec![tied_high_id, earlier_started, first, tied_low_id];

    let keep_one = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        CleanupPolicy::KeepRuns(Count::new(1)),
        fixed_now(),
        runs.clone(),
    )
    .unwrap();
    assert_eq!(
        keep_one
            .runs()
            .iter()
            .map(CleanupInventoryRun::run_id)
            .collect::<Vec<_>>(),
        vec![
            Some(run_id(3)),
            Some(run_id(4)),
            Some(run_id(1)),
            Some(run_id(2)),
        ]
    );
    assert_eq!(
        keep_one
            .runs()
            .iter()
            .map(CleanupInventoryRun::selected)
            .collect::<Vec<_>>(),
        vec![true, true, true, false]
    );

    let keep_zero = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        CleanupPolicy::KeepRuns(Count::new(0)),
        fixed_now(),
        runs,
    )
    .unwrap();
    assert!(keep_zero.runs().iter().all(CleanupInventoryRun::selected));

    let older = CleanupInventoryRun::completed(
        run_id(4),
        archive_path(4),
        40,
        "2026-08-13T00:00:00Z",
        "2026-08-14T00:00:00Z",
    )
    .unwrap();
    let newest_leased = CleanupInventoryRun::completed(
        run_id(5),
        archive_path(5),
        50,
        "2026-08-14T00:00:00Z",
        "2026-08-15T00:00:00Z",
    )
    .unwrap()
    .with_protection(CleanupProtectionReason::Leased);
    let leased_in_keep_set = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        CleanupPolicy::KeepRuns(Count::new(1)),
        fixed_now(),
        vec![newest_leased, older],
    )
    .unwrap();
    assert!(leased_in_keep_set.runs()[0].selected());
    assert_eq!(
        leased_in_keep_set.runs()[1].protection_reason(),
        Some(CleanupProtectionReason::Leased)
    );
}

#[test]
fn max_total_bytes_selects_oldest_first_and_reports_protected_budget_failure() {
    let oldest = CleanupInventoryRun::completed(
        run_id(1),
        archive_path(1),
        300,
        "2026-08-09T00:00:00Z",
        "2026-08-10T00:00:00Z",
    )
    .unwrap();
    let newer = CleanupInventoryRun::completed(
        run_id(2),
        archive_path(2),
        200,
        "2026-08-10T00:00:00Z",
        "2026-08-11T00:00:00Z",
    )
    .unwrap();
    let protected = CleanupInventoryRun::protected(
        Some(run_id(3)),
        archive_path(3),
        Some(400),
        CleanupProtectionReason::Leased,
    );
    let preview = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        CleanupPolicy::MaxTotalBytes(ByteSize::from_str("300").unwrap()),
        fixed_now(),
        vec![protected, newer, oldest],
    )
    .unwrap();

    assert!(!preview.satisfied());
    assert_eq!(
        preview.operation_failure(),
        Some(CleanupOperationFailure::ProtectedBytesExceedBudget)
    );
    assert_eq!(preview.total_bytes(), Some(900));
    assert_eq!(preview.selected_bytes(), 500);
    assert_eq!(preview.remaining_bytes(), Some(400));
    assert!(preview.runs()[0].selected());
    assert!(preview.runs()[1].selected());
    assert_eq!(
        preview.runs()[2].protection_reason(),
        Some(CleanupProtectionReason::Leased)
    );
}

#[test]
fn max_total_bytes_fails_closed_when_protected_size_is_unknown() {
    let protected = CleanupInventoryRun::protected(
        Some(run_id(1)),
        archive_path(1),
        None,
        CleanupProtectionReason::InvalidArchive,
    );
    let preview = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        CleanupPolicy::MaxTotalBytes(ByteSize::from_str("1GiB").unwrap()),
        fixed_now(),
        vec![protected],
    )
    .unwrap();
    assert!(!preview.satisfied());
    assert_eq!(
        preview.operation_failure(),
        Some(CleanupOperationFailure::ProtectedBytesUnknown)
    );
    assert_eq!(preview.total_bytes(), None);
    assert_eq!(preview.remaining_bytes(), None);
}

#[test]
fn max_total_bytes_stops_when_remaining_bytes_equal_the_budget() {
    let oldest = CleanupInventoryRun::completed(
        run_id(1),
        archive_path(1),
        300,
        "2026-08-10T00:00:00Z",
        "2026-08-11T00:00:00Z",
    )
    .unwrap();
    let newest = CleanupInventoryRun::completed(
        run_id(2),
        archive_path(2),
        200,
        "2026-08-11T00:00:00Z",
        "2026-08-12T00:00:00Z",
    )
    .unwrap();
    let preview = preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        CleanupPolicy::MaxTotalBytes(ByteSize::from_str("200").unwrap()),
        fixed_now(),
        vec![newest, oldest],
    )
    .unwrap();

    assert!(preview.satisfied());
    assert!(preview.runs()[0].selected());
    assert_eq!(
        preview.runs()[1].skipped_reason(),
        Some(CleanupSkipReason::WithinTotalByteBudget)
    );
    assert_eq!(preview.remaining_bytes(), Some(200));
}

#[test]
fn scanner_classifies_complete_incomplete_and_active_without_mutating_the_tree() {
    let production = TestProduction::new("scanner-matrix");
    let completed_id = run_id(10);
    let incomplete_id = run_id(11);
    let active_id = run_id(12);
    let completed = create_archive(
        &production,
        completed_id,
        "2026-08-10T00:00:00Z",
        Some("2026-08-11T00:00:00Z"),
    );
    fs::write(completed.join("payload.bin"), [7_u8; 17]).unwrap();
    create_archive(&production, incomplete_id, "2026-08-12T00:00:00Z", None);
    let (_active_directory, active_lease) = create_active_archive(&production, active_id);
    let entry = registry_entry(&production, active_id, 8_012);
    publish_entry(&production, &entry);
    let processes = FakeProcesses::default();
    let servers = FakeServers::default();
    processes.alive(&entry);
    servers.matching(&entry);
    let before = snapshot_tree(production.root());

    let preview = preview_with(
        production.root(),
        CleanupPolicy::KeepRuns(Count::new(0)),
        fixed_now(),
        &processes,
        &servers,
        &RealCleanupLeaseProbe,
    )
    .unwrap();

    let after = snapshot_tree(production.root());
    assert_tree_unchanged(&before, &after);
    let by_id = preview
        .runs()
        .iter()
        .map(|run| (run.run_id().unwrap(), run))
        .collect::<HashMap<_, _>>();
    assert!(by_id[&completed_id].selected());
    assert!(by_id[&completed_id].bytes().unwrap() >= 17);
    assert_eq!(
        by_id[&incomplete_id].skipped_reason(),
        Some(CleanupSkipReason::IncompleteArchive)
    );
    assert_eq!(
        by_id[&active_id].protection_reason(),
        Some(CleanupProtectionReason::Active)
    );

    CleanupArchiveLease::acquire(&completed)
        .expect("preview's availability try-lock must be released before it returns");
    drop(active_lease);
}

#[test]
fn transient_availability_probe_reports_a_shared_reader_and_retains_no_cleanup_lease() {
    let production = TestProduction::new("leased-preview");
    let expected_id = run_id(20);
    let directory = create_archive(
        &production,
        expected_id,
        "2026-08-10T00:00:00Z",
        Some("2026-08-11T00:00:00Z"),
    );
    let reader = SharedArchiveLease::acquire(&directory).unwrap();
    let preview = preview_with(
        production.root(),
        run_policy(20),
        fixed_now(),
        &FakeProcesses::default(),
        &FakeServers::default(),
        &RealCleanupLeaseProbe,
    )
    .unwrap();
    assert_eq!(
        preview.runs()[0].protection_reason(),
        Some(CleanupProtectionReason::Leased)
    );
    assert!(!preview.runs()[0].selected());

    drop(reader);
    CleanupArchiveLease::acquire(&directory)
        .expect("D05 never holds a cleanup lease across preview; D11 owns that phase");
}

#[cfg(unix)]
#[test]
fn apparent_size_rejects_symlinks_without_following_or_modifying_the_target() {
    use std::os::unix::fs::symlink;

    let production = TestProduction::new("symlink-size");
    let expected_id = run_id(30);
    let directory = create_archive(
        &production,
        expected_id,
        "2026-08-10T00:00:00Z",
        Some("2026-08-11T00:00:00Z"),
    );
    let outside = production.root().join("outside.bin");
    let outside_bytes = vec![9_u8; 32 * 1024];
    fs::write(&outside, &outside_bytes).unwrap();
    symlink(&outside, directory.join("escape")).unwrap();

    let preview = preview_with(
        production.root(),
        run_policy(30),
        fixed_now(),
        &FakeProcesses::default(),
        &FakeServers::default(),
        &AvailableLease,
    )
    .unwrap();
    assert_eq!(
        preview.runs()[0].protection_reason(),
        Some(CleanupProtectionReason::UnsafeArchiveEntry)
    );
    assert_eq!(preview.runs()[0].bytes(), None);
    assert_eq!(fs::read(outside).unwrap(), outside_bytes);
}

fn fixture_preview() -> cleanup_policy::CleanupPreview {
    let runs = vec![
        CleanupInventoryRun::completed(
            run_id(1),
            archive_path(1),
            300,
            "2026-08-10T00:00:00Z",
            "2026-08-11T00:00:00Z",
        )
        .unwrap(),
        CleanupInventoryRun::completed(
            run_id(2),
            archive_path(2),
            200,
            "2026-08-11T00:00:00Z",
            "2026-08-12T00:00:00Z",
        )
        .unwrap(),
        CleanupInventoryRun::completed(
            run_id(3),
            archive_path(3),
            100,
            "2026-08-12T00:00:00Z",
            "2026-08-13T00:00:00Z",
        )
        .unwrap(),
        CleanupInventoryRun::incomplete(run_id(4), archive_path(4), 50, "2026-08-14T00:00:00Z")
            .unwrap(),
        CleanupInventoryRun::protected(
            Some(run_id(5)),
            archive_path(5),
            Some(100),
            CleanupProtectionReason::Active,
        ),
    ];
    preview_from_inventory(
        PathBuf::from("/srv/troupe/production"),
        CleanupPolicy::MaxTotalBytes(ByteSize::from_str("350").unwrap()),
        fixed_now(),
        runs,
    )
    .unwrap()
}

#[test]
fn preview_human_and_json_documents_are_frozen_fixtures() {
    let preview = fixture_preview();
    assert_eq!(preview.render(DocumentFormat::Human), HUMAN_FIXTURE);
    assert_eq!(preview.render(DocumentFormat::Json), JSON_FIXTURE);
}

#[test]
fn source_boundary_has_only_a_transient_probe_and_no_removal_authority() {
    let source = include_str!("../src/application/diagnostic_cli/cleanup_policy.rs");
    assert!(source.contains("SharedArchiveLease::acquire"));
    assert!(source.contains("probe_archive_lease_with"));
    assert!(!source.contains("CleanupArchiveLease"));
    assert!(!source.contains("fs::remove"));
    assert!(!source.contains("fs::rename"));
    assert!(source.contains("D11 is the only phase allowed"));
}
