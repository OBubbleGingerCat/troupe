use std::{
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use fs4::{FileExt, TryLockError};

use super::constants::{ARCHIVE_LEASE_ANCHOR_FILENAME, ARCHIVE_LEASE_ANCHOR_MODE};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveLeaseMode {
    Active,
    SharedReader,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveLeaseErrorCode {
    InvalidRunDirectory,
    AnchorInspectFailed,
    AnchorSymlinkRejected,
    AnchorNotFile,
    AnchorOpenFailed,
    AnchorIdentityChanged,
    AnchorPermissionFailed,
    AnchorModeMismatch,
    Contended,
    LockFailed,
}

impl ArchiveLeaseErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRunDirectory => "archive_lease.invalid_run_directory",
            Self::AnchorInspectFailed => "archive_lease.anchor_inspect_failed",
            Self::AnchorSymlinkRejected => "archive_lease.anchor_symlink_rejected",
            Self::AnchorNotFile => "archive_lease.anchor_not_file",
            Self::AnchorOpenFailed => "archive_lease.anchor_open_failed",
            Self::AnchorIdentityChanged => "archive_lease.anchor_identity_changed",
            Self::AnchorPermissionFailed => "archive_lease.anchor_permission_failed",
            Self::AnchorModeMismatch => "archive_lease.anchor_mode_mismatch",
            Self::Contended => "archive_lease.contended",
            Self::LockFailed => "archive_lease.lock_failed",
        }
    }
}

#[derive(Debug)]
pub struct ArchiveLeaseError {
    code: ArchiveLeaseErrorCode,
    mode: ArchiveLeaseMode,
    path: PathBuf,
    source: Option<io::Error>,
}

impl ArchiveLeaseError {
    fn new(
        code: ArchiveLeaseErrorCode,
        mode: ArchiveLeaseMode,
        path: &Path,
        source: Option<io::Error>,
    ) -> Self {
        Self {
            code,
            mode,
            path: path.to_path_buf(),
            source,
        }
    }

    pub const fn code(&self) -> ArchiveLeaseErrorCode {
        self.code
    }

    pub const fn mode(&self) -> ArchiveLeaseMode {
        self.mode
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source_kind(&self) -> Option<io::ErrorKind> {
        self.source.as_ref().map(io::Error::kind)
    }
}

impl fmt::Display for ArchiveLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic archive lease failed [{}] at {}",
            self.code.as_str(),
            self.path.display()
        )
    }
}

impl std::error::Error for ArchiveLeaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseAnchorMetadata {
    is_file: bool,
    is_symlink: bool,
    identity: FileIdentity,
    mode: u32,
}

impl LeaseAnchorMetadata {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            is_file: metadata.file_type().is_file(),
            is_symlink: metadata.file_type().is_symlink(),
            identity: FileIdentity::from_metadata(metadata),
            mode: metadata_mode(metadata),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    first: u64,
    second: u64,
}

impl FileIdentity {
    #[cfg(unix)]
    fn from_metadata(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            first: metadata.dev(),
            second: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            first: metadata.len(),
            second: u64::from(metadata.permissions().readonly()),
        }
    }
}

pub trait ArchiveLeaseHandle: Send {
    fn metadata(&self) -> io::Result<LeaseAnchorMetadata>;
    fn set_owner_only(&self) -> io::Result<()>;
    fn try_lock_shared(&self) -> Result<(), TryLockError>;
    fn try_lock_exclusive(&self) -> Result<(), TryLockError>;
}

pub trait ArchiveLeaseOpener: Send + Sync {
    fn open(&self, path: &Path, create_new: bool) -> io::Result<Box<dyn ArchiveLeaseHandle>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealArchiveLeaseOpener;

impl ArchiveLeaseOpener for RealArchiveLeaseOpener {
    fn open(&self, path: &Path, create_new: bool) -> io::Result<Box<dyn ArchiveLeaseHandle>> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if create_new {
            options.create_new(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.mode(ARCHIVE_LEASE_ANCHOR_MODE);
        }
        options
            .open(path)
            .map(|file| Box::new(RealArchiveLeaseHandle(file)) as Box<dyn ArchiveLeaseHandle>)
    }
}

struct RealArchiveLeaseHandle(File);

impl ArchiveLeaseHandle for RealArchiveLeaseHandle {
    fn metadata(&self) -> io::Result<LeaseAnchorMetadata> {
        self.0
            .metadata()
            .map(|metadata| LeaseAnchorMetadata::from_metadata(&metadata))
    }

    fn set_owner_only(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            self.0
                .set_permissions(fs::Permissions::from_mode(ARCHIVE_LEASE_ANCHOR_MODE))
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }

    fn try_lock_shared(&self) -> Result<(), TryLockError> {
        FileExt::try_lock_shared(&self.0)
    }

    fn try_lock_exclusive(&self) -> Result<(), TryLockError> {
        FileExt::try_lock(&self.0)
    }
}

pub struct ActiveArchiveLease {
    _handle: Box<dyn ArchiveLeaseHandle>,
    anchor_path: PathBuf,
}

impl fmt::Debug for ActiveArchiveLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveArchiveLease")
            .field("mode", &ArchiveLeaseMode::Active)
            .field("anchor_path", &self.anchor_path)
            .finish_non_exhaustive()
    }
}

pub struct ActiveArchiveLeaseGuard<'a> {
    lease: &'a ActiveArchiveLease,
}

impl ActiveArchiveLease {
    pub fn acquire(run_directory: &Path) -> Result<Self, ArchiveLeaseError> {
        Self::acquire_with(&RealArchiveLeaseOpener, run_directory)
    }

    pub fn acquire_with<O: ArchiveLeaseOpener>(
        opener: &O,
        run_directory: &Path,
    ) -> Result<Self, ArchiveLeaseError> {
        let (handle, anchor_path) =
            acquire(opener, run_directory, ArchiveLeaseMode::Active, true, false)?;
        Ok(Self {
            _handle: handle,
            anchor_path,
        })
    }

    pub fn guard(&self) -> ActiveArchiveLeaseGuard<'_> {
        ActiveArchiveLeaseGuard { lease: self }
    }

    pub fn anchor_path(&self) -> &Path {
        &self.anchor_path
    }
}

impl ActiveArchiveLeaseGuard<'_> {
    pub fn anchor_path(&self) -> &Path {
        self.lease.anchor_path()
    }
}

pub struct SharedArchiveLease {
    _handle: Box<dyn ArchiveLeaseHandle>,
    anchor_path: PathBuf,
}

impl fmt::Debug for SharedArchiveLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedArchiveLease")
            .field("mode", &ArchiveLeaseMode::SharedReader)
            .field("anchor_path", &self.anchor_path)
            .finish_non_exhaustive()
    }
}

impl SharedArchiveLease {
    pub fn acquire(run_directory: &Path) -> Result<Self, ArchiveLeaseError> {
        Self::acquire_with(&RealArchiveLeaseOpener, run_directory)
    }

    pub fn acquire_with<O: ArchiveLeaseOpener>(
        opener: &O,
        run_directory: &Path,
    ) -> Result<Self, ArchiveLeaseError> {
        let (handle, anchor_path) = acquire(
            opener,
            run_directory,
            ArchiveLeaseMode::SharedReader,
            false,
            true,
        )?;
        Ok(Self {
            _handle: handle,
            anchor_path,
        })
    }

    pub fn anchor_path(&self) -> &Path {
        &self.anchor_path
    }
}

pub struct CleanupArchiveLease {
    _handle: Box<dyn ArchiveLeaseHandle>,
    anchor_path: PathBuf,
}

impl fmt::Debug for CleanupArchiveLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CleanupArchiveLease")
            .field("mode", &ArchiveLeaseMode::Cleanup)
            .field("anchor_path", &self.anchor_path)
            .finish_non_exhaustive()
    }
}

impl CleanupArchiveLease {
    pub fn acquire(run_directory: &Path) -> Result<Self, ArchiveLeaseError> {
        Self::acquire_with(&RealArchiveLeaseOpener, run_directory)
    }

    pub fn acquire_with<O: ArchiveLeaseOpener>(
        opener: &O,
        run_directory: &Path,
    ) -> Result<Self, ArchiveLeaseError> {
        let (handle, anchor_path) = acquire(
            opener,
            run_directory,
            ArchiveLeaseMode::Cleanup,
            false,
            false,
        )?;
        Ok(Self {
            _handle: handle,
            anchor_path,
        })
    }

    pub fn anchor_path(&self) -> &Path {
        &self.anchor_path
    }
}

fn acquire<O: ArchiveLeaseOpener>(
    opener: &O,
    run_directory: &Path,
    mode: ArchiveLeaseMode,
    create_anchor: bool,
    shared: bool,
) -> Result<(Box<dyn ArchiveLeaseHandle>, PathBuf), ArchiveLeaseError> {
    validate_run_directory(run_directory, mode)?;
    let anchor_path = run_directory.join(ARCHIVE_LEASE_ANCHOR_FILENAME);
    let handle = open_anchor(opener, &anchor_path, mode, create_anchor)?;
    secure_and_revalidate_anchor(&anchor_path, handle.as_ref(), mode)?;
    let lock_result = if shared {
        handle.try_lock_shared()
    } else {
        handle.try_lock_exclusive()
    };
    match lock_result {
        Ok(()) => Ok((handle, anchor_path)),
        Err(TryLockError::WouldBlock) => Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::Contended,
            mode,
            &anchor_path,
            None,
        )),
        Err(TryLockError::Error(error)) => Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::LockFailed,
            mode,
            &anchor_path,
            Some(error),
        )),
    }
}

fn validate_run_directory(
    run_directory: &Path,
    mode: ArchiveLeaseMode,
) -> Result<(), ArchiveLeaseError> {
    let metadata = fs::symlink_metadata(run_directory).map_err(|error| {
        ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::InvalidRunDirectory,
            mode,
            run_directory,
            Some(error),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::InvalidRunDirectory,
            mode,
            run_directory,
            None,
        ));
    }
    Ok(())
}

fn open_anchor<O: ArchiveLeaseOpener>(
    opener: &O,
    anchor_path: &Path,
    mode: ArchiveLeaseMode,
    create_anchor: bool,
) -> Result<Box<dyn ArchiveLeaseHandle>, ArchiveLeaseError> {
    match inspect_anchor(anchor_path, mode)? {
        Some(_) => opener.open(anchor_path, false).map_err(|error| {
            ArchiveLeaseError::new(
                ArchiveLeaseErrorCode::AnchorOpenFailed,
                mode,
                anchor_path,
                Some(error),
            )
        }),
        None if !create_anchor => Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorOpenFailed,
            mode,
            anchor_path,
            Some(io::Error::from(io::ErrorKind::NotFound)),
        )),
        None => match opener.open(anchor_path, true) {
            Ok(handle) => Ok(handle),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                inspect_anchor(anchor_path, mode)?;
                opener.open(anchor_path, false).map_err(|retry_error| {
                    ArchiveLeaseError::new(
                        ArchiveLeaseErrorCode::AnchorOpenFailed,
                        mode,
                        anchor_path,
                        Some(retry_error),
                    )
                })
            }
            Err(error) => Err(ArchiveLeaseError::new(
                ArchiveLeaseErrorCode::AnchorOpenFailed,
                mode,
                anchor_path,
                Some(error),
            )),
        },
    }
}

fn inspect_anchor(
    anchor_path: &Path,
    mode: ArchiveLeaseMode,
) -> Result<Option<LeaseAnchorMetadata>, ArchiveLeaseError> {
    match fs::symlink_metadata(anchor_path) {
        Ok(metadata) => {
            let metadata = LeaseAnchorMetadata::from_metadata(&metadata);
            validate_anchor_kind(metadata, anchor_path, mode)?;
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorInspectFailed,
            mode,
            anchor_path,
            Some(error),
        )),
    }
}

fn secure_and_revalidate_anchor(
    anchor_path: &Path,
    handle: &dyn ArchiveLeaseHandle,
    mode: ArchiveLeaseMode,
) -> Result<(), ArchiveLeaseError> {
    let opened = handle.metadata().map_err(|error| {
        ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorInspectFailed,
            mode,
            anchor_path,
            Some(error),
        )
    })?;
    validate_anchor_kind(opened, anchor_path, mode)?;
    let path_metadata = inspect_anchor(anchor_path, mode)?.ok_or_else(|| {
        ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorIdentityChanged,
            mode,
            anchor_path,
            None,
        )
    })?;
    if opened.identity != path_metadata.identity {
        return Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorIdentityChanged,
            mode,
            anchor_path,
            None,
        ));
    }
    handle.set_owner_only().map_err(|error| {
        ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorPermissionFailed,
            mode,
            anchor_path,
            Some(error),
        )
    })?;
    let secured = handle.metadata().map_err(|error| {
        ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorInspectFailed,
            mode,
            anchor_path,
            Some(error),
        )
    })?;
    if secured.identity != opened.identity {
        return Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorIdentityChanged,
            mode,
            anchor_path,
            None,
        ));
    }
    #[cfg(unix)]
    if secured.mode != ARCHIVE_LEASE_ANCHOR_MODE {
        return Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorModeMismatch,
            mode,
            anchor_path,
            None,
        ));
    }
    Ok(())
}

fn validate_anchor_kind(
    metadata: LeaseAnchorMetadata,
    anchor_path: &Path,
    mode: ArchiveLeaseMode,
) -> Result<(), ArchiveLeaseError> {
    if metadata.is_symlink {
        return Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorSymlinkRejected,
            mode,
            anchor_path,
            None,
        ));
    }
    if !metadata.is_file {
        return Err(ArchiveLeaseError::new(
            ArchiveLeaseErrorCode::AnchorNotFile,
            mode,
            anchor_path,
            None,
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;

    metadata.mode() & 0o777
}

#[cfg(not(unix))]
fn metadata_mode(_metadata: &Metadata) -> u32 {
    ARCHIVE_LEASE_ANCHOR_MODE
}
