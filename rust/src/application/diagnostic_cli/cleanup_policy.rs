#![allow(dead_code)]

use std::{
    cmp::Ordering,
    fmt::{self, Write as _},
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::lease::{
        ArchiveLeaseErrorCode, ArchiveLeaseMode, RealArchiveLeaseOpener, SharedArchiveLease,
        probe_archive_lease_with,
    },
    registry::discover::{
        CandidateClassification, DiscoveryCandidate, ProcessIdentityProbe,
        RealProcessIdentityProbe, ServerIdentityProbe, discover_with,
    },
};

use super::{
    archive_target::ArchiveTarget,
    args::{CleanupPolicy, DocumentFormat},
    http_client::{BlockingRegistryIdentityProbe, HttpClientError},
    resolver::{ResolvedDiagnosticTarget, resolve_discovered_production},
};

const CLEANUP_PREVIEW_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupPolicyErrorCode {
    InvalidProductionRoot,
    DiscoveryFailed,
    HttpFailed,
    TargetNotFound,
    ClockFailed,
    TaskFailed,
}

impl CleanupPolicyErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidProductionRoot => "diagnostic_cleanup.invalid_production_root",
            Self::DiscoveryFailed => "diagnostic_cleanup.discovery_failed",
            Self::HttpFailed => "diagnostic_cleanup.http_failed",
            Self::TargetNotFound => "diagnostic_cleanup.target_not_found",
            Self::ClockFailed => "diagnostic_cleanup.clock_failed",
            Self::TaskFailed => "diagnostic_cleanup.task_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CleanupPolicyError {
    code: CleanupPolicyErrorCode,
    detail: String,
}

impl CleanupPolicyError {
    fn new(code: CleanupPolicyErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn http(error: HttpClientError) -> Self {
        Self::new(CleanupPolicyErrorCode::HttpFailed, error.to_string())
    }

    pub(crate) const fn code(&self) -> CleanupPolicyErrorCode {
        self.code
    }
}

impl fmt::Display for CleanupPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for CleanupPolicyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupSelectionReason {
    ExactRun,
    EndedBeforeCutoff,
    OutsideKeepCount,
    TotalBytesOverBudget,
}

impl CleanupSelectionReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ExactRun => "exact_run",
            Self::EndedBeforeCutoff => "ended_before_cutoff",
            Self::OutsideKeepCount => "outside_keep_count",
            Self::TotalBytesOverBudget => "total_bytes_over_budget",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupProtectionReason {
    Active,
    Leased,
    AmbiguousOwner,
    InvalidArchive,
    IncompatibleArchive,
    MissingArchive,
    MetadataInvalid,
    UnsafeArchiveEntry,
}

impl CleanupProtectionReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Leased => "leased",
            Self::AmbiguousOwner => "ambiguous_owner",
            Self::InvalidArchive => "invalid_archive",
            Self::IncompatibleArchive => "incompatible_archive",
            Self::MissingArchive => "missing_archive",
            Self::MetadataInvalid => "metadata_invalid",
            Self::UnsafeArchiveEntry => "unsafe_archive_entry",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupSkipReason {
    IncompleteArchive,
    NewerThanCutoff,
    WithinKeepCount,
    WithinTotalByteBudget,
}

impl CleanupSkipReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::IncompleteArchive => "incomplete_archive",
            Self::NewerThanCutoff => "newer_than_cutoff",
            Self::WithinKeepCount => "within_keep_count",
            Self::WithinTotalByteBudget => "within_total_byte_budget",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupOperationFailure {
    ProtectedBytesUnknown,
    ProtectedBytesExceedBudget,
}

impl CleanupOperationFailure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ProtectedBytesUnknown => "protected_bytes_unknown",
            Self::ProtectedBytesExceedBudget => "protected_bytes_exceed_budget",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupArchiveState {
    Completed,
    Incomplete,
    Unknown,
}

impl CleanupArchiveState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UtcTimestamp {
    instant: UtcInstant,
    encoded: String,
}

impl UtcTimestamp {
    fn parse(encoded: &str) -> Result<Self, ()> {
        parse_utc_timestamp(encoded).map(|instant| Self {
            instant,
            encoded: encoded.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct UtcInstant {
    seconds: i128,
    nanoseconds: u32,
}

impl UtcInstant {
    fn from_system_time(value: SystemTime) -> Self {
        match value.duration_since(UNIX_EPOCH) {
            Ok(duration) => Self {
                seconds: i128::from(duration.as_secs()),
                nanoseconds: duration.subsec_nanos(),
            },
            Err(error) => {
                let duration = error.duration();
                if duration.subsec_nanos() == 0 {
                    Self {
                        seconds: -i128::from(duration.as_secs()),
                        nanoseconds: 0,
                    }
                } else {
                    Self {
                        seconds: -i128::from(duration.as_secs()) - 1,
                        nanoseconds: 1_000_000_000 - duration.subsec_nanos(),
                    }
                }
            }
        }
    }

    fn subtract(self, duration: Duration) -> Self {
        Self {
            seconds: self.seconds - i128::from(duration.as_secs()),
            nanoseconds: self.nanoseconds,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CleanupInventoryRun {
    run_id: Option<CanonicalUuid>,
    path: PathBuf,
    state: CleanupArchiveState,
    bytes: Option<u64>,
    started_at: Option<UtcTimestamp>,
    ended_at: Option<UtcTimestamp>,
    clean_shutdown: Option<bool>,
    protection_reason: Option<CleanupProtectionReason>,
    detail: Option<String>,
    selected: bool,
    selection_reason: Option<CleanupSelectionReason>,
    skipped_reason: Option<CleanupSkipReason>,
}

impl CleanupInventoryRun {
    pub(crate) fn completed(
        run_id: CanonicalUuid,
        path: impl Into<PathBuf>,
        bytes: u64,
        started_at: &str,
        ended_at: &str,
    ) -> Result<Self, CleanupPolicyError> {
        Ok(Self {
            run_id: Some(run_id),
            path: path.into(),
            state: CleanupArchiveState::Completed,
            bytes: Some(bytes),
            started_at: Some(parse_inventory_timestamp(started_at)?),
            ended_at: Some(parse_inventory_timestamp(ended_at)?),
            clean_shutdown: Some(true),
            protection_reason: None,
            detail: None,
            selected: false,
            selection_reason: None,
            skipped_reason: None,
        })
    }

    pub(crate) fn incomplete(
        run_id: CanonicalUuid,
        path: impl Into<PathBuf>,
        bytes: u64,
        started_at: &str,
    ) -> Result<Self, CleanupPolicyError> {
        Ok(Self {
            run_id: Some(run_id),
            path: path.into(),
            state: CleanupArchiveState::Incomplete,
            bytes: Some(bytes),
            started_at: Some(parse_inventory_timestamp(started_at)?),
            ended_at: None,
            clean_shutdown: Some(false),
            protection_reason: None,
            detail: None,
            selected: false,
            selection_reason: None,
            skipped_reason: None,
        })
    }

    pub(crate) fn protected(
        run_id: Option<CanonicalUuid>,
        path: impl Into<PathBuf>,
        bytes: Option<u64>,
        reason: CleanupProtectionReason,
    ) -> Self {
        Self {
            run_id,
            path: path.into(),
            state: CleanupArchiveState::Unknown,
            bytes,
            started_at: None,
            ended_at: None,
            clean_shutdown: None,
            protection_reason: Some(reason),
            detail: None,
            selected: false,
            selection_reason: None,
            skipped_reason: None,
        }
    }

    pub(crate) fn with_protection(mut self, reason: CleanupProtectionReason) -> Self {
        self.protection_reason = Some(reason);
        self
    }

    pub(crate) const fn run_id(&self) -> Option<CanonicalUuid> {
        self.run_id
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) const fn bytes(&self) -> Option<u64> {
        self.bytes
    }

    pub(crate) const fn selected(&self) -> bool {
        self.selected
    }

    pub(crate) const fn selection_reason(&self) -> Option<CleanupSelectionReason> {
        self.selection_reason
    }

    pub(crate) const fn protection_reason(&self) -> Option<CleanupProtectionReason> {
        self.protection_reason
    }

    pub(crate) const fn skipped_reason(&self) -> Option<CleanupSkipReason> {
        self.skipped_reason
    }

    fn is_eligible_completed(&self) -> bool {
        self.protection_reason.is_none() && self.state == CleanupArchiveState::Completed
    }

    fn render_human(&self, output: &mut String, index: usize) {
        writeln!(output).expect("write to String");
        writeln!(output, "run {}:", index + 1).expect("write to String");
        write_optional_human(output, "run_id", self.run_id.as_ref());
        writeln!(output, "  path: {}", self.path.display()).expect("write to String");
        writeln!(output, "  archive_state: {}", self.state.as_str()).expect("write to String");
        write_optional_human(output, "bytes", self.bytes.as_ref());
        write_optional_human(
            output,
            "started_at",
            self.started_at.as_ref().map(|value| value.encoded.as_str()),
        );
        write_optional_human(
            output,
            "ended_at",
            self.ended_at.as_ref().map(|value| value.encoded.as_str()),
        );
        write_optional_human(output, "clean_shutdown", self.clean_shutdown.as_ref());
        writeln!(output, "  selected: {}", self.selected).expect("write to String");
        write_optional_human(
            output,
            "selection_reason",
            self.selection_reason.map(CleanupSelectionReason::as_str),
        );
        write_optional_human(
            output,
            "protected_reason",
            self.protection_reason.map(CleanupProtectionReason::as_str),
        );
        write_optional_human(
            output,
            "skipped_reason",
            self.skipped_reason.map(CleanupSkipReason::as_str),
        );
        write_optional_human(output, "detail", self.detail.as_deref());
    }

    fn render_json(&self, output: &mut String) {
        output.push_str("{\"run_id\":");
        push_optional_display_json(output, self.run_id.as_ref());
        output.push_str(",\"path\":");
        push_json_path(output, &self.path);
        output.push_str(",\"archive_state\":");
        push_json_string(output, self.state.as_str());
        output.push_str(",\"bytes\":");
        push_optional_display_json(output, self.bytes.as_ref());
        output.push_str(",\"started_at\":");
        push_optional_string_json(
            output,
            self.started_at.as_ref().map(|value| value.encoded.as_str()),
        );
        output.push_str(",\"ended_at\":");
        push_optional_string_json(
            output,
            self.ended_at.as_ref().map(|value| value.encoded.as_str()),
        );
        output.push_str(",\"clean_shutdown\":");
        push_optional_bool_json(output, self.clean_shutdown);
        write!(
            output,
            ",\"selected\":{},\"selection_reason\":",
            self.selected
        )
        .expect("write to String");
        push_optional_string_json(
            output,
            self.selection_reason.map(CleanupSelectionReason::as_str),
        );
        output.push_str(",\"protected_reason\":");
        push_optional_string_json(
            output,
            self.protection_reason.map(CleanupProtectionReason::as_str),
        );
        output.push_str(",\"skipped_reason\":");
        push_optional_string_json(output, self.skipped_reason.map(CleanupSkipReason::as_str));
        output.push_str(",\"detail\":");
        push_optional_string_json(output, self.detail.as_deref());
        output.push('}');
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupPolicySpec {
    Run(CanonicalUuid),
    OlderThan(Duration),
    KeepRuns(u64),
    MaxTotalBytes(u64),
}

impl CleanupPolicySpec {
    const fn kind(self) -> &'static str {
        match self {
            Self::Run(_) => "run",
            Self::OlderThan(_) => "older_than",
            Self::KeepRuns(_) => "keep_runs",
            Self::MaxTotalBytes(_) => "max_total_bytes",
        }
    }

    fn value(self) -> String {
        match self {
            Self::Run(run_id) => run_id.to_string(),
            Self::OlderThan(duration) => duration.as_secs().to_string(),
            Self::KeepRuns(count) | Self::MaxTotalBytes(count) => count.to_string(),
        }
    }
}

impl From<CleanupPolicy> for CleanupPolicySpec {
    fn from(policy: CleanupPolicy) -> Self {
        match policy {
            CleanupPolicy::Run(run_id) => Self::Run(run_id.get()),
            CleanupPolicy::OlderThan(age) => Self::OlderThan(age.get()),
            CleanupPolicy::KeepRuns(count) => Self::KeepRuns(count.get()),
            CleanupPolicy::MaxTotalBytes(size) => Self::MaxTotalBytes(size.bytes()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CleanupPreview {
    production: PathBuf,
    policy: CleanupPolicySpec,
    satisfied: bool,
    operation_failure: Option<CleanupOperationFailure>,
    total_bytes: Option<u128>,
    selected_bytes: u128,
    remaining_bytes: Option<u128>,
    runs: Vec<CleanupInventoryRun>,
}

impl CleanupPreview {
    pub(crate) fn production(&self) -> &Path {
        &self.production
    }

    pub(crate) const fn satisfied(&self) -> bool {
        self.satisfied
    }

    pub(crate) const fn operation_failure(&self) -> Option<CleanupOperationFailure> {
        self.operation_failure
    }

    pub(crate) const fn total_bytes(&self) -> Option<u128> {
        self.total_bytes
    }

    pub(crate) const fn selected_bytes(&self) -> u128 {
        self.selected_bytes
    }

    pub(crate) const fn remaining_bytes(&self) -> Option<u128> {
        self.remaining_bytes
    }

    pub(crate) fn runs(&self) -> &[CleanupInventoryRun] {
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
        writeln!(output, "policy: {}", self.policy.kind()).expect("write to String");
        writeln!(output, "policy_value: {}", self.policy.value()).expect("write to String");
        writeln!(output, "policy_satisfied: {}", self.satisfied).expect("write to String");
        write_optional_top_human(
            &mut output,
            "operation_error",
            self.operation_failure.map(CleanupOperationFailure::as_str),
        );
        writeln!(output, "run_count: {}", self.runs.len()).expect("write to String");
        write_optional_top_human(&mut output, "total_bytes", self.total_bytes.as_ref());
        writeln!(output, "selected_bytes: {}", self.selected_bytes).expect("write to String");
        write_optional_top_human(
            &mut output,
            "remaining_bytes",
            self.remaining_bytes.as_ref(),
        );
        for (index, run) in self.runs.iter().enumerate() {
            run.render_human(&mut output, index);
        }
        output
    }

    fn render_json(&self) -> String {
        let mut output = String::new();
        write!(
            output,
            "{{\"cleanup_preview_schema_version\":{CLEANUP_PREVIEW_SCHEMA_VERSION},\"production\":"
        )
        .expect("write to String");
        push_json_path(&mut output, &self.production);
        output.push_str(",\"policy\":{\"kind\":");
        push_json_string(&mut output, self.policy.kind());
        output.push_str(",\"value\":");
        push_json_string(&mut output, &self.policy.value());
        write!(
            output,
            "}},\"policy_satisfied\":{},\"operation_error\":",
            self.satisfied
        )
        .expect("write to String");
        push_optional_string_json(
            &mut output,
            self.operation_failure.map(CleanupOperationFailure::as_str),
        );
        write!(
            output,
            ",\"run_count\":\"{}\",\"total_bytes\":",
            self.runs.len()
        )
        .expect("write to String");
        push_optional_display_json(&mut output, self.total_bytes.as_ref());
        write!(
            output,
            ",\"selected_bytes\":\"{}\",\"remaining_bytes\":",
            self.selected_bytes
        )
        .expect("write to String");
        push_optional_display_json(&mut output, self.remaining_bytes.as_ref());
        output.push_str(",\"runs\":[");
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
pub(crate) enum CleanupLeaseAvailability {
    Available,
    Leased,
    Invalid,
}

pub(crate) trait CleanupLeaseProbe: Send + Sync {
    fn probe(&self, run_directory: &Path) -> CleanupLeaseAvailability;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RealCleanupLeaseProbe;

impl CleanupLeaseProbe for RealCleanupLeaseProbe {
    fn probe(&self, run_directory: &Path) -> CleanupLeaseAvailability {
        // This is a non-blocking availability check only. The runtime helper drops the lock before
        // returning; only the apply path holds the lease across identity revalidation and
        // whole-directory deletion.
        match probe_archive_lease_with(
            &RealArchiveLeaseOpener,
            run_directory,
            ArchiveLeaseMode::Cleanup,
        ) {
            Ok(()) => CleanupLeaseAvailability::Available,
            Err(error) if error.code() == ArchiveLeaseErrorCode::Contended => {
                CleanupLeaseAvailability::Leased
            }
            Err(_) => CleanupLeaseAvailability::Invalid,
        }
    }
}

pub(crate) async fn preview(
    production: PathBuf,
    policy: CleanupPolicy,
) -> Result<CleanupPreview, CleanupPolicyError> {
    tokio::task::spawn_blocking(move || {
        let production = canonical_production_root(&production)?;
        let server_probe =
            BlockingRegistryIdentityProbe::new().map_err(CleanupPolicyError::http)?;
        preview_canonical_with(
            &production,
            policy,
            SystemTime::now(),
            &RealProcessIdentityProbe,
            &server_probe,
            &RealCleanupLeaseProbe,
        )
    })
    .await
    .map_err(|error| {
        CleanupPolicyError::new(
            CleanupPolicyErrorCode::TaskFailed,
            format!("cleanup preview task failed: {error}"),
        )
    })?
}

pub(crate) fn preview_with(
    production: &Path,
    policy: CleanupPolicy,
    now: SystemTime,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
    lease_probe: &dyn CleanupLeaseProbe,
) -> Result<CleanupPreview, CleanupPolicyError> {
    let production = canonical_production_root(production)?;
    preview_canonical_with(
        &production,
        policy,
        now,
        process_probe,
        server_probe,
        lease_probe,
    )
}

fn canonical_production_root(production: &Path) -> Result<PathBuf, CleanupPolicyError> {
    let canonical = fs::canonicalize(production).map_err(|error| {
        CleanupPolicyError::new(
            CleanupPolicyErrorCode::InvalidProductionRoot,
            format!("cannot resolve {}: {error}", production.display()),
        )
    })?;
    if !canonical.is_dir() {
        return Err(CleanupPolicyError::new(
            CleanupPolicyErrorCode::InvalidProductionRoot,
            format!(
                "production root is not a directory: {}",
                canonical.display()
            ),
        ));
    }
    Ok(canonical)
}

fn preview_canonical_with(
    production: &Path,
    policy: CleanupPolicy,
    now: SystemTime,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
    lease_probe: &dyn CleanupLeaseProbe,
) -> Result<CleanupPreview, CleanupPolicyError> {
    let candidates = discover_with(production, process_probe, server_probe).map_err(|error| {
        CleanupPolicyError::new(CleanupPolicyErrorCode::DiscoveryFailed, error.to_string())
    })?;
    let policy_spec = CleanupPolicySpec::from(policy);
    let requested_run = match policy_spec {
        CleanupPolicySpec::Run(run_id) => Some(run_id),
        _ => None,
    };
    let mut runs = candidates
        .iter()
        .filter(|candidate| candidate.archive_present())
        .filter(|candidate| {
            requested_run.is_none_or(|run_id| candidate_identity(candidate) == Some(run_id))
        })
        .map(|candidate| {
            inspect_candidate(
                candidate,
                &candidates,
                process_probe,
                server_probe,
                lease_probe,
            )
        })
        .collect::<Vec<_>>();

    if let Some(run_id) = requested_run
        && runs.is_empty()
    {
        let matching_without_archive = candidates
            .iter()
            .find(|candidate| candidate_identity(candidate) == Some(run_id));
        if let Some(candidate) = matching_without_archive {
            runs.push(protected_candidate(
                candidate,
                CleanupProtectionReason::MissingArchive,
                candidate.detail().map(ToOwned::to_owned),
            ));
        } else {
            return Err(CleanupPolicyError::new(
                CleanupPolicyErrorCode::TargetNotFound,
                format!("Run {run_id} is not present under the production diagnostics root"),
            ));
        }
    }

    select_preview(
        production.to_path_buf(),
        policy_spec,
        UtcInstant::from_system_time(now),
        runs,
    )
}

fn inspect_candidate(
    candidate: &DiscoveryCandidate,
    candidates: &[DiscoveryCandidate],
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
    lease_probe: &dyn CleanupLeaseProbe,
) -> CleanupInventoryRun {
    let run_id = candidate_identity(candidate);
    let path = candidate
        .archive_directory()
        .unwrap_or_else(|| candidate.path())
        .to_path_buf();
    let initial_protection = protection_for_classification(candidate.classification());
    let read_lease = match SharedArchiveLease::acquire(&path) {
        Ok(lease) => lease,
        Err(error) => {
            let reason = initial_protection.unwrap_or_else(|| {
                if error.code() == ArchiveLeaseErrorCode::Contended {
                    CleanupProtectionReason::Leased
                } else {
                    CleanupProtectionReason::InvalidArchive
                }
            });
            let detail = match candidate.detail() {
                Some(candidate_detail) => format!(
                    "{candidate_detail}; archive cannot be leased for stable preview scanning: {error}"
                ),
                None => format!("archive cannot be leased for stable preview scanning: {error}"),
            };
            let mut run = protected_candidate(candidate, reason, Some(detail));
            run.path = path;
            return run;
        }
    };
    let bytes = match apparent_regular_file_bytes(&path) {
        Ok(bytes) => Some(bytes),
        Err(detail) => {
            let mut run = protected_candidate(
                candidate,
                CleanupProtectionReason::UnsafeArchiveEntry,
                Some(detail),
            );
            run.path = path;
            return run;
        }
    };

    if let Some(reason) = initial_protection {
        let mut run =
            protected_candidate(candidate, reason, candidate.detail().map(ToOwned::to_owned));
        run.path = path;
        run.bytes = bytes;
        return run;
    }

    let Some(run_id) = run_id else {
        let mut run = protected_candidate(
            candidate,
            CleanupProtectionReason::InvalidArchive,
            Some("candidate has no trusted or path-derived Run identity".to_owned()),
        );
        run.path = path;
        run.bytes = bytes;
        return run;
    };

    let target = match candidate.classification() {
        CandidateClassification::DefiniteStale => {
            match resolve_discovered_production(
                candidates,
                Some(run_id),
                process_probe,
                server_probe,
            ) {
                Ok(ResolvedDiagnosticTarget::Archive(target)) => Ok(target),
                Ok(ResolvedDiagnosticTarget::Live(_)) => Err((
                    CleanupProtectionReason::Active,
                    "Run became active during cleanup preview revalidation".to_owned(),
                )),
                Err(error) => Err((CleanupProtectionReason::AmbiguousOwner, error.to_string())),
            }
        }
        CandidateClassification::Completed | CandidateClassification::Incomplete => {
            ArchiveTarget::open_expected(&path, run_id)
                .map_err(|error| (CleanupProtectionReason::MetadataInvalid, error.to_string()))
        }
        _ => unreachable!("protected classifications returned above"),
    };
    let mut target = match target {
        Ok(target) => target,
        Err((reason, detail)) => {
            let mut run = CleanupInventoryRun::protected(Some(run_id), path, bytes, reason);
            run.detail = Some(detail);
            return run;
        }
    };
    let captured = match target.capture() {
        Ok(captured) => captured,
        Err(error) => {
            let mut run = CleanupInventoryRun::protected(
                Some(run_id),
                path,
                bytes,
                CleanupProtectionReason::MetadataInvalid,
            );
            run.detail = Some(error.to_string());
            return run;
        }
    };
    let metadata = captured.metadata();
    let started_at = UtcTimestamp::parse(metadata.started_at());
    let ended_at = metadata.ended_at().map(UtcTimestamp::parse).transpose();
    let clean_shutdown = metadata.clean_shutdown();
    drop(captured);
    drop(target);
    drop(read_lease);

    let (started_at, ended_at) = match (started_at, ended_at) {
        (Ok(started_at), Ok(ended_at)) => (started_at, ended_at),
        _ => {
            let mut run = CleanupInventoryRun::protected(
                Some(run_id),
                path,
                bytes,
                CleanupProtectionReason::MetadataInvalid,
            );
            run.detail = Some("Run timestamps are not canonical UTC RFC3339 values".to_owned());
            return run;
        }
    };

    let lease = lease_probe.probe(&path);
    let protection_reason = match lease {
        CleanupLeaseAvailability::Available => None,
        CleanupLeaseAvailability::Leased => Some(CleanupProtectionReason::Leased),
        CleanupLeaseAvailability::Invalid => Some(CleanupProtectionReason::InvalidArchive),
    };
    let state = if clean_shutdown && ended_at.is_some() {
        CleanupArchiveState::Completed
    } else {
        CleanupArchiveState::Incomplete
    };
    CleanupInventoryRun {
        run_id: Some(run_id),
        path,
        state,
        bytes,
        started_at: Some(started_at),
        ended_at,
        clean_shutdown: Some(clean_shutdown),
        protection_reason,
        detail: (lease == CleanupLeaseAvailability::Invalid)
            .then(|| "archive lease anchor could not be validated".to_owned()),
        selected: false,
        selection_reason: None,
        skipped_reason: None,
    }
}

pub(crate) const fn protection_for_classification(
    classification: CandidateClassification,
) -> Option<CleanupProtectionReason> {
    match classification {
        CandidateClassification::Active => Some(CleanupProtectionReason::Active),
        CandidateClassification::Unhealthy | CandidateClassification::IdentityMismatch => {
            Some(CleanupProtectionReason::AmbiguousOwner)
        }
        CandidateClassification::Invalid => Some(CleanupProtectionReason::InvalidArchive),
        CandidateClassification::Incompatible => Some(CleanupProtectionReason::IncompatibleArchive),
        CandidateClassification::DefiniteStale
        | CandidateClassification::Completed
        | CandidateClassification::Incomplete => None,
    }
}

fn protected_candidate(
    candidate: &DiscoveryCandidate,
    reason: CleanupProtectionReason,
    detail: Option<String>,
) -> CleanupInventoryRun {
    let mut run = CleanupInventoryRun::protected(
        candidate_identity(candidate),
        candidate
            .archive_directory()
            .unwrap_or_else(|| candidate.path()),
        None,
        reason,
    );
    run.detail = detail;
    run
}

fn candidate_identity(candidate: &DiscoveryCandidate) -> Option<CanonicalUuid> {
    candidate.run_id().or_else(|| candidate.path_run_id())
}

pub(crate) fn preview_from_inventory(
    production: PathBuf,
    policy: CleanupPolicy,
    now: SystemTime,
    runs: Vec<CleanupInventoryRun>,
) -> Result<CleanupPreview, CleanupPolicyError> {
    select_preview(
        production,
        CleanupPolicySpec::from(policy),
        UtcInstant::from_system_time(now),
        runs,
    )
}

fn select_preview(
    production: PathBuf,
    policy: CleanupPolicySpec,
    now: UtcInstant,
    mut runs: Vec<CleanupInventoryRun>,
) -> Result<CleanupPreview, CleanupPolicyError> {
    runs.sort_by(cleanup_order);
    match policy {
        CleanupPolicySpec::Run(run_id) => {
            runs.retain(|run| run.run_id == Some(run_id));
            if runs.is_empty() {
                return Err(CleanupPolicyError::new(
                    CleanupPolicyErrorCode::TargetNotFound,
                    format!("Run {run_id} is not present in the cleanup inventory"),
                ));
            }
            for run in &mut runs {
                if run.protection_reason.is_none() {
                    run.selected = true;
                    run.selection_reason = Some(CleanupSelectionReason::ExactRun);
                }
            }
        }
        CleanupPolicySpec::OlderThan(age) => {
            let cutoff = now.subtract(age);
            for run in &mut runs {
                if run.protection_reason.is_some() {
                    continue;
                }
                if run.state == CleanupArchiveState::Incomplete {
                    run.skipped_reason = Some(CleanupSkipReason::IncompleteArchive);
                } else if run
                    .ended_at
                    .as_ref()
                    .is_some_and(|ended_at| ended_at.instant < cutoff)
                {
                    run.selected = true;
                    run.selection_reason = Some(CleanupSelectionReason::EndedBeforeCutoff);
                } else {
                    run.skipped_reason = Some(CleanupSkipReason::NewerThanCutoff);
                }
            }
        }
        CleanupPolicySpec::KeepRuns(keep) => {
            mark_incomplete_batch_runs(&mut runs);
            let completed = runs
                .iter()
                .enumerate()
                .filter_map(|(index, run)| {
                    (run.state == CleanupArchiveState::Completed).then_some(index)
                })
                .collect::<Vec<_>>();
            let keep = usize::try_from(keep).unwrap_or(usize::MAX);
            let select_count = completed.len().saturating_sub(keep);
            for (position, index) in completed.into_iter().enumerate() {
                let run = &mut runs[index];
                if run.protection_reason.is_some() {
                    continue;
                } else if position < select_count {
                    run.selected = true;
                    run.selection_reason = Some(CleanupSelectionReason::OutsideKeepCount);
                } else {
                    run.skipped_reason = Some(CleanupSkipReason::WithinKeepCount);
                }
            }
        }
        CleanupPolicySpec::MaxTotalBytes(budget) => {
            mark_incomplete_batch_runs(&mut runs);
            if let Some(total) = sum_all_bytes(&runs) {
                let mut remaining = total;
                for run in runs.iter_mut().filter(|run| run.is_eligible_completed()) {
                    if remaining <= u128::from(budget) {
                        run.skipped_reason = Some(CleanupSkipReason::WithinTotalByteBudget);
                        continue;
                    }
                    let bytes = u128::from(run.bytes.expect("eligible archives have known bytes"));
                    run.selected = true;
                    run.selection_reason = Some(CleanupSelectionReason::TotalBytesOverBudget);
                    remaining -= bytes;
                }
            }
        }
    }

    let total_bytes = sum_all_bytes(&runs);
    let selected_bytes = runs
        .iter()
        .filter(|run| run.selected)
        .map(|run| u128::from(run.bytes.expect("selected archives have known bytes")))
        .sum::<u128>();
    let remaining_bytes = total_bytes.map(|total| total - selected_bytes);
    let operation_failure = match policy {
        CleanupPolicySpec::MaxTotalBytes(budget) => match remaining_bytes {
            None => Some(CleanupOperationFailure::ProtectedBytesUnknown),
            Some(remaining) if remaining > u128::from(budget) => {
                Some(CleanupOperationFailure::ProtectedBytesExceedBudget)
            }
            Some(_) => None,
        },
        _ => None,
    };
    Ok(CleanupPreview {
        production,
        policy,
        satisfied: operation_failure.is_none(),
        operation_failure,
        total_bytes,
        selected_bytes,
        remaining_bytes,
        runs,
    })
}

fn mark_incomplete_batch_runs(runs: &mut [CleanupInventoryRun]) {
    for run in runs {
        if run.protection_reason.is_none() && run.state == CleanupArchiveState::Incomplete {
            run.skipped_reason = Some(CleanupSkipReason::IncompleteArchive);
        }
    }
}

fn cleanup_order(left: &CleanupInventoryRun, right: &CleanupInventoryRun) -> Ordering {
    compare_optional_timestamp(&left.ended_at, &right.ended_at)
        .then_with(|| compare_optional_timestamp(&left.started_at, &right.started_at))
        .then_with(|| compare_optional_run_id(left.run_id, right.run_id))
        .then_with(|| left.path.cmp(&right.path))
}

fn compare_optional_timestamp(
    left: &Option<UtcTimestamp>,
    right: &Option<UtcTimestamp>,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.instant.cmp(&right.instant),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_optional_run_id(left: Option<CanonicalUuid>, right: Option<CanonicalUuid>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.to_string().cmp(&right.to_string()),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn sum_all_bytes(runs: &[CleanupInventoryRun]) -> Option<u128> {
    runs.iter().try_fold(0_u128, |total, run| {
        run.bytes.map(|bytes| total + u128::from(bytes))
    })
}

fn parse_inventory_timestamp(value: &str) -> Result<UtcTimestamp, CleanupPolicyError> {
    UtcTimestamp::parse(value).map_err(|()| {
        CleanupPolicyError::new(
            CleanupPolicyErrorCode::ClockFailed,
            format!("timestamp is not canonical UTC RFC3339: {value}"),
        )
    })
}

fn parse_utc_timestamp(value: &str) -> Result<UtcInstant, ()> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes.last() != Some(&b'Z')
    {
        return Err(());
    }
    let year = parse_decimal(&bytes[0..4])? as i64;
    let month = parse_decimal(&bytes[5..7])?;
    let day = parse_decimal(&bytes[8..10])?;
    let hour = parse_decimal(&bytes[11..13])?;
    let minute = parse_decimal(&bytes[14..16])?;
    let second = parse_decimal(&bytes[17..19])?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(());
    }
    let nanoseconds = match &bytes[19..bytes.len() - 1] {
        [] => 0,
        fraction if fraction[0] == b'.' && (2..=10).contains(&fraction.len()) => {
            let digits = &fraction[1..];
            let value = parse_decimal(digits)?;
            value * 10_u32.pow(u32::try_from(9 - digits.len()).map_err(|_| ())?)
        }
        _ => return Err(()),
    };
    let days = days_from_civil(year, month, day);
    Ok(UtcInstant {
        seconds: i128::from(days) * 86_400
            + i128::from(hour) * 3_600
            + i128::from(minute) * 60
            + i128::from(second),
        nanoseconds,
    })
}

fn parse_decimal(bytes: &[u8]) -> Result<u32, ()> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(());
    }
    bytes.iter().try_fold(0_u32, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or(())
    })
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn apparent_regular_file_bytes(run_directory: &Path) -> Result<u64, String> {
    let initial = fs::symlink_metadata(run_directory).map_err(|error| {
        format!(
            "cannot inspect Run directory {}: {error}",
            run_directory.display()
        )
    })?;
    if initial.file_type().is_symlink() || !initial.is_dir() {
        return Err(format!(
            "Run archive is not a non-symlink directory: {}",
            run_directory.display()
        ));
    }
    let root_identity = NodeIdentity::from_metadata(&initial);
    let mut total = 0_u64;
    let mut pending = vec![run_directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let before = fs::symlink_metadata(&directory)
            .map_err(|error| format!("cannot inspect {}: {error}", directory.display()))?;
        if before.file_type().is_symlink() || !before.is_dir() {
            return Err(format!(
                "archive directory changed or is unsafe: {}",
                directory.display()
            ));
        }
        let identity = NodeIdentity::from_metadata(&before);
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!("cannot enumerate archive {}: {error}", directory.display())
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(format!(
                    "archive symlink is not allowed: {}",
                    path.display()
                ));
            }
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                total = total.checked_add(metadata.len()).ok_or_else(|| {
                    format!(
                        "archive apparent byte total overflowed at {}",
                        path.display()
                    )
                })?;
            } else {
                return Err(format!(
                    "archive entry is not a regular file or directory: {}",
                    path.display()
                ));
            }
        }
        let after = fs::symlink_metadata(&directory)
            .map_err(|error| format!("cannot revalidate {}: {error}", directory.display()))?;
        if after.file_type().is_symlink()
            || !after.is_dir()
            || NodeIdentity::from_metadata(&after) != identity
        {
            return Err(format!(
                "archive directory identity changed while scanning: {}",
                directory.display()
            ));
        }
    }
    let final_root = fs::symlink_metadata(run_directory).map_err(|error| {
        format!(
            "cannot revalidate Run directory {}: {error}",
            run_directory.display()
        )
    })?;
    if final_root.file_type().is_symlink()
        || !final_root.is_dir()
        || NodeIdentity::from_metadata(&final_root) != root_identity
    {
        return Err("Run directory identity changed while computing apparent bytes".to_owned());
    }
    Ok(total)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NodeIdentity {
    first: u64,
    second: u64,
}

impl NodeIdentity {
    #[cfg(unix)]
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            first: metadata.dev(),
            second: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            first: metadata.len(),
            second: u64::from(metadata.permissions().readonly()),
        }
    }
}

fn write_optional_human<T: fmt::Display + ?Sized>(
    output: &mut String,
    field: &str,
    value: Option<&T>,
) {
    match value {
        Some(value) => writeln!(output, "  {field}: {value}"),
        None => writeln!(output, "  {field}: null"),
    }
    .expect("write to String");
}

fn write_optional_top_human<T: fmt::Display + ?Sized>(
    output: &mut String,
    field: &str,
    value: Option<&T>,
) {
    match value {
        Some(value) => writeln!(output, "{field}: {value}"),
        None => writeln!(output, "{field}: null"),
    }
    .expect("write to String");
}

fn push_optional_display_json<T: fmt::Display + ?Sized>(output: &mut String, value: Option<&T>) {
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

fn push_optional_bool_json(output: &mut String, value: Option<bool>) {
    match value {
        Some(value) => output.push_str(if value { "true" } else { "false" }),
        None => output.push_str("null"),
    }
}

fn push_json_path(output: &mut String, value: &Path) {
    push_json_string(output, &value.to_string_lossy());
}

fn push_json_string(output: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

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
                let byte = character as u8;
                output.push_str("\\u00");
                output.push(HEX[(byte >> 4) as usize] as char);
                output.push(HEX[(byte & 0x0f) as usize] as char);
            }
            character => output.push(character),
        }
    }
    output.push('"');
}
