#![allow(dead_code)] // D07 and the finite query nodes wire this private target.

use std::{fmt, path::Path};

use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};
use troupe_diagnostics_runtime::query::reader::{
    CapturedEventSource, DiagnosticReader, ReaderFailure,
};

/// A validated archive reader that owns its Q00 shared lease for the target lifetime.
pub(crate) struct ArchiveTarget {
    reader: DiagnosticReader<'static>,
    run_id: CanonicalUuid,
    validated_watermark: SchemaU64,
}

impl fmt::Debug for ArchiveTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArchiveTarget")
            .field("run_id", &self.run_id)
            .field("validated_watermark", &self.validated_watermark)
            .field("run_directory", &self.reader.run_directory())
            .finish_non_exhaustive()
    }
}

impl ArchiveTarget {
    /// Opens a copied archive whose directory name is not part of its identity.
    pub(crate) fn open_identified(run_directory: &Path) -> Result<Self, ReaderFailure> {
        let reader = DiagnosticReader::open_identified_archive(run_directory)?;
        Self::finish_validation(reader)
    }

    /// Opens a production-owned archive whose selected Run identity is already known.
    pub(crate) fn open_expected(
        run_directory: &Path,
        expected_run_id: CanonicalUuid,
    ) -> Result<Self, ReaderFailure> {
        let reader = DiagnosticReader::open_archive(run_directory, expected_run_id)?;
        Self::finish_validation(reader)
    }

    fn finish_validation(mut reader: DiagnosticReader<'static>) -> Result<Self, ReaderFailure> {
        let captured = reader.capture()?;
        let run_id = captured.metadata().run_id();
        let validated_watermark = captured.captured_watermark();
        drop(captured);
        Ok(Self {
            reader,
            run_id,
            validated_watermark,
        })
    }

    pub(crate) const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub(crate) const fn validated_watermark(&self) -> SchemaU64 {
        self.validated_watermark
    }

    pub(crate) fn run_directory(&self) -> &Path {
        self.reader.run_directory()
    }

    /// Captures a fresh finite prefix while retaining the reader-owned shared lease.
    pub(crate) fn capture(&mut self) -> Result<CapturedEventSource<'_>, ReaderFailure> {
        self.reader.capture()
    }
}
