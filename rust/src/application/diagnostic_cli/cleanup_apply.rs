#![allow(dead_code)] // D07 wires this private apply engine into the CLI dispatcher.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt::{self, Write as _},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write as _},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[cfg(all(unix, not(all(target_os = "linux", target_arch = "x86_64"))))]
use std::os::unix::fs::MetadataExt;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::{
    ffi::CString,
    os::{
        raw::c_long,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawFd,
        },
    },
};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::{
        constants::ARCHIVE_LEASE_ANCHOR_FILENAME,
        lease::{ArchiveLeaseErrorCode, CleanupArchiveLease},
    },
    registry::{
        codec::decode_registry_entry,
        discover::{
            CandidateClassification, CandidateSource, DiscoveryCandidate, ProcessIdentityProbe,
            RealProcessIdentityProbe, ServerIdentityProbe, discover_with,
        },
        model::RegistryEntry,
        revalidate::revalidate_for_cleanup,
    },
    store::schema::DIAGNOSTIC_DATABASE_FILENAME,
};
use uuid::Uuid;

use super::{
    archive_target::ArchiveTarget,
    args::{CleanupPolicy, DocumentFormat},
    cleanup_policy::{
        CleanupInventoryRun, CleanupLeaseProbe, CleanupProtectionReason, CleanupSkipReason,
        RealCleanupLeaseProbe, preview_with,
    },
    http_client::BlockingRegistryIdentityProbe,
};

const CLEANUP_APPLY_SCHEMA_VERSION: u16 = 1;
const CLEANUP_INTENT_PREFIX: &str = ".troupe-cleanup-intent-v1-";
const CLEANUP_INTENT_TEMP_PREFIX: &str = ".troupe-cleanup-intent-tmp-v1-";
const CLEANUP_TOMBSTONE_PREFIX: &str = ".troupe-cleanup-v1-";
const CLEANUP_INTENT_MODE: u32 = 0o600;
const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const O_DIRECTORY: i32 = 0o200000;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const O_NOFOLLOW: i32 = 0o400000;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const RENAME_NOREPLACE: u32 = 1;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_RENAMEAT2: c_long = 316;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupApplyErrorCode {
    InvalidProductionRoot,
    UnsupportedPlatform,
    HttpProbeFailed,
    PreviewFailed,
    RecoveryScanFailed,
    TaskFailed,
}

impl CleanupApplyErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidProductionRoot => "diagnostic_cleanup_apply.invalid_production_root",
            Self::UnsupportedPlatform => "diagnostic_cleanup_apply.unsupported_platform",
            Self::HttpProbeFailed => "diagnostic_cleanup_apply.http_probe_failed",
            Self::PreviewFailed => "diagnostic_cleanup_apply.preview_failed",
            Self::RecoveryScanFailed => "diagnostic_cleanup_apply.recovery_scan_failed",
            Self::TaskFailed => "diagnostic_cleanup_apply.task_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CleanupApplyError {
    code: CleanupApplyErrorCode,
    detail: String,
}

impl CleanupApplyError {
    fn new(code: CleanupApplyErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub(crate) const fn code(&self) -> CleanupApplyErrorCode {
        self.code
    }
}

impl fmt::Display for CleanupApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for CleanupApplyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupApplyDisposition {
    Deleted,
    Skipped,
    Retained,
    Failed,
}

impl CleanupApplyDisposition {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::Skipped => "skipped",
            Self::Retained => "retained",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupApplyReason {
    Deleted,
    RecoveredDeletion,
    ProtectedActive,
    ProtectedLeased,
    ProtectedAmbiguousOwner,
    ProtectedInvalidArchive,
    ProtectedIncompatibleArchive,
    ProtectedMissingArchive,
    ProtectedMetadataInvalid,
    ProtectedUnsafeArchiveEntry,
    IncompleteArchive,
    NewerThanCutoff,
    WithinKeepCount,
    WithinTotalByteBudget,
    NotSelected,
    Active,
    Leased,
    Raced,
    RegistryCleanupFailed,
    IntentFailed,
    RenameFailed,
    NamespaceSyncFailed,
    DeleteFailed,
    RecoveryConflict,
    RecoveryInvalid,
}

impl CleanupApplyReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::RecoveredDeletion => "recovered_deletion",
            Self::ProtectedActive => "protected_active",
            Self::ProtectedLeased => "protected_leased",
            Self::ProtectedAmbiguousOwner => "protected_ambiguous_owner",
            Self::ProtectedInvalidArchive => "protected_invalid_archive",
            Self::ProtectedIncompatibleArchive => "protected_incompatible_archive",
            Self::ProtectedMissingArchive => "protected_missing_archive",
            Self::ProtectedMetadataInvalid => "protected_metadata_invalid",
            Self::ProtectedUnsafeArchiveEntry => "protected_unsafe_archive_entry",
            Self::IncompleteArchive => "incomplete_archive",
            Self::NewerThanCutoff => "newer_than_cutoff",
            Self::WithinKeepCount => "within_keep_count",
            Self::WithinTotalByteBudget => "within_total_byte_budget",
            Self::NotSelected => "not_selected",
            Self::Active => "active",
            Self::Leased => "leased",
            Self::Raced => "raced",
            Self::RegistryCleanupFailed => "registry_cleanup_failed",
            Self::IntentFailed => "intent_failed",
            Self::RenameFailed => "rename_failed",
            Self::NamespaceSyncFailed => "namespace_sync_failed",
            Self::DeleteFailed => "delete_failed",
            Self::RecoveryConflict => "recovery_conflict",
            Self::RecoveryInvalid => "recovery_invalid",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupApplyPhase {
    Preview,
    Discovery,
    StoreValidation,
    Lease,
    RegistryRevalidation,
    Intent,
    RegistryUnpublish,
    Rename,
    TargetParentSync,
    SourceParentSync,
    Delete,
    DeleteParentSync,
    Recovery,
}

impl CleanupApplyPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Discovery => "discovery",
            Self::StoreValidation => "store_validation",
            Self::Lease => "lease",
            Self::RegistryRevalidation => "registry_revalidation",
            Self::Intent => "intent",
            Self::RegistryUnpublish => "registry_unpublish",
            Self::Rename => "rename",
            Self::TargetParentSync => "target_parent_sync",
            Self::SourceParentSync => "source_parent_sync",
            Self::Delete => "delete",
            Self::DeleteParentSync => "delete_parent_sync",
            Self::Recovery => "recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupApplyOperationFailure {
    PolicyUnsatisfied,
    MutationFailed,
    PostconditionFailed,
}

impl CleanupApplyOperationFailure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PolicyUnsatisfied => "policy_unsatisfied",
            Self::MutationFailed => "mutation_failed",
            Self::PostconditionFailed => "postcondition_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CleanupApplyRunResult {
    run_id: Option<CanonicalUuid>,
    source_path: PathBuf,
    tombstone_path: Option<PathBuf>,
    bytes: Option<u64>,
    disposition: CleanupApplyDisposition,
    reason: CleanupApplyReason,
    phase: Option<CleanupApplyPhase>,
    detail: Option<String>,
}

impl CleanupApplyRunResult {
    pub(crate) const fn run_id(&self) -> Option<CanonicalUuid> {
        self.run_id
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub(crate) fn tombstone_path(&self) -> Option<&Path> {
        self.tombstone_path.as_deref()
    }

    pub(crate) const fn disposition(&self) -> CleanupApplyDisposition {
        self.disposition
    }

    pub(crate) const fn reason(&self) -> CleanupApplyReason {
        self.reason
    }

    pub(crate) const fn phase(&self) -> Option<CleanupApplyPhase> {
        self.phase
    }

    fn deleted(
        run_id: CanonicalUuid,
        source_path: PathBuf,
        tombstone_path: PathBuf,
        bytes: Option<u64>,
        recovered: bool,
    ) -> Self {
        Self {
            run_id: Some(run_id),
            source_path,
            tombstone_path: Some(tombstone_path),
            bytes,
            disposition: CleanupApplyDisposition::Deleted,
            reason: if recovered {
                CleanupApplyReason::RecoveredDeletion
            } else {
                CleanupApplyReason::Deleted
            },
            phase: None,
            detail: None,
        }
    }

    fn failed(
        run_id: Option<CanonicalUuid>,
        source_path: PathBuf,
        tombstone_path: Option<PathBuf>,
        bytes: Option<u64>,
        reason: CleanupApplyReason,
        phase: CleanupApplyPhase,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            source_path,
            tombstone_path,
            bytes,
            disposition: CleanupApplyDisposition::Failed,
            reason,
            phase: Some(phase),
            detail: Some(detail.into()),
        }
    }

    fn skipped(
        run_id: Option<CanonicalUuid>,
        source_path: PathBuf,
        bytes: Option<u64>,
        reason: CleanupApplyReason,
        phase: CleanupApplyPhase,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            source_path,
            tombstone_path: None,
            bytes,
            disposition: CleanupApplyDisposition::Skipped,
            reason,
            phase: Some(phase),
            detail: Some(detail.into()),
        }
    }

    fn skipped_tombstone(
        run_id: CanonicalUuid,
        source_path: PathBuf,
        tombstone_path: PathBuf,
        reason: CleanupApplyReason,
        phase: CleanupApplyPhase,
        detail: impl Into<String>,
    ) -> Self {
        let mut result = Self::skipped(Some(run_id), source_path, None, reason, phase, detail);
        result.tombstone_path = Some(tombstone_path);
        result
    }

    fn from_unselected(run: &CleanupInventoryRun) -> Self {
        let (disposition, reason) = if let Some(reason) = run.protection_reason() {
            (CleanupApplyDisposition::Skipped, protection_reason(reason))
        } else {
            (
                CleanupApplyDisposition::Retained,
                run.skipped_reason()
                    .map(skip_reason)
                    .unwrap_or(CleanupApplyReason::NotSelected),
            )
        };
        Self {
            run_id: run.run_id(),
            source_path: run.path().to_path_buf(),
            tombstone_path: None,
            bytes: run.bytes(),
            disposition,
            reason,
            phase: Some(CleanupApplyPhase::Preview),
            detail: None,
        }
    }

    fn render_human(&self, output: &mut String, index: usize) {
        writeln!(output).expect("write to String");
        writeln!(output, "run {}:", index + 1).expect("write to String");
        write_optional_human(output, "run_id", self.run_id.as_ref());
        writeln!(output, "  source_path: {}", self.source_path.display()).expect("write to String");
        write_optional_human(
            output,
            "tombstone_path",
            self.tombstone_path.as_ref().map(|path| path.display()),
        );
        write_optional_human(output, "bytes", self.bytes.as_ref());
        writeln!(output, "  disposition: {}", self.disposition.as_str()).expect("write to String");
        writeln!(output, "  reason: {}", self.reason.as_str()).expect("write to String");
        write_optional_human(output, "phase", self.phase.map(CleanupApplyPhase::as_str));
        write_optional_human(output, "detail", self.detail.as_deref());
    }

    fn render_json(&self, output: &mut String) {
        output.push_str("{\"run_id\":");
        push_optional_display_json(output, self.run_id.as_ref());
        output.push_str(",\"source_path\":");
        push_json_string(output, &self.source_path.to_string_lossy());
        output.push_str(",\"tombstone_path\":");
        match &self.tombstone_path {
            Some(path) => push_json_string(output, &path.to_string_lossy()),
            None => output.push_str("null"),
        }
        output.push_str(",\"bytes\":");
        push_optional_display_json(output, self.bytes.as_ref());
        output.push_str(",\"disposition\":");
        push_json_string(output, self.disposition.as_str());
        output.push_str(",\"reason\":");
        push_json_string(output, self.reason.as_str());
        output.push_str(",\"phase\":");
        push_optional_string_json(output, self.phase.map(CleanupApplyPhase::as_str));
        output.push_str(",\"detail\":");
        push_optional_string_json(output, self.detail.as_deref());
        output.push('}');
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CleanupPolicyDisplay {
    kind: &'static str,
    value: String,
}

impl CleanupPolicyDisplay {
    fn new(policy: CleanupPolicy) -> Self {
        match policy {
            CleanupPolicy::Run(run_id) => Self {
                kind: "run",
                value: run_id.get().to_string(),
            },
            CleanupPolicy::OlderThan(age) => Self {
                kind: "older_than",
                value: age.get().as_secs().to_string(),
            },
            CleanupPolicy::KeepRuns(count) => Self {
                kind: "keep_runs",
                value: count.get().to_string(),
            },
            CleanupPolicy::MaxTotalBytes(size) => Self {
                kind: "max_total_bytes",
                value: size.bytes().to_string(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CleanupApplyReport {
    production: PathBuf,
    policy: CleanupPolicyDisplay,
    satisfied: bool,
    operation_failure: Option<CleanupApplyOperationFailure>,
    detail: Option<String>,
    runs: Vec<CleanupApplyRunResult>,
}

impl CleanupApplyReport {
    pub(crate) fn production(&self) -> &Path {
        &self.production
    }

    pub(crate) const fn satisfied(&self) -> bool {
        self.satisfied
    }

    pub(crate) const fn operation_failure(&self) -> Option<CleanupApplyOperationFailure> {
        self.operation_failure
    }

    pub(crate) fn runs(&self) -> &[CleanupApplyRunResult] {
        &self.runs
    }

    pub(crate) fn render(&self, format: DocumentFormat) -> String {
        match format {
            DocumentFormat::Human => self.render_human(),
            DocumentFormat::Json => self.render_json(),
        }
    }

    fn render_human(&self) -> String {
        let mut output = String::new();
        writeln!(output, "production: {}", self.production.display()).expect("write to String");
        writeln!(output, "policy: {}", self.policy.kind).expect("write to String");
        writeln!(output, "policy_value: {}", self.policy.value).expect("write to String");
        writeln!(output, "policy_satisfied: {}", self.satisfied).expect("write to String");
        write_optional_top_human(
            &mut output,
            "operation_error",
            self.operation_failure
                .map(CleanupApplyOperationFailure::as_str),
        );
        write_optional_top_human(&mut output, "detail", self.detail.as_deref());
        writeln!(output, "run_count: {}", self.runs.len()).expect("write to String");
        for (index, run) in self.runs.iter().enumerate() {
            run.render_human(&mut output, index);
        }
        output
    }

    fn render_json(&self) -> String {
        let mut output = String::new();
        write!(
            output,
            "{{\"cleanup_apply_schema_version\":{CLEANUP_APPLY_SCHEMA_VERSION},\"production\":"
        )
        .expect("write to String");
        push_json_string(&mut output, &self.production.to_string_lossy());
        output.push_str(",\"policy\":{\"kind\":");
        push_json_string(&mut output, self.policy.kind);
        output.push_str(",\"value\":");
        push_json_string(&mut output, &self.policy.value);
        write!(output, "}},\"policy_satisfied\":{}", self.satisfied).expect("write to String");
        output.push_str(",\"operation_error\":");
        push_optional_string_json(
            &mut output,
            self.operation_failure
                .map(CleanupApplyOperationFailure::as_str),
        );
        output.push_str(",\"detail\":");
        push_optional_string_json(&mut output, self.detail.as_deref());
        write!(output, ",\"run_count\":\"{}\",\"runs\":[", self.runs.len())
            .expect("write to String");
        for (index, run) in self.runs.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            run.render_json(&mut output);
        }
        output.push_str("]}\n");
        output
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupApplyCheckpoint {
    BeforeExclusiveLease,
    AfterExclusiveLease,
    IntentCreate,
    IntentSync,
    RegistryUnlink,
    RegistrySync,
    Rename,
    TargetParentSync,
    SourceParentSync,
    DeleteTree,
    DeleteParentSync,
}

pub(crate) trait CleanupApplyObserver: Send + Sync {
    fn checkpoint(
        &self,
        run_id: CanonicalUuid,
        checkpoint: CleanupApplyCheckpoint,
    ) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RealCleanupApplyObserver;

impl CleanupApplyObserver for RealCleanupApplyObserver {
    fn checkpoint(
        &self,
        _run_id: CanonicalUuid,
        _checkpoint: CleanupApplyCheckpoint,
    ) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) async fn apply(
    production: PathBuf,
    policy: CleanupPolicy,
) -> Result<CleanupApplyReport, CleanupApplyError> {
    tokio::task::spawn_blocking(move || {
        let server_probe = BlockingRegistryIdentityProbe::new().map_err(|error| {
            CleanupApplyError::new(CleanupApplyErrorCode::HttpProbeFailed, error.to_string())
        })?;
        apply_with(
            &production,
            policy,
            SystemTime::now(),
            &RealProcessIdentityProbe,
            &server_probe,
            &RealCleanupLeaseProbe,
            &RealCleanupApplyObserver,
        )
    })
    .await
    .map_err(|error| {
        CleanupApplyError::new(
            CleanupApplyErrorCode::TaskFailed,
            format!("cleanup apply task failed: {error}"),
        )
    })?
}

pub(crate) fn apply_with(
    production: &Path,
    policy: CleanupPolicy,
    now: SystemTime,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
    lease_probe: &dyn CleanupLeaseProbe,
    observer: &dyn CleanupApplyObserver,
) -> Result<CleanupApplyReport, CleanupApplyError> {
    let production = canonical_production_root(production)?;
    ensure_cleanup_platform()?;
    let exact_run = exact_run(policy);
    let tombstones = scan_tombstones(&production, exact_run)?;
    let mut grouped = BTreeMap::<CanonicalUuid, usize>::new();
    for tombstone in &tombstones {
        if let Ok(parsed) = parse_tombstone(tombstone) {
            *grouped.entry(parsed.run_id).or_default() += 1;
        }
    }

    let mut results = Vec::new();
    for tombstone in tombstones {
        let parsed = match parse_tombstone(&tombstone) {
            Ok(parsed) => parsed,
            Err(detail) => {
                results.push(CleanupApplyRunResult::failed(
                    None,
                    tombstone.clone(),
                    Some(tombstone),
                    None,
                    CleanupApplyReason::RecoveryInvalid,
                    CleanupApplyPhase::Recovery,
                    detail,
                ));
                continue;
            }
        };
        if grouped.get(&parsed.run_id).copied().unwrap_or_default() != 1 {
            results.push(CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                run_directory(&production, parsed.run_id),
                Some(tombstone),
                None,
                CleanupApplyReason::RecoveryConflict,
                CleanupApplyPhase::Recovery,
                "multiple cleanup tombstones exist for the same Run",
            ));
            continue;
        }
        results.push(recover_tombstone(
            &production,
            &tombstone,
            parsed,
            process_probe,
            server_probe,
            observer,
        ));
    }

    if let Some(run_id) = exact_run
        && results.iter().any(|result| result.run_id == Some(run_id))
    {
        let satisfied = results.iter().any(|result| {
            result.run_id == Some(run_id) && result.disposition == CleanupApplyDisposition::Deleted
        }) && results.iter().all(|result| {
            result.run_id != Some(run_id) || result.disposition == CleanupApplyDisposition::Deleted
        });
        return Ok(finish_report(production, policy, satisfied, None, results));
    }

    let preview = preview_with(
        &production,
        policy,
        now,
        process_probe,
        server_probe,
        lease_probe,
    )
    .map_err(|error| {
        CleanupApplyError::new(CleanupApplyErrorCode::PreviewFailed, error.to_string())
    })?;
    for run in preview.runs() {
        if run.selected() {
            results.push(apply_selected_run(
                &production,
                run,
                process_probe,
                server_probe,
                observer,
            ));
        } else {
            results.push(CleanupApplyRunResult::from_unselected(run));
        }
    }

    let hard_failure = results
        .iter()
        .any(|result| result.disposition == CleanupApplyDisposition::Failed);
    if let Some(run_id) = exact_run {
        let satisfied = results.iter().any(|result| {
            result.run_id == Some(run_id) && result.disposition == CleanupApplyDisposition::Deleted
        });
        return Ok(finish_report(production, policy, satisfied, None, results));
    }

    let postcondition = preview_with(
        &production,
        policy,
        now,
        process_probe,
        server_probe,
        lease_probe,
    );
    let (policy_satisfied, postcondition_detail) = match postcondition {
        Ok(preview) => (
            preview.operation_failure().is_none()
                && preview.runs().iter().all(|run| !run.selected()),
            None,
        ),
        Err(error) => (false, Some(error.to_string())),
    };
    Ok(finish_report(
        production,
        policy,
        policy_satisfied && !hard_failure,
        postcondition_detail,
        results,
    ))
}

fn finish_report(
    production: PathBuf,
    policy: CleanupPolicy,
    satisfied: bool,
    postcondition_detail: Option<String>,
    runs: Vec<CleanupApplyRunResult>,
) -> CleanupApplyReport {
    let mutation_failed = runs
        .iter()
        .any(|run| run.disposition == CleanupApplyDisposition::Failed);
    let operation_failure = if postcondition_detail.is_some() {
        Some(CleanupApplyOperationFailure::PostconditionFailed)
    } else if mutation_failed {
        Some(CleanupApplyOperationFailure::MutationFailed)
    } else if !satisfied {
        Some(CleanupApplyOperationFailure::PolicyUnsatisfied)
    } else {
        None
    };
    CleanupApplyReport {
        production,
        policy: CleanupPolicyDisplay::new(policy),
        satisfied,
        operation_failure,
        detail: postcondition_detail,
        runs,
    }
}

fn apply_selected_run(
    production: &Path,
    run: &CleanupInventoryRun,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
    observer: &dyn CleanupApplyObserver,
) -> CleanupApplyRunResult {
    let source = run.path().to_path_buf();
    let bytes = run.bytes();
    let Some(run_id) = run.run_id() else {
        return CleanupApplyRunResult::failed(
            None,
            source,
            None,
            bytes,
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Discovery,
            "selected cleanup inventory row has no trusted Run identity",
        );
    };
    if source != run_directory(production, run_id) {
        return CleanupApplyRunResult::failed(
            Some(run_id),
            source,
            None,
            bytes,
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Discovery,
            "selected Run path is outside the fixed production runs namespace",
        );
    }

    let candidate = match fresh_cleanup_candidate(production, run_id, process_probe, server_probe) {
        Ok(candidate) => candidate,
        Err(failure) => return failure.into_result(run_id, source, bytes),
    };
    let validated = match validate_archive_before_lock(&source, run_id) {
        Ok(validated) => validated,
        Err(detail) => {
            return CleanupApplyRunResult::skipped(
                Some(run_id),
                source,
                bytes,
                CleanupApplyReason::Raced,
                CleanupApplyPhase::StoreValidation,
                detail,
            );
        }
    };
    if let Err(error) = observer.checkpoint(run_id, CleanupApplyCheckpoint::BeforeExclusiveLease) {
        return CleanupApplyRunResult::skipped(
            Some(run_id),
            source,
            bytes,
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Lease,
            error.to_string(),
        );
    }
    let cleanup_lease = match CleanupArchiveLease::acquire(&source) {
        Ok(lease) => lease,
        Err(error) if error.code() == ArchiveLeaseErrorCode::Contended => {
            return CleanupApplyRunResult::skipped(
                Some(run_id),
                source,
                bytes,
                CleanupApplyReason::Leased,
                CleanupApplyPhase::Lease,
                error.to_string(),
            );
        }
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(run_id),
                source,
                None,
                bytes,
                CleanupApplyReason::Raced,
                CleanupApplyPhase::Lease,
                error.to_string(),
            );
        }
    };
    if let Err(error) = observer.checkpoint(run_id, CleanupApplyCheckpoint::AfterExclusiveLease) {
        return CleanupApplyRunResult::skipped(
            Some(run_id),
            source,
            bytes,
            CleanupApplyReason::Raced,
            CleanupApplyPhase::RegistryRevalidation,
            error.to_string(),
        );
    }
    if let Err(detail) = validated.revalidate(&source) {
        return CleanupApplyRunResult::skipped(
            Some(run_id),
            source,
            bytes,
            CleanupApplyReason::Raced,
            CleanupApplyPhase::StoreValidation,
            detail,
        );
    }

    let stale_registry = if candidate.classification() == CandidateClassification::DefiniteStale {
        let revalidated = revalidate_for_cleanup(&candidate, process_probe, server_probe);
        let Some(refreshed) = revalidated.candidate() else {
            let reason =
                if revalidated.observed_classification() == Some(CandidateClassification::Active) {
                    CleanupApplyReason::Active
                } else {
                    CleanupApplyReason::Raced
                };
            return CleanupApplyRunResult::skipped(
                Some(run_id),
                source,
                bytes,
                reason,
                CleanupApplyPhase::RegistryRevalidation,
                format!(
                    "definite-stale registry revalidation was refused: {:?}",
                    revalidated.status()
                ),
            );
        };
        Some(refreshed.clone())
    } else {
        None
    };

    let intent = match ensure_cleanup_intent(&source, run_id, observer) {
        Ok(intent) => intent,
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(run_id),
                source,
                None,
                bytes,
                CleanupApplyReason::IntentFailed,
                CleanupApplyPhase::Intent,
                error.to_string(),
            );
        }
    };
    let tombstone = tombstone_path(production, run_id, intent.attempt_id);

    if let Some(candidate) = stale_registry
        && let Err(error) = unpublish_stale_registry(&candidate, run_id, observer)
    {
        return CleanupApplyRunResult::failed(
            Some(run_id),
            source,
            None,
            bytes,
            CleanupApplyReason::RegistryCleanupFailed,
            CleanupApplyPhase::RegistryUnpublish,
            error.to_string(),
        );
    }

    if let Err(error) = observer.checkpoint(run_id, CleanupApplyCheckpoint::Rename) {
        return CleanupApplyRunResult::failed(
            Some(run_id),
            source,
            None,
            bytes,
            CleanupApplyReason::RenameFailed,
            CleanupApplyPhase::Rename,
            error.to_string(),
        );
    }
    let namespace = match rename_noreplace(&source, &tombstone) {
        Ok(namespace) => namespace,
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(run_id),
                source,
                None,
                bytes,
                CleanupApplyReason::RenameFailed,
                CleanupApplyPhase::Rename,
                error.to_string(),
            );
        }
    };
    if let Err(error) = revalidate_directory_identity(&tombstone, validated.root) {
        return CleanupApplyRunResult::failed(
            Some(run_id),
            source,
            Some(tombstone),
            bytes,
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Rename,
            error.to_string(),
        );
    }
    let diagnostics = diagnostics_directory(production);
    if let Err(error) = observer
        .checkpoint(run_id, CleanupApplyCheckpoint::TargetParentSync)
        .and_then(|()| sync_directory_identity(&diagnostics, namespace.target_parent))
    {
        return CleanupApplyRunResult::failed(
            Some(run_id),
            source,
            Some(tombstone),
            bytes,
            CleanupApplyReason::NamespaceSyncFailed,
            CleanupApplyPhase::TargetParentSync,
            error.to_string(),
        );
    }
    let runs = runs_directory(production);
    if let Err(error) = observer
        .checkpoint(run_id, CleanupApplyCheckpoint::SourceParentSync)
        .and_then(|()| sync_directory_identity(&runs, namespace.source_parent))
    {
        return CleanupApplyRunResult::failed(
            Some(run_id),
            source,
            Some(tombstone),
            bytes,
            CleanupApplyReason::NamespaceSyncFailed,
            CleanupApplyPhase::SourceParentSync,
            error.to_string(),
        );
    }
    if let Err(error) = observer
        .checkpoint(run_id, CleanupApplyCheckpoint::DeleteTree)
        .and_then(|()| remove_validated_directory(&tombstone, validated.root))
    {
        return CleanupApplyRunResult::failed(
            Some(run_id),
            source,
            Some(tombstone),
            bytes,
            CleanupApplyReason::DeleteFailed,
            CleanupApplyPhase::Delete,
            error.to_string(),
        );
    }
    if let Err(error) = observer
        .checkpoint(run_id, CleanupApplyCheckpoint::DeleteParentSync)
        .and_then(|()| sync_directory_identity(&diagnostics, namespace.target_parent))
    {
        return CleanupApplyRunResult::failed(
            Some(run_id),
            source,
            Some(tombstone),
            bytes,
            CleanupApplyReason::NamespaceSyncFailed,
            CleanupApplyPhase::DeleteParentSync,
            error.to_string(),
        );
    }
    drop(cleanup_lease);
    CleanupApplyRunResult::deleted(run_id, source, tombstone, bytes, false)
}

#[derive(Clone, Debug)]
struct AttemptFailure {
    disposition: CleanupApplyDisposition,
    reason: CleanupApplyReason,
    phase: CleanupApplyPhase,
    detail: String,
}

impl AttemptFailure {
    fn skipped(
        reason: CleanupApplyReason,
        phase: CleanupApplyPhase,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            disposition: CleanupApplyDisposition::Skipped,
            reason,
            phase,
            detail: detail.into(),
        }
    }

    fn failed(
        reason: CleanupApplyReason,
        phase: CleanupApplyPhase,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            disposition: CleanupApplyDisposition::Failed,
            reason,
            phase,
            detail: detail.into(),
        }
    }

    fn into_result(
        self,
        run_id: CanonicalUuid,
        source: PathBuf,
        bytes: Option<u64>,
    ) -> CleanupApplyRunResult {
        match self.disposition {
            CleanupApplyDisposition::Skipped => CleanupApplyRunResult::skipped(
                Some(run_id),
                source,
                bytes,
                self.reason,
                self.phase,
                self.detail,
            ),
            _ => CleanupApplyRunResult::failed(
                Some(run_id),
                source,
                None,
                bytes,
                self.reason,
                self.phase,
                self.detail,
            ),
        }
    }
}

fn fresh_cleanup_candidate(
    production: &Path,
    run_id: CanonicalUuid,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<DiscoveryCandidate, AttemptFailure> {
    let candidates = discover_with(production, process_probe, server_probe).map_err(|error| {
        AttemptFailure::failed(
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Discovery,
            error.to_string(),
        )
    })?;
    let mut matching = candidates
        .into_iter()
        .filter(|candidate| candidate.path_run_id() == Some(run_id));
    let Some(candidate) = matching.next() else {
        return Err(AttemptFailure::skipped(
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Discovery,
            "Run disappeared after cleanup preview",
        ));
    };
    if matching.next().is_some() {
        return Err(AttemptFailure::skipped(
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Discovery,
            "Run has multiple discovery candidates",
        ));
    }
    if !candidate.archive_present()
        || candidate.archive_directory() != Some(run_directory(production, run_id).as_path())
    {
        return Err(AttemptFailure::skipped(
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Discovery,
            "Run archive identity or fixed path changed after cleanup preview",
        ));
    }
    match candidate.classification() {
        CandidateClassification::Completed
        | CandidateClassification::Incomplete
        | CandidateClassification::DefiniteStale => Ok(candidate),
        CandidateClassification::Active => Err(AttemptFailure::skipped(
            CleanupApplyReason::Active,
            CleanupApplyPhase::Discovery,
            "Run became active after cleanup preview",
        )),
        classification => Err(AttemptFailure::skipped(
            CleanupApplyReason::Raced,
            CleanupApplyPhase::Discovery,
            format!(
                "Run changed to protected classification {} after cleanup preview",
                classification.as_str()
            ),
        )),
    }
}

#[derive(Clone, Copy, Debug)]
struct ValidatedArchive {
    root: NodeIdentity,
    database: NodeIdentity,
}

impl ValidatedArchive {
    fn revalidate(self, run_directory: &Path) -> Result<(), String> {
        revalidate_directory_identity(run_directory, self.root)
            .map_err(|error| error.to_string())?;
        let database = run_directory.join(DIAGNOSTIC_DATABASE_FILENAME);
        let observed = node_identity(&database).map_err(|error| error.to_string())?;
        if observed != self.database || observed.kind != NodeKind::RegularFile {
            return Err(
                "diagnostic store identity changed during cleanup lease upgrade".to_owned(),
            );
        }
        validate_directory_tree(run_directory).map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn validate_archive_before_lock(
    run_directory: &Path,
    run_id: CanonicalUuid,
) -> Result<ValidatedArchive, String> {
    let root = node_identity(run_directory).map_err(|error| error.to_string())?;
    if root.kind != NodeKind::Directory {
        return Err("Run archive path is not a regular directory".to_owned());
    }
    let database_path = run_directory.join(DIAGNOSTIC_DATABASE_FILENAME);
    let database = node_identity(&database_path).map_err(|error| error.to_string())?;
    if database.kind != NodeKind::RegularFile {
        return Err("diagnostic database is not a regular file".to_owned());
    }
    let mut target =
        ArchiveTarget::open_expected(run_directory, run_id).map_err(|error| error.to_string())?;
    let captured = target.capture().map_err(|error| error.to_string())?;
    if captured.metadata().run_id() != run_id {
        return Err("diagnostic store Run identity changed during cleanup validation".to_owned());
    }
    drop(captured);
    drop(target);
    let validated = ValidatedArchive { root, database };
    validated.revalidate(run_directory)?;
    Ok(validated)
}

#[derive(Clone, Copy, Debug)]
struct CleanupIntent {
    attempt_id: Uuid,
}

fn ensure_cleanup_intent(
    run_directory: &Path,
    run_id: CanonicalUuid,
    observer: &dyn CleanupApplyObserver,
) -> io::Result<CleanupIntent> {
    let directory = open_directory(run_directory)?;
    let intent_names = directory_names(&directory)?
        .into_iter()
        .filter(|name| name.to_string_lossy().starts_with(CLEANUP_INTENT_PREFIX))
        .collect::<Vec<_>>();
    if intent_names.len() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Run archive contains multiple cleanup intents",
        ));
    }
    if let Some(name) = intent_names.first() {
        let attempt_id = parse_intent_name(name)?;
        let path = child_path(&directory, name)?;
        validate_intent_file(&path, run_id)?;
        return Ok(CleanupIntent { attempt_id });
    }

    observer.checkpoint(run_id, CleanupApplyCheckpoint::IntentCreate)?;
    let attempt_id = Uuid::new_v4();
    let name = intent_name(attempt_id);
    let temporary_name = format!("{CLEANUP_INTENT_TEMP_PREFIX}{}", attempt_id.simple());
    let temporary_path = child_path(&directory, OsStr::new(&temporary_name))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(CLEANUP_INTENT_MODE);
    }
    let mut file = options.open(&temporary_path)?;
    if let Err(error) = file
        .write_all(intent_contents(run_id).as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        cleanup_intent_temporary(&directory, OsStr::new(&temporary_name));
        return Err(error);
    }
    drop(file);
    if let Err(error) =
        rename_entry_noreplace(&directory, OsStr::new(&temporary_name), OsStr::new(&name))
    {
        cleanup_intent_temporary(&directory, OsStr::new(&temporary_name));
        return Err(error);
    }
    let path = child_path(&directory, OsStr::new(&name))?;
    validate_intent_file(&path, run_id)?;
    observer.checkpoint(run_id, CleanupApplyCheckpoint::IntentSync)?;
    directory.file.sync_all()?;
    Ok(CleanupIntent { attempt_id })
}

fn cleanup_intent_temporary(directory: &OpenedDirectory, name: &OsStr) {
    if let Ok(path) = child_path(directory, name) {
        let _ = fs::remove_file(path);
        let _ = directory.file.sync_all();
    }
}

fn unpublish_stale_registry(
    candidate: &DiscoveryCandidate,
    run_id: CanonicalUuid,
    observer: &dyn CleanupApplyObserver,
) -> io::Result<()> {
    if candidate.source() != CandidateSource::Instance {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stale cleanup candidate is not a registry instance",
        ));
    }
    let expected = candidate.registry_entry().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "stale cleanup candidate has no validated registry entry",
        )
    })?;
    if expected.run_id() != run_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stale registry Run identity changed",
        ));
    }
    let path = candidate.path();
    let parent_path = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "registry path has no parent")
    })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "registry path has no filename")
    })?;
    let parent = open_directory(parent_path)?;
    let secured_path = child_path(&parent, name)?;
    validate_registry_file(&secured_path, expected)?;
    observer.checkpoint(run_id, CleanupApplyCheckpoint::RegistryUnlink)?;
    validate_registry_file(&secured_path, expected)?;
    fs::remove_file(&secured_path)?;
    observer.checkpoint(run_id, CleanupApplyCheckpoint::RegistrySync)?;
    parent.file.sync_all()
}

fn validate_registry_file(path: &Path, expected: &RegistryEntry) -> io::Result<()> {
    let before = node_identity(path)?;
    if before.kind != NodeKind::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry locator is not a regular file",
        ));
    }
    let mut file = open_regular_nofollow(path)?;
    let opened = NodeIdentity::from_metadata(&file.metadata()?);
    if opened != before {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry locator identity changed while opening",
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_REGISTRY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry locator exceeds the size limit",
        ));
    }
    let decoded = decode_registry_entry(path, &bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("registry locator is invalid: {error}"),
        )
    })?;
    if &decoded != expected || node_identity(path)? != before {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry locator identity or contents changed",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct ParsedTombstone {
    run_id: CanonicalUuid,
    attempt_id: Uuid,
}

fn scan_tombstones(
    production: &Path,
    exact_run: Option<CanonicalUuid>,
) -> Result<Vec<PathBuf>, CleanupApplyError> {
    let diagnostics = diagnostics_directory(production);
    let directory = match open_directory(&diagnostics) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(CleanupApplyError::new(
                CleanupApplyErrorCode::RecoveryScanFailed,
                format!(
                    "cannot scan cleanup intermediates under {}: {error}",
                    diagnostics.display()
                ),
            ));
        }
    };
    let mut tombstones = Vec::new();
    let names = directory_names(&directory).map_err(|error| {
        CleanupApplyError::new(
            CleanupApplyErrorCode::RecoveryScanFailed,
            format!("cannot read a cleanup intermediate entry: {error}"),
        )
    })?;
    for name in names {
        let encoded = name.to_string_lossy();
        if !encoded.starts_with(CLEANUP_TOMBSTONE_PREFIX) {
            continue;
        }
        let path = diagnostics.join(&name);
        if let Some(run_id) = exact_run {
            match parse_tombstone(&path) {
                Ok(parsed) if parsed.run_id == run_id => {}
                Err(_) if encoded.contains(&run_id.to_string()) => {}
                _ => continue,
            }
        }
        tombstones.push(path);
    }
    tombstones.sort();
    Ok(tombstones)
}

fn recover_tombstone(
    production: &Path,
    tombstone: &Path,
    parsed: ParsedTombstone,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
    observer: &dyn CleanupApplyObserver,
) -> CleanupApplyRunResult {
    let source = run_directory(production, parsed.run_id);
    match fs::symlink_metadata(&source) {
        Ok(_) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryConflict,
                CleanupApplyPhase::Recovery,
                "both the discoverable Run and its cleanup tombstone exist",
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Recovery,
                error.to_string(),
            );
        }
    }

    let root = match node_identity(tombstone) {
        Ok(identity) if identity.kind == NodeKind::Directory => identity,
        Ok(_) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Recovery,
                "cleanup tombstone is not a regular directory",
            );
        }
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Recovery,
                error.to_string(),
            );
        }
    };
    let directory = match open_directory(tombstone) {
        Ok(directory) => directory,
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Recovery,
                error.to_string(),
            );
        }
    };
    let names = match directory_names(&directory) {
        Ok(names) => names,
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Recovery,
                error.to_string(),
            );
        }
    };
    let expected_intent = OsString::from(intent_name(parsed.attempt_id));
    let intent_names = names
        .iter()
        .filter(|name| name.to_string_lossy().starts_with(CLEANUP_INTENT_PREFIX))
        .collect::<Vec<_>>();
    let has_intent =
        intent_names.len() == 1 && intent_names[0].as_os_str() == expected_intent.as_os_str();
    let has_anchor = names
        .iter()
        .any(|name| name == OsStr::new(ARCHIVE_LEASE_ANCHOR_FILENAME));
    if has_intent && !has_anchor {
        return CleanupApplyRunResult::failed(
            Some(parsed.run_id),
            source,
            Some(tombstone.to_path_buf()),
            None,
            CleanupApplyReason::RecoveryInvalid,
            CleanupApplyPhase::Recovery,
            "cleanup tombstone intent exists without its exclusive lease anchor",
        );
    }
    if has_intent {
        let path = match child_path(&directory, &expected_intent) {
            Ok(path) => path,
            Err(error) => {
                return CleanupApplyRunResult::failed(
                    Some(parsed.run_id),
                    source,
                    Some(tombstone.to_path_buf()),
                    None,
                    CleanupApplyReason::RecoveryInvalid,
                    CleanupApplyPhase::Recovery,
                    error.to_string(),
                );
            }
        };
        if let Err(error) = validate_intent_file(&path, parsed.run_id) {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Recovery,
                error.to_string(),
            );
        }
    } else if !intent_names.is_empty()
        || names
            .iter()
            .any(|name| name != OsStr::new(ARCHIVE_LEASE_ANCHOR_FILENAME))
    {
        return CleanupApplyRunResult::failed(
            Some(parsed.run_id),
            source,
            Some(tombstone.to_path_buf()),
            None,
            CleanupApplyReason::RecoveryInvalid,
            CleanupApplyPhase::Recovery,
            "cleanup tombstone has no matching durable intent",
        );
    }

    let candidates = match discover_with(production, process_probe, server_probe) {
        Ok(candidates) => candidates,
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Discovery,
                error.to_string(),
            );
        }
    };
    let mut matching = candidates
        .into_iter()
        .filter(|candidate| candidate.path_run_id() == Some(parsed.run_id));
    let stale_registry = match (matching.next(), matching.next()) {
        (None, None) => None,
        (Some(candidate), None)
            if candidate.source() == CandidateSource::Instance
                && candidate.classification() == CandidateClassification::DefiniteStale
                && !candidate.archive_present() =>
        {
            Some(candidate)
        }
        (Some(candidate), None)
            if candidate.classification() == CandidateClassification::Active =>
        {
            return CleanupApplyRunResult::skipped_tombstone(
                parsed.run_id,
                source,
                tombstone.to_path_buf(),
                CleanupApplyReason::Active,
                CleanupApplyPhase::Recovery,
                "an active registry owner blocks cleanup tombstone recovery",
            );
        }
        _ => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryConflict,
                CleanupApplyPhase::Recovery,
                "registry or archive discovery conflicts with cleanup tombstone recovery",
            );
        }
    };

    let cleanup_lease = if has_anchor {
        match CleanupArchiveLease::acquire(tombstone) {
            Ok(lease) => Some(lease),
            Err(error) if error.code() == ArchiveLeaseErrorCode::Contended => {
                return CleanupApplyRunResult::skipped_tombstone(
                    parsed.run_id,
                    source,
                    tombstone.to_path_buf(),
                    CleanupApplyReason::Leased,
                    CleanupApplyPhase::Recovery,
                    error.to_string(),
                );
            }
            Err(error) => {
                return CleanupApplyRunResult::failed(
                    Some(parsed.run_id),
                    source,
                    Some(tombstone.to_path_buf()),
                    None,
                    CleanupApplyReason::RecoveryInvalid,
                    CleanupApplyPhase::Recovery,
                    error.to_string(),
                );
            }
        }
    } else {
        None
    };
    if let Err(error) =
        observer.checkpoint(parsed.run_id, CleanupApplyCheckpoint::AfterExclusiveLease)
    {
        return CleanupApplyRunResult::failed(
            Some(parsed.run_id),
            source,
            Some(tombstone.to_path_buf()),
            None,
            CleanupApplyReason::RecoveryConflict,
            CleanupApplyPhase::Recovery,
            error.to_string(),
        );
    }
    match fs::symlink_metadata(&source) {
        Ok(_) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryConflict,
                CleanupApplyPhase::Recovery,
                "discoverable Run reappeared during cleanup tombstone recovery",
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Recovery,
                error.to_string(),
            );
        }
    }
    if let Some(candidate) = stale_registry {
        let revalidated = revalidate_for_cleanup(&candidate, process_probe, server_probe);
        let Some(refreshed) = revalidated.candidate() else {
            return CleanupApplyRunResult::skipped_tombstone(
                parsed.run_id,
                source,
                tombstone.to_path_buf(),
                CleanupApplyReason::Raced,
                CleanupApplyPhase::RegistryRevalidation,
                format!(
                    "stale registry changed during recovery: {:?}",
                    revalidated.status()
                ),
            );
        };
        if let Err(error) = unpublish_stale_registry(refreshed, parsed.run_id, observer) {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RegistryCleanupFailed,
                CleanupApplyPhase::RegistryUnpublish,
                error.to_string(),
            );
        }
    }
    if let Err(error) = validate_directory_tree(tombstone) {
        return CleanupApplyRunResult::failed(
            Some(parsed.run_id),
            source,
            Some(tombstone.to_path_buf()),
            None,
            CleanupApplyReason::RecoveryInvalid,
            CleanupApplyPhase::Recovery,
            error.to_string(),
        );
    }
    let diagnostics = diagnostics_directory(production);
    let diagnostics_identity = match open_directory(&diagnostics) {
        Ok(directory) => directory.identity,
        Err(error) => {
            return CleanupApplyRunResult::failed(
                Some(parsed.run_id),
                source,
                Some(tombstone.to_path_buf()),
                None,
                CleanupApplyReason::RecoveryInvalid,
                CleanupApplyPhase::Recovery,
                error.to_string(),
            );
        }
    };
    if let Err(error) = observer
        .checkpoint(parsed.run_id, CleanupApplyCheckpoint::DeleteTree)
        .and_then(|()| remove_validated_directory(tombstone, root))
    {
        return CleanupApplyRunResult::failed(
            Some(parsed.run_id),
            source,
            Some(tombstone.to_path_buf()),
            None,
            CleanupApplyReason::DeleteFailed,
            CleanupApplyPhase::Delete,
            error.to_string(),
        );
    }
    if let Err(error) = observer
        .checkpoint(parsed.run_id, CleanupApplyCheckpoint::DeleteParentSync)
        .and_then(|()| sync_directory_identity(&diagnostics, diagnostics_identity))
    {
        return CleanupApplyRunResult::failed(
            Some(parsed.run_id),
            source,
            Some(tombstone.to_path_buf()),
            None,
            CleanupApplyReason::NamespaceSyncFailed,
            CleanupApplyPhase::DeleteParentSync,
            error.to_string(),
        );
    }
    drop(cleanup_lease);
    CleanupApplyRunResult::deleted(parsed.run_id, source, tombstone.to_path_buf(), None, true)
}

fn parse_tombstone(path: &Path) -> Result<ParsedTombstone, String> {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "cleanup tombstone name is not UTF-8".to_owned())?;
    let suffix = name
        .strip_prefix(CLEANUP_TOMBSTONE_PREFIX)
        .ok_or_else(|| "cleanup tombstone uses an unknown name".to_owned())?;
    if !suffix.is_ascii() || suffix.len() != 36 + 1 + 32 || suffix.as_bytes()[36] != b'-' {
        return Err("cleanup tombstone name is malformed".to_owned());
    }
    let run_id = CanonicalUuid::parse(&suffix[..36])
        .map_err(|_| "cleanup tombstone Run identity is invalid".to_owned())?;
    let encoded_attempt = &suffix[37..];
    let attempt_id = Uuid::parse_str(encoded_attempt)
        .map_err(|_| "cleanup tombstone attempt identity is invalid".to_owned())?;
    if attempt_id.simple().to_string() != encoded_attempt {
        return Err("cleanup tombstone attempt identity is not canonical".to_owned());
    }
    Ok(ParsedTombstone { run_id, attempt_id })
}

fn tombstone_path(production: &Path, run_id: CanonicalUuid, attempt_id: Uuid) -> PathBuf {
    diagnostics_directory(production).join(format!(
        "{CLEANUP_TOMBSTONE_PREFIX}{run_id}-{}",
        attempt_id.simple()
    ))
}

fn intent_name(attempt_id: Uuid) -> String {
    format!("{CLEANUP_INTENT_PREFIX}{}", attempt_id.simple())
}

fn parse_intent_name(name: &OsStr) -> io::Result<Uuid> {
    let name = name.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "cleanup intent name is not UTF-8",
        )
    })?;
    let encoded = name.strip_prefix(CLEANUP_INTENT_PREFIX).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "cleanup intent name uses an unknown prefix",
        )
    })?;
    let attempt = Uuid::parse_str(encoded).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "cleanup intent attempt identity is invalid",
        )
    })?;
    if encoded.len() != 32 || attempt.simple().to_string() != encoded {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cleanup intent attempt identity is not canonical",
        ));
    }
    Ok(attempt)
}

fn intent_contents(run_id: CanonicalUuid) -> String {
    format!("troupe.cleanup.intent.v1\nrun_id={run_id}\n")
}

fn validate_intent_file(path: &Path, run_id: CanonicalUuid) -> io::Result<()> {
    let expected = intent_contents(run_id);
    let mut file = open_regular_nofollow(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes != expected.as_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cleanup intent contents do not match the Run identity",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NodeIdentity {
    kind: NodeKind,
    first: u64,
    second: u64,
}

impl NodeIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            NodeKind::Symlink
        } else if file_type.is_dir() {
            NodeKind::Directory
        } else if file_type.is_file() {
            NodeKind::RegularFile
        } else {
            NodeKind::Other
        };
        #[cfg(unix)]
        let (first, second) = (metadata.dev(), metadata.ino());
        #[cfg(not(unix))]
        let (first, second) = (metadata.len(), u64::from(metadata.permissions().readonly()));
        Self {
            kind,
            first,
            second,
        }
    }
}

fn node_identity(path: &Path) -> io::Result<NodeIdentity> {
    fs::symlink_metadata(path).map(|metadata| NodeIdentity::from_metadata(&metadata))
}

struct OpenedDirectory {
    file: File,
    path: PathBuf,
    identity: NodeIdentity,
}

fn open_directory(path: &Path) -> io::Result<OpenedDirectory> {
    let before = node_identity(path)?;
    if before.kind != NodeKind::Directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular directory", path.display()),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    options.custom_flags(O_DIRECTORY | O_NOFOLLOW);
    let file = options.open(path)?;
    let opened = NodeIdentity::from_metadata(&file.metadata()?);
    if opened != before || opened.kind != NodeKind::Directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} changed while opening", path.display()),
        ));
    }
    Ok(OpenedDirectory {
        file,
        path: path.to_path_buf(),
        identity: opened,
    })
}

fn open_regular_nofollow(path: &Path) -> io::Result<File> {
    let before = node_identity(path)?;
    if before.kind != NodeKind::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    options.custom_flags(O_NOFOLLOW);
    let file = options.open(path)?;
    if NodeIdentity::from_metadata(&file.metadata()?) != before {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} changed while opening", path.display()),
        ));
    }
    Ok(file)
}

fn directory_path(directory: &OpenedDirectory) -> PathBuf {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        PathBuf::from(format!("/proc/self/fd/{}", directory.file.as_raw_fd()))
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        directory.path.clone()
    }
}

fn directory_names(directory: &OpenedDirectory) -> io::Result<Vec<OsString>> {
    let mut names = fs::read_dir(directory_path(directory))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

fn child_path(directory: &OpenedDirectory, name: &OsStr) -> io::Result<PathBuf> {
    let candidate = Path::new(name);
    if name.is_empty()
        || name == OsStr::new(".")
        || name == OsStr::new("..")
        || candidate.components().count() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory entry name is invalid",
        ));
    }
    Ok(directory_path(directory).join(name))
}

fn validate_directory_tree(path: &Path) -> io::Result<NodeIdentity> {
    let root = open_directory(path)?;
    validate_open_directory(&root)?;
    Ok(root.identity)
}

fn validate_open_directory(directory: &OpenedDirectory) -> io::Result<()> {
    for name in directory_names(directory)? {
        let path = child_path(directory, &name)?;
        match node_identity(&path)? {
            identity @ NodeIdentity {
                kind: NodeKind::Directory,
                ..
            } => {
                let child = open_directory(&path)?;
                if child.identity != identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive directory entry changed while opening",
                    ));
                }
                validate_open_directory(&child)?;
            }
            NodeIdentity {
                kind: NodeKind::RegularFile,
                ..
            } => {}
            NodeIdentity {
                kind: NodeKind::Symlink,
                ..
            } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("archive symlink is rejected: {}", path.display()),
                ));
            }
            NodeIdentity {
                kind: NodeKind::Other,
                ..
            } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("archive special entry is rejected: {}", path.display()),
                ));
            }
        }
    }
    Ok(())
}

fn remove_validated_directory(path: &Path, expected: NodeIdentity) -> io::Result<()> {
    let root = open_directory(path)?;
    if root.identity != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cleanup tombstone identity changed before deletion",
        ));
    }
    validate_open_directory(&root)?;
    remove_open_directory_contents(&root, true)?;
    if node_identity(path)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cleanup tombstone identity changed before root removal",
        ));
    }
    drop(root);
    fs::remove_dir(path)
}

fn remove_open_directory_contents(directory: &OpenedDirectory, root: bool) -> io::Result<()> {
    let mut names = directory_names(directory)?;
    if root {
        names.sort_by_key(|name| {
            if name == OsStr::new(ARCHIVE_LEASE_ANCHOR_FILENAME) {
                2
            } else if name.to_string_lossy().starts_with(CLEANUP_INTENT_PREFIX) {
                1
            } else {
                0
            }
        });
    }
    for name in names {
        let path = child_path(directory, &name)?;
        let identity = node_identity(&path)?;
        match identity.kind {
            NodeKind::Directory => {
                let child = open_directory(&path)?;
                if child.identity != identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive directory entry changed before deletion",
                    ));
                }
                remove_open_directory_contents(&child, false)?;
                drop(child);
                if node_identity(&path)? != identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive directory identity changed before removal",
                    ));
                }
                fs::remove_dir(&path)?;
            }
            NodeKind::RegularFile => {
                if node_identity(&path)? != identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "archive file identity changed before removal",
                    ));
                }
                fs::remove_file(&path)?;
            }
            NodeKind::Symlink => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("archive symlink is rejected: {}", path.display()),
                ));
            }
            NodeKind::Other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("archive special entry is rejected: {}", path.display()),
                ));
            }
        }
        directory.file.sync_all()?;
    }
    Ok(())
}

fn sync_directory_identity(path: &Path, expected: NodeIdentity) -> io::Result<()> {
    let directory = open_directory(path)?;
    if directory.identity != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "directory identity changed before sync at {}",
                path.display()
            ),
        ));
    }
    directory.file.sync_all()
}

fn revalidate_directory_identity(path: &Path, expected: NodeIdentity) -> io::Result<()> {
    let directory = open_directory(path)?;
    if directory.identity != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("directory identity changed at {}", path.display()),
        ));
    }
    Ok(())
}

fn rename_entry_noreplace(
    directory: &OpenedDirectory,
    source: &OsStr,
    target: &OsStr,
) -> io::Result<()> {
    validate_single_name(source)?;
    validate_single_name(target)?;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let source = CString::new(source.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "cleanup source contains NUL")
        })?;
        let target = CString::new(target.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "cleanup target contains NUL")
        })?;
        // SAFETY: both names are validated NUL-terminated strings and the directory FD remains
        // live for the complete syscall.
        let result = unsafe {
            syscall(
                SYS_RENAMEAT2,
                directory.file.as_raw_fd(),
                source.as_ptr(),
                directory.file.as_raw_fd(),
                target.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = directory;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace cleanup rename is unsupported on this platform",
        ))
    }
}

#[derive(Clone, Copy, Debug)]
struct RenamedNamespace {
    source_parent: NodeIdentity,
    target_parent: NodeIdentity,
}

fn rename_noreplace(source: &Path, target: &Path) -> io::Result<RenamedNamespace> {
    let source_parent = source.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "cleanup source has no parent")
    })?;
    let target_parent = target.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "cleanup target has no parent")
    })?;
    let source_name = source.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup source has no filename",
        )
    })?;
    let target_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup target has no filename",
        )
    })?;
    validate_single_name(source_name)?;
    validate_single_name(target_name)?;
    let source_directory = open_directory(source_parent)?;
    let target_directory = open_directory(target_parent)?;
    if source_directory.identity.first != target_directory.identity.first {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup source and tombstone are not on the same filesystem",
        ));
    }
    match fs::symlink_metadata(target) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cleanup tombstone already exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let source_name = CString::new(source_name.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "cleanup source contains NUL")
        })?;
        let target_name = CString::new(target_name.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "cleanup target contains NUL")
        })?;
        // SAFETY: both names are validated NUL-terminated strings and both directory FDs live
        // for the complete syscall.
        let result = unsafe {
            syscall(
                SYS_RENAMEAT2,
                source_directory.file.as_raw_fd(),
                source_name.as_ptr(),
                target_directory.file.as_raw_fd(),
                target_name.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            Ok(RenamedNamespace {
                source_parent: source_directory.identity,
                target_parent: target_directory.identity,
            })
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (source_directory, target_directory);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace cleanup rename is unsupported on this platform",
        ))
    }
}

fn validate_single_name(name: &OsStr) -> io::Result<()> {
    if name.is_empty()
        || name == OsStr::new(".")
        || name == OsStr::new("..")
        || Path::new(name).components().count() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup namespace name is invalid",
        ));
    }
    Ok(())
}

fn canonical_production_root(production: &Path) -> Result<PathBuf, CleanupApplyError> {
    let canonical = fs::canonicalize(production).map_err(|error| {
        CleanupApplyError::new(
            CleanupApplyErrorCode::InvalidProductionRoot,
            format!("cannot resolve {}: {error}", production.display()),
        )
    })?;
    if !canonical.is_dir() {
        return Err(CleanupApplyError::new(
            CleanupApplyErrorCode::InvalidProductionRoot,
            format!(
                "production root is not a directory: {}",
                canonical.display()
            ),
        ));
    }
    Ok(canonical)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn ensure_cleanup_platform() -> Result<(), CleanupApplyError> {
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn ensure_cleanup_platform() -> Result<(), CleanupApplyError> {
    Err(CleanupApplyError::new(
        CleanupApplyErrorCode::UnsupportedPlatform,
        "safe cleanup apply requires Linux x86_64 renameat2 and no-follow directory handles",
    ))
}

fn diagnostics_directory(production: &Path) -> PathBuf {
    production.join(".troupe/diagnostics")
}

fn runs_directory(production: &Path) -> PathBuf {
    diagnostics_directory(production).join("runs")
}

fn run_directory(production: &Path, run_id: CanonicalUuid) -> PathBuf {
    runs_directory(production).join(run_id.to_string())
}

fn exact_run(policy: CleanupPolicy) -> Option<CanonicalUuid> {
    match policy {
        CleanupPolicy::Run(run_id) => Some(run_id.get()),
        CleanupPolicy::OlderThan(_)
        | CleanupPolicy::KeepRuns(_)
        | CleanupPolicy::MaxTotalBytes(_) => None,
    }
}

const fn protection_reason(reason: CleanupProtectionReason) -> CleanupApplyReason {
    match reason {
        CleanupProtectionReason::Active => CleanupApplyReason::ProtectedActive,
        CleanupProtectionReason::Leased => CleanupApplyReason::ProtectedLeased,
        CleanupProtectionReason::AmbiguousOwner => CleanupApplyReason::ProtectedAmbiguousOwner,
        CleanupProtectionReason::InvalidArchive => CleanupApplyReason::ProtectedInvalidArchive,
        CleanupProtectionReason::IncompatibleArchive => {
            CleanupApplyReason::ProtectedIncompatibleArchive
        }
        CleanupProtectionReason::MissingArchive => CleanupApplyReason::ProtectedMissingArchive,
        CleanupProtectionReason::MetadataInvalid => CleanupApplyReason::ProtectedMetadataInvalid,
        CleanupProtectionReason::UnsafeArchiveEntry => {
            CleanupApplyReason::ProtectedUnsafeArchiveEntry
        }
    }
}

const fn skip_reason(reason: CleanupSkipReason) -> CleanupApplyReason {
    match reason {
        CleanupSkipReason::IncompleteArchive => CleanupApplyReason::IncompleteArchive,
        CleanupSkipReason::NewerThanCutoff => CleanupApplyReason::NewerThanCutoff,
        CleanupSkipReason::WithinKeepCount => CleanupApplyReason::WithinKeepCount,
        CleanupSkipReason::WithinTotalByteBudget => CleanupApplyReason::WithinTotalByteBudget,
    }
}

fn write_optional_human<T: fmt::Display>(output: &mut String, key: &str, value: Option<T>) {
    match value {
        Some(value) => writeln!(output, "  {key}: {value}").expect("write to String"),
        None => writeln!(output, "  {key}: null").expect("write to String"),
    }
}

fn write_optional_top_human<T: fmt::Display>(output: &mut String, key: &str, value: Option<T>) {
    match value {
        Some(value) => writeln!(output, "{key}: {value}").expect("write to String"),
        None => writeln!(output, "{key}: null").expect("write to String"),
    }
}

fn push_optional_display_json<T: fmt::Display>(output: &mut String, value: Option<&T>) {
    match value {
        Some(value) => push_json_string(output, &value.to_string()),
        None => output.push_str("null"),
    }
}

fn push_optional_string_json(output: &mut String, value: Option<&str>) {
    match value {
        Some(value) => push_json_string(output, value),
        None => output.push_str("null"),
    }
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                write!(output, "\\u{:04x}", character as u32).expect("write to String");
            }
            character => output.push(character),
        }
    }
    output.push('"');
}
