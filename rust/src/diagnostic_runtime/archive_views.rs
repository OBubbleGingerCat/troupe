use std::{fmt, path::Path};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::query::{
    archive_views::{ArchiveViewLoadError, StoredViewCatalog, load_stored_view_records},
    reader::{DiagnosticReader, ReaderFailure, ReaderFailureClass},
};

#[derive(Debug)]
pub(crate) enum ArchiveViewOperationError {
    Reader(ReaderFailure),
    Views(ArchiveViewLoadError),
}

impl ArchiveViewOperationError {
    pub(crate) const fn class(&self) -> ReaderFailureClass {
        match self {
            Self::Reader(error) => error.class(),
            Self::Views(error) => error.class(),
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Reader(error) => error.code().as_str(),
            Self::Views(error) => error.code().as_str(),
        }
    }
}

impl fmt::Display for ArchiveViewOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reader(error) => fmt::Display::fmt(error, formatter),
            Self::Views(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ArchiveViewOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reader(error) => Some(error),
            Self::Views(error) => Some(error),
        }
    }
}

impl From<ReaderFailure> for ArchiveViewOperationError {
    fn from(error: ReaderFailure) -> Self {
        Self::Reader(error)
    }
}

impl From<ArchiveViewLoadError> for ArchiveViewOperationError {
    fn from(error: ArchiveViewLoadError) -> Self {
        Self::Views(error)
    }
}

pub(crate) fn open_archive_views(
    run_directory: &Path,
    expected_run_id: CanonicalUuid,
) -> Result<StoredViewCatalog, ArchiveViewOperationError> {
    let mut reader = DiagnosticReader::open_archive(run_directory, expected_run_id)?;
    let source = reader.capture()?;
    Ok(load_stored_view_records(&source)?)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use troupe_diagnostics_runtime::{
        archive::lease::ActiveArchiveLease,
        query::reader::ReaderFailureClass,
        store::{
            connection::{DiagnosticStore, InitialStoreMetadata},
            view_records::{CompiledViewSet, persist_view_set},
        },
    };

    use super::*;

    const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
    const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestRunDirectory(PathBuf);

    impl TestRunDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "troupe-b13-runtime-archive-views-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test Run directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRunDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_id(value: &str) -> CanonicalUuid {
        CanonicalUuid::parse(value).expect("canonical test Run UUID")
    }

    fn create_empty_archive(directory: &Path) {
        let active_lease = ActiveArchiveLease::acquire(directory).expect("acquire active lease");
        drop(
            DiagnosticStore::create(
                directory,
                &InitialStoreMetadata::new(
                    run_id(RUN_ID),
                    "2026-08-16T00:00:00Z",
                    "configuration-sha256:b13-runtime",
                ),
            )
            .expect("create diagnostic store"),
        );
        let compiled = CompiledViewSet::from_json_records(std::iter::empty::<&[u8]>())
            .expect("compile empty view set");
        persist_view_set(directory, run_id(RUN_ID), &compiled).expect("persist empty view set");
        drop(active_lease);
    }

    #[test]
    fn opens_an_inactive_archive_through_the_q00_reader() {
        let directory = TestRunDirectory::new();
        create_empty_archive(directory.path());

        let catalog = open_archive_views(directory.path(), run_id(RUN_ID))
            .expect("open compatible archive view catalog");
        assert!(catalog.views().is_empty());
    }

    #[test]
    fn store_identity_mismatch_fails_the_archive_operation() {
        let directory = TestRunDirectory::new();
        create_empty_archive(directory.path());

        let error = open_archive_views(directory.path(), run_id(OTHER_RUN_ID))
            .expect_err("reject mismatched archive Run identity");
        assert_eq!(error.class(), ReaderFailureClass::ArchiveOperation);
        assert_eq!(error.code(), "diagnostic_reader.store_validation");
    }
}
