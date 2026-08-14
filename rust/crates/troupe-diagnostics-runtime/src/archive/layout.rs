use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use troupe_diagnostics_core::id::CanonicalUuid;

use super::probe::{
    ArchiveDirectory, ArchiveFileSystem, NodeKind, NodeMetadata, ProbeError, ProbeErrorCode,
    RealFileSystem, WRITE_PROBE_PREFIX, verify_directory_is_writable,
};

const OWNER_DIRECTORY_MODE: u32 = 0o700;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveStartupErrorCode {
    InvalidProductionRoot,
    DirectoryInspectFailed,
    SymlinkRejected,
    NotDirectory,
    DirectoryIdentityChanged,
    DirectoryCreateFailed,
    DirectoryPermissionFailed,
    DirectoryModeMismatch,
    RunIdentityCollision,
    ProbeCreateFailed,
    ProbeWriteFailed,
    ProbeSyncFailed,
    ProbeCloseFailed,
    ProbeUnlinkFailed,
}

impl ArchiveStartupErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidProductionRoot => "archive.invalid_production_root",
            Self::DirectoryInspectFailed => "archive.directory_inspect_failed",
            Self::SymlinkRejected => "archive.symlink_rejected",
            Self::NotDirectory => "archive.not_directory",
            Self::DirectoryIdentityChanged => "archive.directory_identity_changed",
            Self::DirectoryCreateFailed => "archive.directory_create_failed",
            Self::DirectoryPermissionFailed => "archive.directory_permission_failed",
            Self::DirectoryModeMismatch => "archive.directory_mode_mismatch",
            Self::RunIdentityCollision => "archive.run_identity_collision",
            Self::ProbeCreateFailed => "archive.probe_create_failed",
            Self::ProbeWriteFailed => "archive.probe_write_failed",
            Self::ProbeSyncFailed => "archive.probe_sync_failed",
            Self::ProbeCloseFailed => "archive.probe_close_failed",
            Self::ProbeUnlinkFailed => "archive.probe_unlink_failed",
        }
    }
}

#[derive(Debug)]
pub struct ArchiveStartupError {
    code: ArchiveStartupErrorCode,
    path: PathBuf,
    source: io::Error,
}

impl ArchiveStartupError {
    fn new(code: ArchiveStartupErrorCode, path: &Path, source: io::Error) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            source,
        }
    }

    fn logical(code: ArchiveStartupErrorCode, path: &Path) -> Self {
        let kind = match code {
            ArchiveStartupErrorCode::RunIdentityCollision => io::ErrorKind::AlreadyExists,
            ArchiveStartupErrorCode::InvalidProductionRoot
            | ArchiveStartupErrorCode::SymlinkRejected
            | ArchiveStartupErrorCode::NotDirectory
            | ArchiveStartupErrorCode::DirectoryIdentityChanged
            | ArchiveStartupErrorCode::DirectoryModeMismatch => io::ErrorKind::InvalidData,
            _ => io::ErrorKind::Other,
        };
        Self::new(code, path, io::Error::from(kind))
    }

    fn from_probe(error: ProbeError) -> Self {
        let code = match error.code() {
            ProbeErrorCode::Create => ArchiveStartupErrorCode::ProbeCreateFailed,
            ProbeErrorCode::Write => ArchiveStartupErrorCode::ProbeWriteFailed,
            ProbeErrorCode::Sync => ArchiveStartupErrorCode::ProbeSyncFailed,
            ProbeErrorCode::Close => ArchiveStartupErrorCode::ProbeCloseFailed,
            ProbeErrorCode::Unlink => ArchiveStartupErrorCode::ProbeUnlinkFailed,
        };
        let path = error.path().to_path_buf();
        Self::new(code, &path, error.into_source())
    }

    pub const fn code(&self) -> ArchiveStartupErrorCode {
        self.code
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source_kind(&self) -> io::ErrorKind {
        self.source.kind()
    }
}

impl fmt::Display for ArchiveStartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic archive startup failed [{}] at {}",
            self.code.as_str(),
            self.path.display()
        )
    }
}

impl std::error::Error for ArchiveStartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
pub struct ArchiveLayout {
    production_root: PathBuf,
    troupe_directory: PathBuf,
    diagnostics_directory: PathBuf,
    instances_directory: PathBuf,
    runs_directory: PathBuf,
    run_directory: PathBuf,
    run_id: CanonicalUuid,
}

impl ArchiveLayout {
    pub fn prepare(
        production_root: &Path,
        run_id: CanonicalUuid,
    ) -> Result<Self, ArchiveStartupError> {
        Self::prepare_with(&RealFileSystem, production_root, run_id)
    }

    pub fn prepare_with<F: ArchiveFileSystem>(
        filesystem: &F,
        production_root: &Path,
        run_id: CanonicalUuid,
    ) -> Result<Self, ArchiveStartupError> {
        if !production_root.is_absolute() {
            return Err(ArchiveStartupError::logical(
                ArchiveStartupErrorCode::InvalidProductionRoot,
                production_root,
            ));
        }
        inspect_directory(filesystem, production_root, false)?;

        let troupe_directory = production_root.join(".troupe");
        let (troupe_identity, troupe_created) =
            ensure_state_directory(filesystem, &troupe_directory)?;
        let probe_name = format!("{WRITE_PROBE_PREFIX}{run_id}");
        if let Err(error) = verify_directory_is_writable(filesystem, &troupe_directory, &probe_name)
        {
            if troupe_created {
                cleanup_new_directory(filesystem, &troupe_directory, troupe_identity);
            }
            return Err(ArchiveStartupError::from_probe(error));
        }

        let diagnostics_directory = troupe_directory.join("diagnostics");
        ensure_owned_directory(filesystem, &diagnostics_directory)?;
        let instances_directory = diagnostics_directory.join("instances");
        ensure_owned_directory(filesystem, &instances_directory)?;
        let runs_directory = diagnostics_directory.join("runs");
        ensure_owned_directory(filesystem, &runs_directory)?;
        let run_directory = runs_directory.join(run_id.to_string());
        create_run_directory(filesystem, &run_directory)?;

        Ok(Self {
            production_root: production_root.to_path_buf(),
            troupe_directory,
            diagnostics_directory,
            instances_directory,
            runs_directory,
            run_directory,
            run_id,
        })
    }

    pub fn production_root(&self) -> &Path {
        &self.production_root
    }

    pub fn troupe_directory(&self) -> &Path {
        &self.troupe_directory
    }

    pub fn diagnostics_directory(&self) -> &Path {
        &self.diagnostics_directory
    }

    pub fn instances_directory(&self) -> &Path {
        &self.instances_directory
    }

    pub fn runs_directory(&self) -> &Path {
        &self.runs_directory
    }

    pub fn run_directory(&self) -> &Path {
        &self.run_directory
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }
}

fn inspect_directory<F: ArchiveFileSystem>(
    filesystem: &F,
    path: &Path,
    secure: bool,
) -> Result<NodeMetadata, ArchiveStartupError> {
    let path_metadata = filesystem.path_metadata(path).map_err(|error| {
        ArchiveStartupError::new(ArchiveStartupErrorCode::DirectoryInspectFailed, path, error)
    })?;
    validate_directory_kind(path, path_metadata)?;

    let mut directory = filesystem.open_directory(path).map_err(|error| {
        ArchiveStartupError::new(ArchiveStartupErrorCode::DirectoryInspectFailed, path, error)
    })?;
    let opened_metadata = directory.fstat().map_err(|error| {
        ArchiveStartupError::new(ArchiveStartupErrorCode::DirectoryInspectFailed, path, error)
    })?;
    validate_directory_kind(path, opened_metadata)?;
    validate_identity(path, path_metadata, opened_metadata)?;

    if !secure {
        return Ok(opened_metadata);
    }

    directory.chmod(OWNER_DIRECTORY_MODE).map_err(|error| {
        ArchiveStartupError::new(
            ArchiveStartupErrorCode::DirectoryPermissionFailed,
            path,
            error,
        )
    })?;
    let secured_metadata = directory.fstat().map_err(|error| {
        ArchiveStartupError::new(ArchiveStartupErrorCode::DirectoryInspectFailed, path, error)
    })?;
    validate_directory_kind(path, secured_metadata)?;
    validate_identity(path, opened_metadata, secured_metadata)?;
    if secured_metadata.mode != OWNER_DIRECTORY_MODE {
        return Err(ArchiveStartupError::logical(
            ArchiveStartupErrorCode::DirectoryModeMismatch,
            path,
        ));
    }

    let final_path_metadata = filesystem.path_metadata(path).map_err(|error| {
        ArchiveStartupError::new(ArchiveStartupErrorCode::DirectoryInspectFailed, path, error)
    })?;
    validate_directory_kind(path, final_path_metadata)?;
    validate_identity(path, secured_metadata, final_path_metadata)?;
    Ok(secured_metadata)
}

fn validate_directory_kind(path: &Path, metadata: NodeMetadata) -> Result<(), ArchiveStartupError> {
    match metadata.kind {
        NodeKind::Directory => Ok(()),
        NodeKind::Symlink => Err(ArchiveStartupError::logical(
            ArchiveStartupErrorCode::SymlinkRejected,
            path,
        )),
        NodeKind::Other => Err(ArchiveStartupError::logical(
            ArchiveStartupErrorCode::NotDirectory,
            path,
        )),
    }
}

fn validate_identity(
    path: &Path,
    before: NodeMetadata,
    after: NodeMetadata,
) -> Result<(), ArchiveStartupError> {
    if before.same_identity(after) {
        Ok(())
    } else {
        Err(ArchiveStartupError::logical(
            ArchiveStartupErrorCode::DirectoryIdentityChanged,
            path,
        ))
    }
}

fn ensure_owned_directory<F: ArchiveFileSystem>(
    filesystem: &F,
    path: &Path,
) -> Result<NodeMetadata, ArchiveStartupError> {
    match filesystem.path_metadata(path) {
        Ok(metadata) => {
            validate_directory_kind(path, metadata)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            filesystem
                .mkdir(path, OWNER_DIRECTORY_MODE)
                .map_err(|error| {
                    let code = if error.kind() == io::ErrorKind::AlreadyExists {
                        ArchiveStartupErrorCode::DirectoryIdentityChanged
                    } else {
                        ArchiveStartupErrorCode::DirectoryCreateFailed
                    };
                    ArchiveStartupError::new(code, path, error)
                })?;
        }
        Err(error) => {
            return Err(ArchiveStartupError::new(
                ArchiveStartupErrorCode::DirectoryInspectFailed,
                path,
                error,
            ));
        }
    }
    inspect_directory(filesystem, path, true)
}

fn ensure_state_directory<F: ArchiveFileSystem>(
    filesystem: &F,
    path: &Path,
) -> Result<(NodeMetadata, bool), ArchiveStartupError> {
    let created = match filesystem.path_metadata(path) {
        Ok(metadata) => {
            validate_directory_kind(path, metadata)?;
            false
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            filesystem
                .mkdir(path, OWNER_DIRECTORY_MODE)
                .map_err(|error| {
                    let code = if error.kind() == io::ErrorKind::AlreadyExists {
                        ArchiveStartupErrorCode::DirectoryIdentityChanged
                    } else {
                        ArchiveStartupErrorCode::DirectoryCreateFailed
                    };
                    ArchiveStartupError::new(code, path, error)
                })?;
            true
        }
        Err(error) => {
            return Err(ArchiveStartupError::new(
                ArchiveStartupErrorCode::DirectoryInspectFailed,
                path,
                error,
            ));
        }
    };
    inspect_directory(filesystem, path, created).map(|metadata| (metadata, created))
}

fn create_run_directory<F: ArchiveFileSystem>(
    filesystem: &F,
    path: &Path,
) -> Result<NodeMetadata, ArchiveStartupError> {
    if let Err(error) = filesystem.mkdir(path, OWNER_DIRECTORY_MODE) {
        if error.kind() == io::ErrorKind::AlreadyExists {
            return classify_run_collision(filesystem, path);
        }
        return Err(ArchiveStartupError::new(
            ArchiveStartupErrorCode::DirectoryCreateFailed,
            path,
            error,
        ));
    }
    inspect_directory(filesystem, path, true)
}

fn classify_run_collision<F: ArchiveFileSystem>(
    filesystem: &F,
    path: &Path,
) -> Result<NodeMetadata, ArchiveStartupError> {
    let metadata = filesystem.path_metadata(path).map_err(|error| {
        ArchiveStartupError::new(ArchiveStartupErrorCode::DirectoryInspectFailed, path, error)
    })?;
    match metadata.kind {
        NodeKind::Symlink => Err(ArchiveStartupError::logical(
            ArchiveStartupErrorCode::SymlinkRejected,
            path,
        )),
        NodeKind::Other => Err(ArchiveStartupError::logical(
            ArchiveStartupErrorCode::NotDirectory,
            path,
        )),
        NodeKind::Directory => Err(ArchiveStartupError::logical(
            ArchiveStartupErrorCode::RunIdentityCollision,
            path,
        )),
    }
}

fn cleanup_new_directory<F: ArchiveFileSystem>(
    filesystem: &F,
    path: &Path,
    expected: NodeMetadata,
) {
    let Ok(current) = filesystem.path_metadata(path) else {
        return;
    };
    if current.kind == NodeKind::Directory && current.same_identity(expected) {
        let _ = filesystem.remove_directory(path);
    }
}
