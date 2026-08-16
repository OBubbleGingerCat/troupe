use super::{
    codec::decode_registry_entry,
    discover::{
        CandidateClassification, DiscoveryCandidate, ProcessIdentityProbe, ServerIdentityProbe,
        classify_registry_entry, read_registry_snapshot,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevalidationPurpose {
    UseActive,
    CleanupDefiniteStale,
}

impl RevalidationPurpose {
    const fn required_classification(self) -> CandidateClassification {
        match self {
            Self::UseActive => CandidateClassification::Active,
            Self::CleanupDefiniteStale => CandidateClassification::DefiniteStale,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevalidationStatus {
    Authorized,
    CandidateNotEligible,
    LocatorUnavailable,
    LocatorChanged,
    ClassificationChanged,
}

#[derive(Clone, Debug)]
pub struct RevalidationResult {
    purpose: RevalidationPurpose,
    status: RevalidationStatus,
    observed_classification: Option<CandidateClassification>,
    candidate: Option<DiscoveryCandidate>,
}

impl RevalidationResult {
    pub const fn purpose(&self) -> RevalidationPurpose {
        self.purpose
    }

    pub const fn status(&self) -> RevalidationStatus {
        self.status
    }

    pub const fn is_authorized(&self) -> bool {
        matches!(self.status, RevalidationStatus::Authorized)
    }

    pub const fn observed_classification(&self) -> Option<CandidateClassification> {
        self.observed_classification
    }

    /// Returns the freshly read and identity-checked candidate only when the requested action is
    /// authorized.
    pub const fn candidate(&self) -> Option<&DiscoveryCandidate> {
        if self.is_authorized() {
            self.candidate.as_ref()
        } else {
            None
        }
    }

    pub const fn observed_candidate(&self) -> Option<&DiscoveryCandidate> {
        self.candidate.as_ref()
    }

    fn refused(purpose: RevalidationPurpose, status: RevalidationStatus) -> Self {
        Self {
            purpose,
            status,
            observed_classification: None,
            candidate: None,
        }
    }
}

pub fn revalidate_for_use(
    candidate: &DiscoveryCandidate,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> RevalidationResult {
    revalidate(
        candidate,
        RevalidationPurpose::UseActive,
        process_probe,
        server_probe,
    )
}

pub fn revalidate_for_cleanup(
    candidate: &DiscoveryCandidate,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> RevalidationResult {
    revalidate(
        candidate,
        RevalidationPurpose::CleanupDefiniteStale,
        process_probe,
        server_probe,
    )
}

pub fn revalidate(
    candidate: &DiscoveryCandidate,
    purpose: RevalidationPurpose,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> RevalidationResult {
    let required = purpose.required_classification();
    if candidate.classification() != required {
        return RevalidationResult::refused(purpose, RevalidationStatus::CandidateNotEligible);
    }
    let (Some(expected_snapshot), Some(expected_entry)) =
        (candidate.registry_snapshot(), candidate.registry_entry())
    else {
        return RevalidationResult::refused(purpose, RevalidationStatus::CandidateNotEligible);
    };

    let snapshot = match read_registry_snapshot(candidate.path()) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            return RevalidationResult::refused(purpose, RevalidationStatus::LocatorUnavailable);
        }
    };
    if snapshot != *expected_snapshot {
        return RevalidationResult::refused(purpose, RevalidationStatus::LocatorChanged);
    }
    let entry = match decode_registry_entry(candidate.path(), snapshot.bytes()) {
        Ok(entry) if entry == *expected_entry => entry,
        Ok(_) | Err(_) => {
            return RevalidationResult::refused(purpose, RevalidationStatus::LocatorChanged);
        }
    };

    let (classification, detail) = classify_registry_entry(&entry, process_probe, server_probe);
    let refreshed = candidate.revalidated(classification, entry, snapshot, detail);
    RevalidationResult {
        purpose,
        status: if classification == required {
            RevalidationStatus::Authorized
        } else {
            RevalidationStatus::ClassificationChanged
        },
        observed_classification: Some(classification),
        candidate: Some(refreshed),
    }
}
