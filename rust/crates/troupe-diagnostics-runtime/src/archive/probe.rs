use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const WRITE_PROBE_NAME: &str = ".troupe-write-probe";
const WRITE_PROBE_MODE: u32 = 0o600;
const WRITE_PROBE_PAYLOAD: &[u8] = b"troupe-diagnostics-write-probe-v1\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeMetadata {
    pub kind: NodeKind,
    pub mode: u32,
    pub device: u64,
    pub inode: u64,
}

impl NodeMetadata {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            NodeKind::Symlink
        } else if file_type.is_dir() {
            NodeKind::Directory
        } else {
            NodeKind::Other
        };
        Self {
            kind,
            mode: metadata.mode() & 0o7777,
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    pub const fn same_identity(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

pub trait ArchiveDirectory {
    fn chmod(&mut self, mode: u32) -> io::Result<()>;
    fn fstat(&self) -> io::Result<NodeMetadata>;
}

pub trait ArchiveProbeFile {
    fn write_all(&mut self, value: &[u8]) -> io::Result<()>;
    fn fsync(&self) -> io::Result<()>;
    fn close(self) -> io::Result<()>;
}

pub trait ArchiveFileSystem {
    type Directory: ArchiveDirectory;
    type ProbeFile: ArchiveProbeFile;

    fn path_metadata(&self, path: &Path) -> io::Result<NodeMetadata>;
    fn mkdir(&self, path: &Path, mode: u32) -> io::Result<()>;
    fn open_directory(&self, path: &Path) -> io::Result<Self::Directory>;
    fn create_probe(&self, path: &Path, mode: u32) -> io::Result<Self::ProbeFile>;
    fn unlink(&self, path: &Path) -> io::Result<()>;
    fn remove_directory(&self, path: &Path) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealFileSystem;

pub struct RealDirectory(File);

impl ArchiveDirectory for RealDirectory {
    fn chmod(&mut self, mode: u32) -> io::Result<()> {
        self.0.set_permissions(fs::Permissions::from_mode(mode))
    }

    fn fstat(&self) -> io::Result<NodeMetadata> {
        self.0.metadata().map(|value| NodeMetadata::from_metadata(&value))
    }
}

pub struct RealProbeFile(Option<File>);

impl RealProbeFile {
    fn file(&self) -> io::Result<&File> {
        self.0
            .as_ref()
            .ok_or_else(|| io::Error::other("write probe file is already closed"))
    }

    fn file_mut(&mut self) -> io::Result<&mut File> {
        self.0
            .as_mut()
            .ok_or_else(|| io::Error::other("write probe file is already closed"))
    }
}

impl ArchiveProbeFile for RealProbeFile {
    fn write_all(&mut self, value: &[u8]) -> io::Result<()> {
        Write::write_all(self.file_mut()?, value)
    }

    fn fsync(&self) -> io::Result<()> {
        self.file()?.sync_all()
    }

    fn close(mut self) -> io::Result<()> {
        drop(self.0.take());
        Ok(())
    }
}

impl ArchiveFileSystem for RealFileSystem {
    type Directory = RealDirectory;
    type ProbeFile = RealProbeFile;

    fn path_metadata(&self, path: &Path) -> io::Result<NodeMetadata> {
        fs::symlink_metadata(path).map(|value| NodeMetadata::from_metadata(&value))
    }

    fn mkdir(&self, path: &Path, mode: u32) -> io::Result<()> {
        let mut builder = DirBuilder::new();
        builder.mode(mode).create(path)
    }

    fn open_directory(&self, path: &Path) -> io::Result<Self::Directory> {
        File::open(path).map(RealDirectory)
    }

    fn create_probe(&self, path: &Path, mode: u32) -> io::Result<Self::ProbeFile> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
            .map(|file| RealProbeFile(Some(file)))
    }

    fn unlink(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn remove_directory(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir(path)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeErrorCode {
    Create,
    Write,
    Sync,
    Close,
    Unlink,
}

#[derive(Debug)]
pub struct ProbeError {
    code: ProbeErrorCode,
    path: PathBuf,
    source: io::Error,
}

impl ProbeError {
    fn new(code: ProbeErrorCode, path: &Path, source: io::Error) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            source,
        }
    }

    pub const fn code(&self) -> ProbeErrorCode {
        self.code
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn into_source(self) -> io::Error {
        self.source
    }
}

fn close_and_unlink_after_failure<F: ArchiveFileSystem>(
    filesystem: &F,
    path: &Path,
    file: F::ProbeFile,
) {
    let _ = file.close();
    let _ = filesystem.unlink(path);
}

pub fn verify_directory_is_writable<F: ArchiveFileSystem>(
    filesystem: &F,
    directory: &Path,
) -> Result<(), ProbeError> {
    let path = directory.join(WRITE_PROBE_NAME);
    let mut file = filesystem
        .create_probe(&path, WRITE_PROBE_MODE)
        .map_err(|error| ProbeError::new(ProbeErrorCode::Create, &path, error))?;

    if let Err(error) = file.write_all(WRITE_PROBE_PAYLOAD) {
        close_and_unlink_after_failure(filesystem, &path, file);
        return Err(ProbeError::new(ProbeErrorCode::Write, &path, error));
    }
    if let Err(error) = file.fsync() {
        close_and_unlink_after_failure(filesystem, &path, file);
        return Err(ProbeError::new(ProbeErrorCode::Sync, &path, error));
    }
    if let Err(error) = file.close() {
        let _ = filesystem.unlink(&path);
        return Err(ProbeError::new(ProbeErrorCode::Close, &path, error));
    }
    filesystem
        .unlink(&path)
        .map_err(|error| ProbeError::new(ProbeErrorCode::Unlink, &path, error))
}
