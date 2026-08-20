#![allow(dead_code)]

use std::{fmt, fs, path::Path};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::registry::{
    discover::{
        CandidateClassification, CandidateSource, DiscoveryCandidate, ProcessIdentityProbe,
        RealProcessIdentityProbe, ServerIdentityProbe, discover_with,
    },
    revalidate::{revalidate_for_cleanup, revalidate_for_use},
};

use super::{
    archive_target::ArchiveTarget,
    http_client::{
        BlockingRegistryIdentityProbe, DiagnosticHttpClient, HttpClientError, HttpClientErrorCode,
    },
    target::DiagnosticTarget,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolverErrorCode {
    InvalidProductionRoot,
    DiscoveryFailed,
    TargetNotFound,
    AmbiguousLiveTarget,
    AmbiguousArchiveTarget,
    UnsafeCandidate,
    RevalidationFailed,
    ArchiveFailed,
    HttpFailed,
    TaskFailed,
}

impl ResolverErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidProductionRoot => "diagnostic_resolver.invalid_production_root",
            Self::DiscoveryFailed => "diagnostic_resolver.discovery_failed",
            Self::TargetNotFound => "diagnostic_resolver.target_not_found",
            Self::AmbiguousLiveTarget => "diagnostic_resolver.ambiguous_live_target",
            Self::AmbiguousArchiveTarget => "diagnostic_resolver.ambiguous_archive_target",
            Self::UnsafeCandidate => "diagnostic_resolver.unsafe_candidate",
            Self::RevalidationFailed => "diagnostic_resolver.revalidation_failed",
            Self::ArchiveFailed => "diagnostic_resolver.archive_failed",
            Self::HttpFailed => "diagnostic_resolver.http_failed",
            Self::TaskFailed => "diagnostic_resolver.task_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolverError {
    code: ResolverErrorCode,
    detail: String,
    http_code: Option<HttpClientErrorCode>,
}

impl ResolverError {
    fn new(code: ResolverErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            http_code: None,
        }
    }

    fn http(error: HttpClientError) -> Self {
        Self {
            code: ResolverErrorCode::HttpFailed,
            detail: error.to_string(),
            http_code: Some(error.code()),
        }
    }

    pub(crate) const fn code(&self) -> ResolverErrorCode {
        self.code
    }

    pub(crate) const fn http_code(&self) -> Option<HttpClientErrorCode> {
        self.http_code
    }
}

impl fmt::Display for ResolverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for ResolverError {}

#[derive(Debug)]
pub(crate) enum ResolvedDiagnosticTarget {
    Live(DiagnosticHttpClient),
    Archive(ArchiveTarget),
}

impl ResolvedDiagnosticTarget {
    pub(crate) const fn run_id(&self) -> CanonicalUuid {
        match self {
            Self::Live(client) => client.run_id(),
            Self::Archive(archive) => archive.run_id(),
        }
    }

    pub(crate) const fn is_live(&self) -> bool {
        matches!(self, Self::Live(_))
    }

    pub(crate) const fn as_http(&self) -> Option<&DiagnosticHttpClient> {
        match self {
            Self::Live(client) => Some(client),
            Self::Archive(_) => None,
        }
    }

    pub(crate) fn as_archive_mut(&mut self) -> Option<&mut ArchiveTarget> {
        match self {
            Self::Live(_) => None,
            Self::Archive(archive) => Some(archive),
        }
    }
}

pub(crate) async fn resolve(
    target: DiagnosticTarget,
) -> Result<ResolvedDiagnosticTarget, ResolverError> {
    match target {
        DiagnosticTarget::Url(url) => DiagnosticHttpClient::connect(url.into_inner())
            .await
            .map(ResolvedDiagnosticTarget::Live)
            .map_err(ResolverError::http),
        DiagnosticTarget::Archive(run_directory) => tokio::task::spawn_blocking(move || {
            ArchiveTarget::open_identified(&run_directory)
                .map(ResolvedDiagnosticTarget::Archive)
                .map_err(|error| archive_error(&error))
        })
        .await
        .map_err(|error| {
            ResolverError::new(
                ResolverErrorCode::TaskFailed,
                format!("archive resolution task failed: {error}"),
            )
        })?,
        DiagnosticTarget::Production { production, run } => {
            tokio::task::spawn_blocking(move || {
                let root = fs::canonicalize(&production).map_err(|error| {
                    ResolverError::new(
                        ResolverErrorCode::InvalidProductionRoot,
                        format!("cannot resolve {}: {error}", production.display()),
                    )
                })?;
                let server_probe =
                    BlockingRegistryIdentityProbe::new().map_err(ResolverError::http)?;
                resolve_production_with(
                    &root,
                    run.map(|run_id| run_id.get()),
                    &RealProcessIdentityProbe,
                    &server_probe,
                )
            })
            .await
            .map_err(|error| {
                ResolverError::new(
                    ResolverErrorCode::TaskFailed,
                    format!("production resolution task failed: {error}"),
                )
            })?
        }
    }
}

pub(crate) fn resolve_production_with(
    production_root: &Path,
    requested_run_id: Option<CanonicalUuid>,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<ResolvedDiagnosticTarget, ResolverError> {
    let candidates =
        discover_with(production_root, process_probe, server_probe).map_err(|error| {
            ResolverError::new(ResolverErrorCode::DiscoveryFailed, format!("{}", error))
        })?;
    resolve_discovered_production(&candidates, requested_run_id, process_probe, server_probe)
}

pub(crate) fn resolve_discovered_production(
    candidates: &[DiscoveryCandidate],
    requested_run_id: Option<CanonicalUuid>,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<ResolvedDiagnosticTarget, ResolverError> {
    match requested_run_id {
        Some(run_id) => resolve_explicit_run(candidates, run_id, process_probe, server_probe),
        None => resolve_implicit_run(candidates, process_probe, server_probe),
    }
}

fn resolve_explicit_run(
    candidates: &[DiscoveryCandidate],
    run_id: CanonicalUuid,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<ResolvedDiagnosticTarget, ResolverError> {
    let mut matching = candidates
        .iter()
        .filter(|candidate| candidate.path_run_id() == Some(run_id));
    let Some(candidate) = matching.next() else {
        return Err(ResolverError::new(
            ResolverErrorCode::TargetNotFound,
            format!("Run {run_id} is not present"),
        ));
    };
    if matching.next().is_some() {
        return Err(ResolverError::new(
            ResolverErrorCode::UnsafeCandidate,
            format!("Run {run_id} has multiple discovery candidates"),
        ));
    }

    match (candidate.source(), candidate.classification()) {
        (CandidateSource::Instance, CandidateClassification::Active) => {
            resolve_active(candidate, process_probe, server_probe)
        }
        (CandidateSource::Instance, CandidateClassification::DefiniteStale) => {
            revalidate_stale(candidate, process_probe, server_probe)?;
            open_candidate_archive(candidate, run_id)
        }
        (CandidateSource::Archive, CandidateClassification::Completed)
        | (CandidateSource::Archive, CandidateClassification::Incomplete) => {
            open_candidate_archive(candidate, run_id)
        }
        (_, classification) => Err(ResolverError::new(
            ResolverErrorCode::UnsafeCandidate,
            format!(
                "Run {run_id} is classified as {} and cannot be bypassed",
                classification.as_str()
            ),
        )),
    }
}

fn resolve_implicit_run(
    candidates: &[DiscoveryCandidate],
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<ResolvedDiagnosticTarget, ResolverError> {
    let potentially_live = candidates
        .iter()
        .filter(|candidate| candidate.is_potentially_live())
        .collect::<Vec<_>>();
    let active = potentially_live
        .iter()
        .copied()
        .filter(|candidate| candidate.classification() == CandidateClassification::Active)
        .collect::<Vec<_>>();

    if !potentially_live.is_empty() {
        if potentially_live.len() == 1 && active.len() == 1 {
            return resolve_active(active[0], process_probe, server_probe);
        }
        return Err(ResolverError::new(
            ResolverErrorCode::AmbiguousLiveTarget,
            "active or potentially-live instances require an explicit Run ID",
        ));
    }

    for candidate in candidates.iter().filter(|candidate| {
        candidate.source() == CandidateSource::Instance
            && candidate.classification() == CandidateClassification::DefiniteStale
    }) {
        revalidate_stale(candidate, process_probe, server_probe)?;
    }

    let archives = candidates
        .iter()
        .filter(
            |candidate| match (candidate.source(), candidate.classification()) {
                (CandidateSource::Archive, CandidateClassification::Completed)
                | (CandidateSource::Archive, CandidateClassification::Incomplete) => true,
                (CandidateSource::Instance, CandidateClassification::DefiniteStale) => {
                    candidate.archive_present()
                }
                _ => false,
            },
        )
        .collect::<Vec<_>>();

    let mut opened = Vec::new();
    let mut first_archive_error = None;
    for candidate in archives {
        let Some(run_id) = candidate.run_id() else {
            first_archive_error.get_or_insert_with(|| {
                ResolverError::new(
                    ResolverErrorCode::UnsafeCandidate,
                    "archive Run identity is not trusted",
                )
            });
            continue;
        };
        match open_candidate_archive_target(candidate, run_id) {
            Ok(target) => opened.push(target),
            Err(error) => {
                first_archive_error.get_or_insert(error);
            }
        }
    }

    match opened.len() {
        0 => {
            if let Some(error) = first_archive_error {
                return Err(error);
            }
            let has_unusable_archive = candidates.iter().any(|candidate| {
                candidate.source() == CandidateSource::Archive || candidate.archive_present()
            });
            Err(ResolverError::new(
                if has_unusable_archive {
                    ResolverErrorCode::UnsafeCandidate
                } else {
                    ResolverErrorCode::TargetNotFound
                },
                if has_unusable_archive {
                    "no structurally valid archive can be selected"
                } else {
                    "no diagnostic Run is present"
                },
            ))
        }
        1 => Ok(ResolvedDiagnosticTarget::Archive(
            opened.pop().expect("one opened archive"),
        )),
        _ => Err(ResolverError::new(
            ResolverErrorCode::AmbiguousArchiveTarget,
            "multiple valid archives require an explicit Run ID",
        )),
    }
}

fn resolve_active(
    candidate: &DiscoveryCandidate,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<ResolvedDiagnosticTarget, ResolverError> {
    let result = revalidate_for_use(candidate, process_probe, server_probe);
    let refreshed = result.candidate().ok_or_else(|| {
        ResolverError::new(
            ResolverErrorCode::RevalidationFailed,
            format!(
                "active candidate revalidation was refused: {:?}",
                result.status()
            ),
        )
    })?;
    let entry = refreshed.registry_entry().ok_or_else(|| {
        ResolverError::new(
            ResolverErrorCode::RevalidationFailed,
            "revalidated active candidate has no registry entry",
        )
    })?;
    DiagnosticHttpClient::from_validated_registry_entry(entry)
        .map(ResolvedDiagnosticTarget::Live)
        .map_err(ResolverError::http)
}

fn revalidate_stale(
    candidate: &DiscoveryCandidate,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<(), ResolverError> {
    let result = revalidate_for_cleanup(candidate, process_probe, server_probe);
    if result.is_authorized() {
        Ok(())
    } else {
        Err(ResolverError::new(
            ResolverErrorCode::RevalidationFailed,
            format!(
                "definite-stale candidate revalidation was refused: {:?}",
                result.status()
            ),
        ))
    }
}

fn open_candidate_archive(
    candidate: &DiscoveryCandidate,
    expected_run_id: CanonicalUuid,
) -> Result<ResolvedDiagnosticTarget, ResolverError> {
    open_candidate_archive_target(candidate, expected_run_id).map(ResolvedDiagnosticTarget::Archive)
}

fn open_candidate_archive_target(
    candidate: &DiscoveryCandidate,
    expected_run_id: CanonicalUuid,
) -> Result<ArchiveTarget, ResolverError> {
    let run_directory = candidate.archive_directory().ok_or_else(|| {
        ResolverError::new(
            ResolverErrorCode::TargetNotFound,
            format!("Run {expected_run_id} has no archive directory"),
        )
    })?;
    if !candidate.archive_present() {
        return Err(ResolverError::new(
            ResolverErrorCode::TargetNotFound,
            format!("Run {expected_run_id} has no archive"),
        ));
    }
    ArchiveTarget::open_expected(run_directory, expected_run_id)
        .map_err(|error| archive_error(&error))
}

fn archive_error(error: &impl fmt::Display) -> ResolverError {
    ResolverError::new(ResolverErrorCode::ArchiveFailed, error.to_string())
}
