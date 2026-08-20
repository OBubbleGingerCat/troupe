use std::{
    ffi::CString,
    fmt, fs,
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{
        raw::c_long,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            io::{AsRawFd, RawFd},
        },
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use troupe_diagnostics_core::id::CanonicalUuid;

use crate::archive::layout::ArchiveLayout;

use super::{codec::encode_registry_entry, model::RegistryEntry};

pub const REGISTRY_DIRECTORY_MODE: u32 = 0o700;
pub const REGISTRY_FILE_MODE: u32 = 0o600;
const MAX_TEMP_CREATE_ATTEMPTS: u64 = 128;
const RENAME_NOREPLACE: u32 = 1;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_RENAMEAT2: c_long = 316;

static NEXT_TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryNodeKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistryNodeMetadata {
    kind: RegistryNodeKind,
    mode: u32,
    device: u64,
    inode: u64,
}

impl RegistryNodeMetadata {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            RegistryNodeKind::Symlink
        } else if file_type.is_dir() {
            RegistryNodeKind::Directory
        } else if file_type.is_file() {
            RegistryNodeKind::RegularFile
        } else {
            RegistryNodeKind::Other
        };
        Self {
            kind,
            mode: metadata.mode() & 0o7777,
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    pub const fn kind(self) -> RegistryNodeKind {
        self.kind
    }

    pub const fn mode(self) -> u32 {
        self.mode
    }

    pub const fn same_identity(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

pub trait RegistryFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn chmod(&mut self, mode: u32) -> io::Result<()>;
    fn fstat(&self) -> io::Result<RegistryNodeMetadata>;
    fn sync(&self) -> io::Result<()>;
    fn close(self) -> io::Result<()>;
}

pub trait RegistryDirectory {
    type File: RegistryFile;

    fn fstat(&self) -> io::Result<RegistryNodeMetadata>;
    fn chmod(&mut self, mode: u32) -> io::Result<()>;
    fn create_exclusive(&self, name: &str, mode: u32) -> io::Result<Self::File>;
    fn entry_metadata(&self, name: &str) -> io::Result<RegistryNodeMetadata>;
    fn read_entry(&self, name: &str) -> io::Result<Vec<u8>>;
    fn rename_noreplace(&self, source: &str, target: &str) -> io::Result<()>;
    fn unlink(&self, name: &str) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
}

pub trait RegistryFileSystem {
    type Directory: RegistryDirectory;

    fn path_metadata(&self, path: &Path) -> io::Result<RegistryNodeMetadata>;
    fn open_directory(&self, path: &Path) -> io::Result<Self::Directory>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealRegistryFileSystem;

pub struct RealRegistryDirectory(File);

pub struct RealRegistryFile(Option<File>);

impl RegistryFileSystem for RealRegistryFileSystem {
    type Directory = RealRegistryDirectory;

    fn path_metadata(&self, path: &Path) -> io::Result<RegistryNodeMetadata> {
        fs::symlink_metadata(path).map(|metadata| RegistryNodeMetadata::from_metadata(&metadata))
    }

    fn open_directory(&self, path: &Path) -> io::Result<Self::Directory> {
        File::open(path).map(RealRegistryDirectory)
    }
}

impl RealRegistryDirectory {
    fn entry_path(&self, name: &str) -> io::Result<PathBuf> {
        validate_entry_name(name)?;
        Ok(PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd())).join(name))
    }

    fn raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl RegistryDirectory for RealRegistryDirectory {
    type File = RealRegistryFile;

    fn fstat(&self) -> io::Result<RegistryNodeMetadata> {
        self.0
            .metadata()
            .map(|metadata| RegistryNodeMetadata::from_metadata(&metadata))
    }

    fn chmod(&mut self, mode: u32) -> io::Result<()> {
        self.0.set_permissions(fs::Permissions::from_mode(mode))
    }

    fn create_exclusive(&self, name: &str, mode: u32) -> io::Result<Self::File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(self.entry_path(name)?)
            .map(|file| RealRegistryFile(Some(file)))
    }

    fn entry_metadata(&self, name: &str) -> io::Result<RegistryNodeMetadata> {
        fs::symlink_metadata(self.entry_path(name)?)
            .map(|metadata| RegistryNodeMetadata::from_metadata(&metadata))
    }

    fn read_entry(&self, name: &str) -> io::Result<Vec<u8>> {
        fs::read(self.entry_path(name)?)
    }

    fn rename_noreplace(&self, source: &str, target: &str) -> io::Result<()> {
        renameat2_noreplace(self.raw_fd(), source, target)
    }

    fn unlink(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.entry_path(name)?)
    }

    fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}

impl RealRegistryFile {
    fn file(&self) -> io::Result<&File> {
        self.0
            .as_ref()
            .ok_or_else(|| io::Error::other("registry file is already closed"))
    }

    fn file_mut(&mut self) -> io::Result<&mut File> {
        self.0
            .as_mut()
            .ok_or_else(|| io::Error::other("registry file is already closed"))
    }
}

impl RegistryFile for RealRegistryFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        Write::write_all(self.file_mut()?, bytes)
    }

    fn chmod(&mut self, mode: u32) -> io::Result<()> {
        self.file()?
            .set_permissions(fs::Permissions::from_mode(mode))
    }

    fn fstat(&self) -> io::Result<RegistryNodeMetadata> {
        self.file()?
            .metadata()
            .map(|metadata| RegistryNodeMetadata::from_metadata(&metadata))
    }

    fn sync(&self) -> io::Result<()> {
        self.file()?.sync_all()
    }

    fn close(mut self) -> io::Result<()> {
        drop(self.0.take());
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistryPublicationReadiness {
    store_ready: bool,
    listener_ready: bool,
}

impl RegistryPublicationReadiness {
    pub const fn new(store_ready: bool, listener_ready: bool) -> Self {
        Self {
            store_ready,
            listener_ready,
        }
    }

    pub const fn is_ready(self) -> bool {
        self.store_ready && self.listener_ready
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListenerState {
    Running,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl From<RegistryNodeMetadata> for FileIdentity {
    fn from(metadata: RegistryNodeMetadata) -> Self {
        Self {
            device: metadata.device,
            inode: metadata.inode,
        }
    }
}

impl FileIdentity {
    const fn matches(self, metadata: RegistryNodeMetadata) -> bool {
        self.device == metadata.device && self.inode == metadata.inode
    }
}

#[must_use = "a published registry entry must be durably unpublished before listener shutdown"]
#[derive(Debug)]
pub struct RegistryPublication {
    instances_directory: PathBuf,
    locator_name: String,
    locator_path: PathBuf,
    directory_identity: FileIdentity,
    locator_identity: FileIdentity,
    locator_bytes: Vec<u8>,
    published: bool,
}

impl RegistryPublication {
    pub fn locator_path(&self) -> &Path {
        &self.locator_path
    }

    pub const fn is_published(&self) -> bool {
        self.published
    }

    pub fn unpublish(&mut self, listener_state: ListenerState) -> Result<(), RegistryPublishError> {
        self.unpublish_with(&RealRegistryFileSystem, listener_state)
    }

    pub fn unpublish_with<F: RegistryFileSystem>(
        &mut self,
        filesystem: &F,
        listener_state: ListenerState,
    ) -> Result<(), RegistryPublishError> {
        if listener_state != ListenerState::Running {
            return Err(RegistryPublishError::logical(
                RegistryPublishErrorCode::ListenerAlreadyStopped,
                &self.locator_path,
            ));
        }
        if !self.published {
            return Ok(());
        }

        let (directory, directory_metadata) = open_validated_directory(
            filesystem,
            &self.instances_directory,
            Some(self.directory_identity),
        )?;
        if !self.directory_identity.matches(directory_metadata) {
            return Err(RegistryPublishError::logical(
                RegistryPublishErrorCode::DirectoryIdentityChanged,
                &self.instances_directory,
            ));
        }
        let locator_metadata = directory
            .entry_metadata(&self.locator_name)
            .map_err(|error| {
                RegistryPublishError::io(
                    RegistryPublishErrorCode::TargetMetadataFailed,
                    &self.locator_path,
                    error,
                )
            })?;
        if locator_metadata.kind() != RegistryNodeKind::RegularFile
            || !self.locator_identity.matches(locator_metadata)
        {
            return Err(RegistryPublishError::logical(
                RegistryPublishErrorCode::LocatorIdentityChanged,
                &self.locator_path,
            ));
        }
        let locator_bytes = directory.read_entry(&self.locator_name).map_err(|error| {
            RegistryPublishError::io(
                RegistryPublishErrorCode::TargetReadFailed,
                &self.locator_path,
                error,
            )
        })?;
        if locator_bytes != self.locator_bytes {
            return Err(RegistryPublishError::logical(
                RegistryPublishErrorCode::LocatorIdentityChanged,
                &self.locator_path,
            ));
        }
        revalidate_directory_path(
            filesystem,
            &self.instances_directory,
            self.directory_identity,
        )?;
        directory.unlink(&self.locator_name).map_err(|error| {
            RegistryPublishError::io(
                RegistryPublishErrorCode::UnlinkFailed,
                &self.locator_path,
                error,
            )
        })?;
        self.published = false;
        directory.sync().map_err(|error| {
            RegistryPublishError::io(
                RegistryPublishErrorCode::DirectorySyncFailed,
                &self.instances_directory,
                error,
            )
        })?;
        revalidate_directory_path(
            filesystem,
            &self.instances_directory,
            self.directory_identity,
        )?;
        Ok(())
    }

    pub fn rollback_startup(&mut self) -> Result<(), RegistryPublishError> {
        self.unpublish(ListenerState::Running)
    }
}

pub fn publish_registry_entry(
    layout: &ArchiveLayout,
    entry: &RegistryEntry,
    readiness: RegistryPublicationReadiness,
) -> Result<RegistryPublication, RegistryPublishError> {
    publish_registry_entry_with(&RealRegistryFileSystem, layout, entry, readiness)
}

pub fn publish_registry_entry_with<F: RegistryFileSystem>(
    filesystem: &F,
    layout: &ArchiveLayout,
    entry: &RegistryEntry,
    readiness: RegistryPublicationReadiness,
) -> Result<RegistryPublication, RegistryPublishError> {
    if !readiness.is_ready() {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::NotReady,
            layout.instances_directory(),
        ));
    }
    if entry.run_id() != layout.run_id()
        || entry.run_directory().as_path() != layout.run_directory()
    {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::EntryIdentityMismatch,
            layout.instances_directory(),
        ));
    }

    let encoded = encode_registry_entry(entry).map_err(|error| {
        RegistryPublishError::io(
            RegistryPublishErrorCode::EncodeFailed,
            layout.instances_directory(),
            io::Error::new(io::ErrorKind::InvalidData, error),
        )
    })?;
    let (directory, directory_metadata) =
        open_validated_directory(filesystem, layout.instances_directory(), None)?;
    let directory_identity = FileIdentity::from(directory_metadata);
    let locator_name = format!("{}.json", layout.run_id());
    let locator_path = layout.instances_directory().join(&locator_name);
    let (temp_name, mut temp_file) =
        create_temp_file(&directory, layout.run_id(), layout.instances_directory())?;
    let temp_path = layout.instances_directory().join(&temp_name);

    if let Err(error) = temp_file.chmod(REGISTRY_FILE_MODE) {
        cleanup_temp(&directory, &temp_name, temp_file);
        return Err(RegistryPublishError::io(
            RegistryPublishErrorCode::TempPermissionFailed,
            &temp_path,
            error,
        ));
    }
    let initial_metadata = match temp_file.fstat() {
        Ok(metadata) => metadata,
        Err(error) => {
            cleanup_temp(&directory, &temp_name, temp_file);
            return Err(RegistryPublishError::io(
                RegistryPublishErrorCode::TempMetadataFailed,
                &temp_path,
                error,
            ));
        }
    };
    if let Err(error) = validate_temp_metadata(initial_metadata, &temp_path) {
        cleanup_temp(&directory, &temp_name, temp_file);
        return Err(error);
    }
    if let Err(error) = temp_file.write_all(&encoded) {
        cleanup_temp(&directory, &temp_name, temp_file);
        return Err(RegistryPublishError::io(
            RegistryPublishErrorCode::TempWriteFailed,
            &temp_path,
            error,
        ));
    }
    if let Err(error) = temp_file.sync() {
        cleanup_temp(&directory, &temp_name, temp_file);
        return Err(RegistryPublishError::io(
            RegistryPublishErrorCode::TempSyncFailed,
            &temp_path,
            error,
        ));
    }
    let final_metadata = match temp_file.fstat() {
        Ok(metadata) => metadata,
        Err(error) => {
            cleanup_temp(&directory, &temp_name, temp_file);
            return Err(RegistryPublishError::io(
                RegistryPublishErrorCode::TempMetadataFailed,
                &temp_path,
                error,
            ));
        }
    };
    if let Err(error) = validate_temp_metadata(final_metadata, &temp_path) {
        cleanup_temp(&directory, &temp_name, temp_file);
        return Err(error);
    }
    if !initial_metadata.same_identity(final_metadata) {
        cleanup_temp(&directory, &temp_name, temp_file);
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::TempIdentityChanged,
            &temp_path,
        ));
    }
    if let Err(error) = temp_file.close() {
        cleanup_named_entry(&directory, &temp_name);
        return Err(RegistryPublishError::io(
            RegistryPublishErrorCode::TempCloseFailed,
            &temp_path,
            error,
        ));
    }

    revalidate_directory_path(filesystem, layout.instances_directory(), directory_identity)
        .inspect_err(|_| cleanup_named_entry(&directory, &temp_name))?;
    if let Err(error) = directory.rename_noreplace(&temp_name, &locator_name) {
        cleanup_named_entry(&directory, &temp_name);
        let code = if error.kind() == io::ErrorKind::AlreadyExists {
            RegistryPublishErrorCode::TargetAlreadyExists
        } else {
            RegistryPublishErrorCode::RenameFailed
        };
        return Err(RegistryPublishError::io(code, &locator_path, error));
    }

    let target_metadata = match directory.entry_metadata(&locator_name) {
        Ok(metadata) => metadata,
        Err(error) => {
            rollback_published_target(&directory, &locator_name);
            return Err(RegistryPublishError::io(
                RegistryPublishErrorCode::TargetMetadataFailed,
                &locator_path,
                error,
            ));
        }
    };
    if target_metadata.kind() != RegistryNodeKind::RegularFile
        || target_metadata.mode() != REGISTRY_FILE_MODE
        || !initial_metadata.same_identity(target_metadata)
    {
        rollback_published_target(&directory, &locator_name);
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::TargetIdentityChanged,
            &locator_path,
        ));
    }

    if let Err(error) =
        revalidate_directory_path(filesystem, layout.instances_directory(), directory_identity)
    {
        rollback_published_target(&directory, &locator_name);
        return Err(error);
    }
    if let Err(error) = directory.sync() {
        rollback_published_target(&directory, &locator_name);
        return Err(RegistryPublishError::io(
            RegistryPublishErrorCode::DirectorySyncFailed,
            layout.instances_directory(),
            error,
        ));
    }
    if let Err(error) =
        revalidate_directory_path(filesystem, layout.instances_directory(), directory_identity)
    {
        rollback_published_target(&directory, &locator_name);
        return Err(error);
    }

    Ok(RegistryPublication {
        instances_directory: layout.instances_directory().to_path_buf(),
        locator_name,
        locator_path,
        directory_identity,
        locator_identity: FileIdentity::from(target_metadata),
        locator_bytes: encoded,
        published: true,
    })
}

fn create_temp_file<D: RegistryDirectory>(
    directory: &D,
    run_id: CanonicalUuid,
    directory_path: &Path,
) -> Result<(String, D::File), RegistryPublishError> {
    for _ in 0..MAX_TEMP_CREATE_ATTEMPTS {
        let nonce = NEXT_TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(".{run_id}.{}.{}.tmp", std::process::id(), nonce);
        match directory.create_exclusive(&name, REGISTRY_FILE_MODE) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(RegistryPublishError::io(
                    RegistryPublishErrorCode::TempCreateFailed,
                    &directory_path.join(name),
                    error,
                ));
            }
        }
    }
    Err(RegistryPublishError::logical(
        RegistryPublishErrorCode::TempCreateFailed,
        directory_path,
    ))
}

fn cleanup_temp<D: RegistryDirectory>(directory: &D, name: &str, file: D::File) {
    let _ = file.close();
    cleanup_named_entry(directory, name);
}

fn cleanup_named_entry<D: RegistryDirectory>(directory: &D, name: &str) {
    let _ = directory.unlink(name);
    let _ = directory.sync();
}

fn rollback_published_target<D: RegistryDirectory>(directory: &D, name: &str) {
    cleanup_named_entry(directory, name);
}

fn validate_temp_metadata(
    metadata: RegistryNodeMetadata,
    path: &Path,
) -> Result<(), RegistryPublishError> {
    if metadata.kind() != RegistryNodeKind::RegularFile {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::TempNotRegular,
            path,
        ));
    }
    if metadata.mode() != REGISTRY_FILE_MODE {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::TempModeMismatch,
            path,
        ));
    }
    Ok(())
}

fn open_validated_directory<F: RegistryFileSystem>(
    filesystem: &F,
    path: &Path,
    expected_identity: Option<FileIdentity>,
) -> Result<(F::Directory, RegistryNodeMetadata), RegistryPublishError> {
    let path_metadata = filesystem.path_metadata(path).map_err(|error| {
        RegistryPublishError::io(
            RegistryPublishErrorCode::DirectoryInspectFailed,
            path,
            error,
        )
    })?;
    validate_directory_kind(path, path_metadata)?;
    if expected_identity.is_some_and(|identity| !identity.matches(path_metadata)) {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::DirectoryIdentityChanged,
            path,
        ));
    }

    let mut directory = filesystem.open_directory(path).map_err(|error| {
        RegistryPublishError::io(
            RegistryPublishErrorCode::DirectoryInspectFailed,
            path,
            error,
        )
    })?;
    let opened_metadata = directory.fstat().map_err(|error| {
        RegistryPublishError::io(
            RegistryPublishErrorCode::DirectoryInspectFailed,
            path,
            error,
        )
    })?;
    validate_directory_kind(path, opened_metadata)?;
    if !path_metadata.same_identity(opened_metadata)
        || expected_identity.is_some_and(|identity| !identity.matches(opened_metadata))
    {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::DirectoryIdentityChanged,
            path,
        ));
    }

    directory.chmod(REGISTRY_DIRECTORY_MODE).map_err(|error| {
        RegistryPublishError::io(
            RegistryPublishErrorCode::DirectoryPermissionFailed,
            path,
            error,
        )
    })?;
    let secured_metadata = directory.fstat().map_err(|error| {
        RegistryPublishError::io(
            RegistryPublishErrorCode::DirectoryInspectFailed,
            path,
            error,
        )
    })?;
    validate_directory_kind(path, secured_metadata)?;
    if !opened_metadata.same_identity(secured_metadata) {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::DirectoryIdentityChanged,
            path,
        ));
    }
    if secured_metadata.mode() != REGISTRY_DIRECTORY_MODE {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::DirectoryModeMismatch,
            path,
        ));
    }
    revalidate_directory_path(filesystem, path, FileIdentity::from(secured_metadata))?;
    Ok((directory, secured_metadata))
}

fn revalidate_directory_path<F: RegistryFileSystem>(
    filesystem: &F,
    path: &Path,
    expected_identity: FileIdentity,
) -> Result<(), RegistryPublishError> {
    let metadata = filesystem.path_metadata(path).map_err(|error| {
        RegistryPublishError::io(
            RegistryPublishErrorCode::DirectoryInspectFailed,
            path,
            error,
        )
    })?;
    validate_directory_kind(path, metadata)?;
    if !expected_identity.matches(metadata) {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::DirectoryIdentityChanged,
            path,
        ));
    }
    if metadata.mode() != REGISTRY_DIRECTORY_MODE {
        return Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::DirectoryModeMismatch,
            path,
        ));
    }
    Ok(())
}

fn validate_directory_kind(
    path: &Path,
    metadata: RegistryNodeMetadata,
) -> Result<(), RegistryPublishError> {
    match metadata.kind() {
        RegistryNodeKind::Directory => Ok(()),
        RegistryNodeKind::Symlink => Err(RegistryPublishError::logical(
            RegistryPublishErrorCode::DirectorySymlinkRejected,
            path,
        )),
        RegistryNodeKind::RegularFile | RegistryNodeKind::Other => Err(
            RegistryPublishError::logical(RegistryPublishErrorCode::DirectoryNotDirectory, path),
        ),
    }
}

fn validate_entry_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.as_bytes().contains(&b'/')
        || name.as_bytes().contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "registry entry name is invalid",
        ));
    }
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn renameat2_noreplace(directory_fd: RawFd, source: &str, target: &str) -> io::Result<()> {
    validate_entry_name(source)?;
    validate_entry_name(target)?;
    let source = CString::new(source.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source contains NUL"))?;
    let target = CString::new(target.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target contains NUL"))?;
    // SAFETY: both names are validated NUL-terminated strings and both directory FDs are live.
    let result = unsafe {
        syscall(
            SYS_RENAMEAT2,
            directory_fd,
            source.as_ptr(),
            directory_fd,
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
fn renameat2_noreplace(_directory_fd: RawFd, _source: &str, _target: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unsupported on this platform",
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryPublishErrorCode {
    NotReady,
    EntryIdentityMismatch,
    EncodeFailed,
    DirectoryInspectFailed,
    DirectorySymlinkRejected,
    DirectoryNotDirectory,
    DirectoryIdentityChanged,
    DirectoryPermissionFailed,
    DirectoryModeMismatch,
    TempCreateFailed,
    TempPermissionFailed,
    TempMetadataFailed,
    TempNotRegular,
    TempModeMismatch,
    TempIdentityChanged,
    TempWriteFailed,
    TempSyncFailed,
    TempCloseFailed,
    TargetAlreadyExists,
    RenameFailed,
    TargetMetadataFailed,
    TargetReadFailed,
    TargetIdentityChanged,
    DirectorySyncFailed,
    ListenerAlreadyStopped,
    LocatorIdentityChanged,
    UnlinkFailed,
}

impl RegistryPublishErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotReady => "registry.not_ready",
            Self::EntryIdentityMismatch => "registry.entry_identity_mismatch",
            Self::EncodeFailed => "registry.encode_failed",
            Self::DirectoryInspectFailed => "registry.directory_inspect_failed",
            Self::DirectorySymlinkRejected => "registry.directory_symlink_rejected",
            Self::DirectoryNotDirectory => "registry.directory_not_directory",
            Self::DirectoryIdentityChanged => "registry.directory_identity_changed",
            Self::DirectoryPermissionFailed => "registry.directory_permission_failed",
            Self::DirectoryModeMismatch => "registry.directory_mode_mismatch",
            Self::TempCreateFailed => "registry.temp_create_failed",
            Self::TempPermissionFailed => "registry.temp_permission_failed",
            Self::TempMetadataFailed => "registry.temp_metadata_failed",
            Self::TempNotRegular => "registry.temp_not_regular",
            Self::TempModeMismatch => "registry.temp_mode_mismatch",
            Self::TempIdentityChanged => "registry.temp_identity_changed",
            Self::TempWriteFailed => "registry.temp_write_failed",
            Self::TempSyncFailed => "registry.temp_sync_failed",
            Self::TempCloseFailed => "registry.temp_close_failed",
            Self::TargetAlreadyExists => "registry.target_already_exists",
            Self::RenameFailed => "registry.rename_failed",
            Self::TargetMetadataFailed => "registry.target_metadata_failed",
            Self::TargetReadFailed => "registry.target_read_failed",
            Self::TargetIdentityChanged => "registry.target_identity_changed",
            Self::DirectorySyncFailed => "registry.directory_sync_failed",
            Self::ListenerAlreadyStopped => "registry.listener_already_stopped",
            Self::LocatorIdentityChanged => "registry.locator_identity_changed",
            Self::UnlinkFailed => "registry.unlink_failed",
        }
    }
}

#[derive(Debug)]
pub struct RegistryPublishError {
    code: RegistryPublishErrorCode,
    path: PathBuf,
    source: Option<io::Error>,
}

impl RegistryPublishError {
    fn logical(code: RegistryPublishErrorCode, path: &Path) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            source: None,
        }
    }

    fn io(code: RegistryPublishErrorCode, path: &Path, source: io::Error) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            source: Some(source),
        }
    }

    pub const fn code(&self) -> RegistryPublishErrorCode {
        self.code
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source_kind(&self) -> Option<io::ErrorKind> {
        self.source.as_ref().map(io::Error::kind)
    }
}

impl fmt::Display for RegistryPublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic registry operation failed [{}] at {}",
            self.code.as_str(),
            self.path.display()
        )
    }
}

impl std::error::Error for RegistryPublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}
