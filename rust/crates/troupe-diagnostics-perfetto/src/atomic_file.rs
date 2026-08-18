use std::{
    ffi::{CString, OsStr, OsString},
    fmt,
    fs::{self, File, OpenOptions},
    future::{Future, poll_fn},
    io::{self, Write},
    os::{
        raw::{c_int, c_long},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
            io::{AsRawFd, IntoRawFd, RawFd},
        },
    },
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
};

use tokio::io::{AsyncWrite, AsyncWriteExt};

const TRACE_FILE_MODE: u32 = 0o600;
const MAX_EXCLUSIVE_NAME_ATTEMPTS: u64 = 128;
const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_RENAMEAT2: c_long = 316;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const SYS_RENAMEAT2: c_long = 276;

static NEXT_NAME_NONCE: AtomicU64 = AtomicU64::new(0);

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
))]
unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
}

unsafe extern "C" {
    fn close(file_descriptor: c_int) -> c_int;
}

pub type TraceProducerFuture<'operation, Summary, Error> =
    Pin<Box<dyn Future<Output = Result<Summary, Error>> + 'operation>>;

/// A trace producer that only streams bytes into the supplied writer.
pub trait TraceStreamProducer {
    type Summary;
    type Error;

    fn produce<'operation>(
        &'operation mut self,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
    ) -> TraceProducerFuture<'operation, Self::Summary, Self::Error>;
}

#[derive(Clone, Default)]
pub struct PublicationCancellation {
    inner: Arc<CancellationInner>,
}

#[derive(Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl PublicationCancellation {
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        if let Some(waker) = lock(&self.inner.waker).take() {
            waker.wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    fn register(&self, waker: &Waker) {
        *lock(&self.inner.waker) = Some(waker.clone());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationState {
    Published,
    NotPublished,
    PublicationIndeterminate,
}

impl PublicationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::NotPublished => "not_published",
            Self::PublicationIndeterminate => "publication_indeterminate",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationPhase {
    ValidateOutput,
    OpenParent,
    InspectTarget,
    CreateTemp,
    SyncTempCreation,
    Encode,
    FlushTemp,
    SyncTempFile,
    CloseTemp,
    VerifyTemp,
    LinkBackup,
    SyncBackup,
    VerifyPrecommit,
    PublishRename,
    SyncPublication,
    VerifyPublication,
    CleanupTemp,
    SyncTempCleanup,
    CleanupBackup,
    SyncBackupCleanup,
    RollbackVerify,
    RollbackRename,
    SyncRollback,
    VerifyFinalState,
    Complete,
}

impl PublicationPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ValidateOutput => "validate_output",
            Self::OpenParent => "open_parent",
            Self::InspectTarget => "inspect_target",
            Self::CreateTemp => "create_temp",
            Self::SyncTempCreation => "sync_temp_creation",
            Self::Encode => "encode",
            Self::FlushTemp => "flush_temp",
            Self::SyncTempFile => "sync_temp_file",
            Self::CloseTemp => "close_temp",
            Self::VerifyTemp => "verify_temp",
            Self::LinkBackup => "link_backup",
            Self::SyncBackup => "sync_backup",
            Self::VerifyPrecommit => "verify_precommit",
            Self::PublishRename => "publish_rename",
            Self::SyncPublication => "sync_publication",
            Self::VerifyPublication => "verify_publication",
            Self::CleanupTemp => "cleanup_temp",
            Self::SyncTempCleanup => "sync_temp_cleanup",
            Self::CleanupBackup => "cleanup_backup",
            Self::SyncBackupCleanup => "sync_backup_cleanup",
            Self::RollbackVerify => "rollback_verify",
            Self::RollbackRename => "rollback_rename",
            Self::SyncRollback => "sync_rollback",
            Self::VerifyFinalState => "verify_final_state",
            Self::Complete => "complete",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    pub const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    pub const fn device(self) -> u64 {
        self.device
    }

    pub const fn inode(self) -> u64 {
        self.inode
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeMetadata {
    kind: NodeKind,
    identity: FileIdentity,
}

impl NodeMetadata {
    pub const fn new(kind: NodeKind, identity: FileIdentity) -> Self {
        Self { kind, identity }
    }

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
        Self {
            kind,
            identity: FileIdentity::new(metadata.dev(), metadata.ino()),
        }
    }

    pub const fn kind(self) -> NodeKind {
        self.kind
    }

    pub const fn identity(self) -> FileIdentity {
        self.identity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeObservation {
    Absent,
    Present(NodeMetadata),
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationPaths {
    target: PathBuf,
    temp: Option<PathBuf>,
    backup: Option<PathBuf>,
}

impl PublicationPaths {
    pub fn target(&self) -> &Path {
        &self.target
    }

    pub fn temp(&self) -> Option<&Path> {
        self.temp.as_deref()
    }

    pub fn backup(&self) -> Option<&Path> {
        self.backup.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationObservations {
    target: NodeObservation,
    temp: NodeObservation,
    backup: NodeObservation,
}

impl PublicationObservations {
    pub const fn target(self) -> NodeObservation {
        self.target
    }

    pub const fn temp(self) -> NodeObservation {
        self.temp
    }

    pub const fn backup(self) -> NodeObservation {
        self.backup
    }
}

#[derive(Debug)]
pub enum PublicationFailure<ProducerError> {
    InvalidOutputPath(&'static str),
    TargetAlreadyExists(NodeMetadata),
    TargetTypeRejected(NodeKind),
    TemporaryTypeRejected(NodeKind),
    Cancelled {
        phase: PublicationPhase,
    },
    Producer(ProducerError),
    Io {
        phase: PublicationPhase,
        error: io::Error,
    },
    IdentityChanged {
        phase: PublicationPhase,
        expected: NodeObservation,
        observed: NodeObservation,
    },
}

impl<ProducerError> PublicationFailure<ProducerError> {
    pub const fn phase(&self) -> PublicationPhase {
        match self {
            Self::InvalidOutputPath(_) => PublicationPhase::ValidateOutput,
            Self::TargetAlreadyExists(_) | Self::TargetTypeRejected(_) => {
                PublicationPhase::InspectTarget
            }
            Self::TemporaryTypeRejected(_) => PublicationPhase::CreateTemp,
            Self::Cancelled { phase } => *phase,
            Self::Producer(_) => PublicationPhase::Encode,
            Self::Io { phase, .. } | Self::IdentityChanged { phase, .. } => *phase,
        }
    }
}

impl<ProducerError: fmt::Display> fmt::Display for PublicationFailure<ProducerError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOutputPath(detail) => {
                write!(formatter, "invalid trace output path: {detail}")
            }
            Self::TargetAlreadyExists(_) => formatter.write_str("trace output already exists"),
            Self::TargetTypeRejected(kind) => {
                write!(formatter, "trace output has rejected node type: {kind:?}")
            }
            Self::TemporaryTypeRejected(kind) => {
                write!(
                    formatter,
                    "trace temporary has rejected node type: {kind:?}"
                )
            }
            Self::Cancelled { phase } => write!(
                formatter,
                "trace publication was cancelled at {}",
                phase.as_str()
            ),
            Self::Producer(error) => write!(formatter, "trace producer failed: {error}"),
            Self::Io { phase, error } => {
                write!(
                    formatter,
                    "trace publication failed at {}: {error}",
                    phase.as_str()
                )
            }
            Self::IdentityChanged {
                phase,
                expected,
                observed,
            } => write!(
                formatter,
                "trace publication identity changed at {}: expected {expected:?}, observed {observed:?}",
                phase.as_str()
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationUncertainty {
    phase: PublicationPhase,
    detail: String,
}

impl PublicationUncertainty {
    fn new(phase: PublicationPhase, detail: impl Into<String>) -> Self {
        Self {
            phase,
            detail: detail.into(),
        }
    }

    pub const fn phase(&self) -> PublicationPhase {
        self.phase
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Debug)]
pub struct PublicationReport<Summary, ProducerError> {
    state: PublicationState,
    phase: PublicationPhase,
    paths: PublicationPaths,
    observations: PublicationObservations,
    summary: Option<Summary>,
    failure: Option<PublicationFailure<ProducerError>>,
    uncertainty: Option<PublicationUncertainty>,
}

impl<Summary, ProducerError> PublicationReport<Summary, ProducerError> {
    pub const fn state(&self) -> PublicationState {
        self.state
    }

    pub const fn phase(&self) -> PublicationPhase {
        self.phase
    }

    pub const fn observations(&self) -> PublicationObservations {
        self.observations
    }

    pub fn paths(&self) -> &PublicationPaths {
        &self.paths
    }

    pub fn summary(&self) -> Option<&Summary> {
        self.summary.as_ref()
    }

    pub fn failure(&self) -> Option<&PublicationFailure<ProducerError>> {
        self.failure.as_ref()
    }

    pub fn uncertainty(&self) -> Option<&PublicationUncertainty> {
        self.uncertainty.as_ref()
    }
}

pub trait AtomicTraceFile: AsyncWrite + Unpin {
    fn metadata(&self) -> io::Result<NodeMetadata>;
    fn sync_all(&mut self) -> io::Result<()>;
    fn close(self) -> io::Result<()>;
}

pub trait AtomicTraceDirectory {
    type File: AtomicTraceFile;

    fn metadata(&self) -> io::Result<NodeMetadata>;
    fn create_exclusive(&self, name: &OsStr, mode: u32) -> io::Result<Self::File>;
    fn entry_metadata(&self, name: &OsStr) -> io::Result<Option<NodeMetadata>>;
    fn hard_link_exclusive(&self, source: &OsStr, target: &OsStr) -> io::Result<()>;
    fn rename_noreplace(&self, source: &OsStr, target: &OsStr) -> io::Result<()>;
    fn exchange(&self, first: &OsStr, second: &OsStr) -> io::Result<()>;
    fn unlink(&self, name: &OsStr) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
}

pub trait AtomicTraceFileSystem {
    type Directory: AtomicTraceDirectory;

    fn path_metadata(&self, path: &Path) -> io::Result<NodeMetadata>;
    fn open_directory(&self, path: &Path) -> io::Result<Self::Directory>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealAtomicTraceFileSystem;

pub struct RealAtomicTraceDirectory(File);

pub struct RealAtomicTraceFile(Option<File>);

impl AtomicTraceFileSystem for RealAtomicTraceFileSystem {
    type Directory = RealAtomicTraceDirectory;

    fn path_metadata(&self, path: &Path) -> io::Result<NodeMetadata> {
        fs::symlink_metadata(path).map(|metadata| NodeMetadata::from_metadata(&metadata))
    }

    fn open_directory(&self, path: &Path) -> io::Result<Self::Directory> {
        File::open(path).map(RealAtomicTraceDirectory)
    }
}

impl RealAtomicTraceDirectory {
    fn raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    fn entry_path(&self, name: &OsStr) -> io::Result<PathBuf> {
        validate_entry_name(name)?;
        Ok(PathBuf::from(format!("/proc/self/fd/{}", self.raw_fd())).join(name))
    }
}

impl AtomicTraceDirectory for RealAtomicTraceDirectory {
    type File = RealAtomicTraceFile;

    fn metadata(&self) -> io::Result<NodeMetadata> {
        self.0
            .metadata()
            .map(|metadata| NodeMetadata::from_metadata(&metadata))
    }

    fn create_exclusive(&self, name: &OsStr, mode: u32) -> io::Result<Self::File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(self.entry_path(name)?)
            .map(|file| RealAtomicTraceFile(Some(file)))
    }

    fn entry_metadata(&self, name: &OsStr) -> io::Result<Option<NodeMetadata>> {
        match fs::symlink_metadata(self.entry_path(name)?) {
            Ok(metadata) => Ok(Some(NodeMetadata::from_metadata(&metadata))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn hard_link_exclusive(&self, source: &OsStr, target: &OsStr) -> io::Result<()> {
        fs::hard_link(self.entry_path(source)?, self.entry_path(target)?)
    }

    fn rename_noreplace(&self, source: &OsStr, target: &OsStr) -> io::Result<()> {
        renameat2(self.raw_fd(), source, target, RENAME_NOREPLACE)
    }

    fn exchange(&self, first: &OsStr, second: &OsStr) -> io::Result<()> {
        renameat2(self.raw_fd(), first, second, RENAME_EXCHANGE)
    }

    fn unlink(&self, name: &OsStr) -> io::Result<()> {
        fs::remove_file(self.entry_path(name)?)
    }

    fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}

impl RealAtomicTraceFile {
    fn file(&self) -> io::Result<&File> {
        self.0
            .as_ref()
            .ok_or_else(|| io::Error::other("trace temporary file is already closed"))
    }

    fn file_mut(&mut self) -> io::Result<&mut File> {
        self.0
            .as_mut()
            .ok_or_else(|| io::Error::other("trace temporary file is already closed"))
    }
}

impl AsyncWrite for RealAtomicTraceFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.file_mut().and_then(|file| file.write(bytes)))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.file_mut().and_then(Write::flush))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.file_mut().and_then(Write::flush))
    }
}

impl AtomicTraceFile for RealAtomicTraceFile {
    fn metadata(&self) -> io::Result<NodeMetadata> {
        self.file()?
            .metadata()
            .map(|metadata| NodeMetadata::from_metadata(&metadata))
    }

    fn sync_all(&mut self) -> io::Result<()> {
        self.file()?.sync_all()
    }

    fn close(mut self) -> io::Result<()> {
        let file = self
            .0
            .take()
            .ok_or_else(|| io::Error::other("trace temporary file is already closed"))?;
        let file_descriptor = file.into_raw_fd();
        // SAFETY: ownership of the live descriptor was transferred out of File exactly once.
        if unsafe { close(file_descriptor) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

pub async fn publish_atomic_trace<Producer>(
    output: &Path,
    force: bool,
    cancellation: &PublicationCancellation,
    producer: &mut Producer,
) -> PublicationReport<Producer::Summary, Producer::Error>
where
    Producer: TraceStreamProducer,
{
    publish_atomic_trace_with(
        &RealAtomicTraceFileSystem,
        output,
        force,
        cancellation,
        producer,
    )
    .await
}

pub async fn publish_atomic_trace_with<FileSystem, Producer>(
    filesystem: &FileSystem,
    output: &Path,
    force: bool,
    cancellation: &PublicationCancellation,
    producer: &mut Producer,
) -> PublicationReport<Producer::Summary, Producer::Error>
where
    FileSystem: AtomicTraceFileSystem,
    Producer: TraceStreamProducer,
{
    let location = match OutputLocation::parse(output) {
        Ok(location) => location,
        Err(detail) => {
            return early_report(
                output,
                PublicationState::NotPublished,
                PublicationPhase::ValidateOutput,
                PublicationFailure::InvalidOutputPath(detail),
            );
        }
    };

    let path_metadata = match filesystem.path_metadata(&location.parent) {
        Ok(metadata) => metadata,
        Err(error) => {
            return early_report(
                output,
                PublicationState::NotPublished,
                PublicationPhase::OpenParent,
                PublicationFailure::Io {
                    phase: PublicationPhase::OpenParent,
                    error,
                },
            );
        }
    };
    if path_metadata.kind() != NodeKind::Directory {
        return early_report(
            output,
            PublicationState::NotPublished,
            PublicationPhase::OpenParent,
            PublicationFailure::InvalidOutputPath(match path_metadata.kind() {
                NodeKind::Symlink => "output parent must not be a symlink",
                _ => "output parent is not a directory",
            }),
        );
    }
    let directory = match filesystem.open_directory(&location.parent) {
        Ok(directory) => directory,
        Err(error) => {
            return early_report(
                output,
                PublicationState::NotPublished,
                PublicationPhase::OpenParent,
                PublicationFailure::Io {
                    phase: PublicationPhase::OpenParent,
                    error,
                },
            );
        }
    };
    let opened_metadata = match directory.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            return early_report(
                output,
                PublicationState::NotPublished,
                PublicationPhase::OpenParent,
                PublicationFailure::Io {
                    phase: PublicationPhase::OpenParent,
                    error,
                },
            );
        }
    };
    if opened_metadata.kind() != NodeKind::Directory
        || opened_metadata.identity() != path_metadata.identity()
    {
        return early_report(
            output,
            PublicationState::NotPublished,
            PublicationPhase::OpenParent,
            PublicationFailure::IdentityChanged {
                phase: PublicationPhase::OpenParent,
                expected: NodeObservation::Present(path_metadata),
                observed: NodeObservation::Present(opened_metadata),
            },
        );
    }

    let initial_target = match directory.entry_metadata(&location.target_name) {
        Ok(None) => NodeObservation::Absent,
        Ok(Some(metadata)) if metadata.kind() == NodeKind::RegularFile => {
            if !force {
                return direct_report(
                    &location,
                    &directory,
                    PublicationState::NotPublished,
                    PublicationPhase::InspectTarget,
                    None,
                    PublicationFailure::TargetAlreadyExists(metadata),
                    None,
                    None,
                );
            }
            NodeObservation::Present(metadata)
        }
        Ok(Some(metadata)) => {
            return direct_report(
                &location,
                &directory,
                PublicationState::NotPublished,
                PublicationPhase::InspectTarget,
                None,
                PublicationFailure::TargetTypeRejected(metadata.kind()),
                None,
                None,
            );
        }
        Err(error) => {
            return direct_report(
                &location,
                &directory,
                PublicationState::NotPublished,
                PublicationPhase::InspectTarget,
                None,
                PublicationFailure::Io {
                    phase: PublicationPhase::InspectTarget,
                    error,
                },
                None,
                None,
            );
        }
    };

    let mut transaction = Transaction::new(
        filesystem,
        directory,
        location,
        path_metadata.identity(),
        initial_target,
    );
    if cancellation.is_cancelled() {
        return transaction.finish_precommit(
            None,
            PublicationFailure::Cancelled {
                phase: PublicationPhase::CreateTemp,
            },
        );
    }

    let (temp_name, mut temp_file) = match transaction.create_temp() {
        Ok(value) => value,
        Err(error) => {
            return transaction.finish_precommit(
                None,
                PublicationFailure::Io {
                    phase: PublicationPhase::CreateTemp,
                    error,
                },
            );
        }
    };
    let temp_metadata = match temp_file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            transaction.track_temp(temp_name, None);
            let _ = temp_file.close();
            return transaction.finish_precommit(
                None,
                PublicationFailure::Io {
                    phase: PublicationPhase::CreateTemp,
                    error,
                },
            );
        }
    };
    transaction.track_temp(temp_name, Some(temp_metadata.identity()));
    if temp_metadata.kind() != NodeKind::RegularFile {
        let _ = temp_file.close();
        return transaction.finish_precommit(
            None,
            PublicationFailure::TemporaryTypeRejected(temp_metadata.kind()),
        );
    }
    if let Err(error) = transaction.directory.sync() {
        let _ = temp_file.close();
        return transaction.finish_precommit(
            None,
            PublicationFailure::Io {
                phase: PublicationPhase::SyncTempCreation,
                error,
            },
        );
    }
    if cancellation.is_cancelled() {
        let _ = temp_file.close();
        return transaction.finish_precommit(
            None,
            PublicationFailure::Cancelled {
                phase: PublicationPhase::Encode,
            },
        );
    }

    let summary = match produce_or_cancel(cancellation, producer, &mut temp_file).await {
        ProduceResult::Completed(Ok(summary)) => Some(summary),
        ProduceResult::Completed(Err(error)) => {
            let _ = temp_file.close();
            return transaction.finish_precommit(None, PublicationFailure::Producer(error));
        }
        ProduceResult::Cancelled => {
            let _ = temp_file.close();
            return transaction.finish_precommit(
                None,
                PublicationFailure::Cancelled {
                    phase: PublicationPhase::Encode,
                },
            );
        }
    };
    if let Err(error) = temp_file.flush().await {
        let _ = temp_file.close();
        return transaction.finish_precommit(
            summary,
            PublicationFailure::Io {
                phase: PublicationPhase::FlushTemp,
                error,
            },
        );
    }
    if cancellation.is_cancelled() {
        let _ = temp_file.close();
        return transaction.finish_precommit(
            summary,
            PublicationFailure::Cancelled {
                phase: PublicationPhase::SyncTempFile,
            },
        );
    }
    if let Err(error) = temp_file.sync_all() {
        let _ = temp_file.close();
        return transaction.finish_precommit(
            summary,
            PublicationFailure::Io {
                phase: PublicationPhase::SyncTempFile,
                error,
            },
        );
    }
    if let Err(error) = temp_file.close() {
        return transaction.finish_precommit(
            summary,
            PublicationFailure::Io {
                phase: PublicationPhase::CloseTemp,
                error,
            },
        );
    }

    if let Err(failure) = transaction.verify_temp() {
        return transaction.finish_precommit(summary, failure);
    }
    if cancellation.is_cancelled() {
        return transaction.finish_precommit(
            summary,
            PublicationFailure::Cancelled {
                phase: PublicationPhase::LinkBackup,
            },
        );
    }
    if let NodeObservation::Present(original) = transaction.initial_target {
        if let Err(failure) = transaction.create_backup(original.identity()) {
            return transaction.finish_precommit(summary, failure);
        }
        if cancellation.is_cancelled() {
            return transaction.finish_precommit(
                summary,
                PublicationFailure::Cancelled {
                    phase: PublicationPhase::VerifyPrecommit,
                },
            );
        }
    }
    if let Err(failure) = transaction.verify_precommit() {
        return transaction.finish_precommit(summary, failure);
    }
    if cancellation.is_cancelled() {
        return transaction.finish_precommit(
            summary,
            PublicationFailure::Cancelled {
                phase: PublicationPhase::PublishRename,
            },
        );
    }

    match transaction.initial_target {
        NodeObservation::Absent => transaction.publish_absent(summary, cancellation),
        NodeObservation::Present(original) => {
            transaction.publish_force(summary, cancellation, original.identity())
        }
        NodeObservation::Unknown => unreachable!("initial target is resolved before mutation"),
    }
}

enum ProduceResult<Summary, Error> {
    Completed(Result<Summary, Error>),
    Cancelled,
}

async fn produce_or_cancel<Producer, Writer>(
    cancellation: &PublicationCancellation,
    producer: &mut Producer,
    writer: &mut Writer,
) -> ProduceResult<Producer::Summary, Producer::Error>
where
    Producer: TraceStreamProducer,
    Writer: AsyncWrite + Unpin,
{
    let mut future = producer.produce(writer);
    poll_fn(|context| {
        cancellation.register(context.waker());
        if cancellation.is_cancelled() {
            return Poll::Ready(ProduceResult::Cancelled);
        }
        match future.as_mut().poll(context) {
            Poll::Ready(result) => Poll::Ready(ProduceResult::Completed(result)),
            Poll::Pending => {
                if cancellation.is_cancelled() {
                    Poll::Ready(ProduceResult::Cancelled)
                } else {
                    Poll::Pending
                }
            }
        }
    })
    .await
}

struct OutputLocation {
    output: PathBuf,
    parent: PathBuf,
    target_name: OsString,
}

impl OutputLocation {
    fn parse(output: &Path) -> Result<Self, &'static str> {
        if output.as_os_str().is_empty() || output == Path::new("-") {
            return Err("output must be a filesystem path and cannot be stdout");
        }
        if output.as_os_str().as_bytes().ends_with(b"/") {
            return Err("output must name a file, not a directory path");
        }
        for component in output.components() {
            match component {
                Component::ParentDir => return Err("parent traversal is not allowed"),
                Component::Prefix(_) => return Err("cross-platform path prefixes are not allowed"),
                Component::RootDir | Component::CurDir | Component::Normal(_) => {}
            }
        }
        if output.as_os_str().as_bytes().contains(&0) {
            return Err("output contains NUL");
        }
        let target_name = output
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or("output has no file name")?
            .to_os_string();
        validate_entry_name(&target_name).map_err(|_| "output file name is invalid")?;
        let parent = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Ok(Self {
            output: output.to_path_buf(),
            parent,
            target_name,
        })
    }
}

#[derive(Clone)]
struct TrackedEntry {
    name: OsString,
    identity: Option<FileIdentity>,
    expected_present: bool,
}

struct Transaction<'filesystem, FileSystem>
where
    FileSystem: AtomicTraceFileSystem,
{
    filesystem: &'filesystem FileSystem,
    directory: FileSystem::Directory,
    location: OutputLocation,
    parent_identity: FileIdentity,
    initial_target: NodeObservation,
    temp: Option<TrackedEntry>,
    backup: Option<TrackedEntry>,
}

impl<'filesystem, FileSystem> Transaction<'filesystem, FileSystem>
where
    FileSystem: AtomicTraceFileSystem,
{
    fn new(
        filesystem: &'filesystem FileSystem,
        directory: FileSystem::Directory,
        location: OutputLocation,
        parent_identity: FileIdentity,
        initial_target: NodeObservation,
    ) -> Self {
        Self {
            filesystem,
            directory,
            location,
            parent_identity,
            initial_target,
            temp: None,
            backup: None,
        }
    }

    fn create_temp(
        &self,
    ) -> io::Result<(
        OsString,
        <FileSystem::Directory as AtomicTraceDirectory>::File,
    )> {
        for _ in 0..MAX_EXCLUSIVE_NAME_ATTEMPTS {
            let name = unique_name("tmp");
            match self.directory.create_exclusive(&name, TRACE_FILE_MODE) {
                Ok(file) => return Ok((name, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve an exclusive trace temporary name",
        ))
    }

    fn track_temp(&mut self, name: OsString, identity: Option<FileIdentity>) {
        self.temp = Some(TrackedEntry {
            name,
            identity,
            expected_present: true,
        });
    }

    fn verify_temp<ProducerError>(&self) -> Result<(), PublicationFailure<ProducerError>> {
        let temp = self
            .temp
            .as_ref()
            .expect("a prepared transaction has a temp");
        let expected = temp
            .identity
            .map(|identity| {
                NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, identity))
            })
            .unwrap_or(NodeObservation::Unknown);
        let observed = observe_entry(&self.directory, &temp.name);
        if observed == expected {
            Ok(())
        } else {
            Err(PublicationFailure::IdentityChanged {
                phase: PublicationPhase::VerifyTemp,
                expected,
                observed,
            })
        }
    }

    fn create_backup<ProducerError>(
        &mut self,
        original_identity: FileIdentity,
    ) -> Result<(), PublicationFailure<ProducerError>> {
        self.verify_target(PublicationPhase::LinkBackup)?;
        let mut backup_name = None;
        for _ in 0..MAX_EXCLUSIVE_NAME_ATTEMPTS {
            let name = unique_name("bak");
            match self
                .directory
                .hard_link_exclusive(&self.location.target_name, &name)
            {
                Ok(()) => {
                    backup_name = Some(name);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(PublicationFailure::Io {
                        phase: PublicationPhase::LinkBackup,
                        error,
                    });
                }
            }
        }
        let Some(backup_name) = backup_name else {
            return Err(PublicationFailure::Io {
                phase: PublicationPhase::LinkBackup,
                error: io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "could not reserve an exclusive trace backup name",
                ),
            });
        };
        self.backup = Some(TrackedEntry {
            name: backup_name,
            identity: Some(original_identity),
            expected_present: true,
        });
        if let Err(error) = self.directory.sync() {
            return Err(PublicationFailure::Io {
                phase: PublicationPhase::SyncBackup,
                error,
            });
        }
        let backup = self.backup.as_ref().expect("backup was just recorded");
        let expected =
            NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, original_identity));
        let observed = observe_entry(&self.directory, &backup.name);
        if observed != expected {
            return Err(PublicationFailure::IdentityChanged {
                phase: PublicationPhase::SyncBackup,
                expected,
                observed,
            });
        }
        Ok(())
    }

    fn verify_precommit<ProducerError>(&self) -> Result<(), PublicationFailure<ProducerError>> {
        self.revalidate_parent(PublicationPhase::VerifyPrecommit)?;
        self.verify_target(PublicationPhase::VerifyPrecommit)?;
        self.verify_temp()
    }

    fn revalidate_parent<ProducerError>(
        &self,
        phase: PublicationPhase,
    ) -> Result<(), PublicationFailure<ProducerError>> {
        let expected =
            NodeObservation::Present(NodeMetadata::new(NodeKind::Directory, self.parent_identity));
        let observed = match self.filesystem.path_metadata(&self.location.parent) {
            Ok(metadata) => NodeObservation::Present(metadata),
            Err(_) => NodeObservation::Unknown,
        };
        if observed == expected {
            Ok(())
        } else {
            Err(PublicationFailure::IdentityChanged {
                phase,
                expected,
                observed,
            })
        }
    }

    fn verify_target<ProducerError>(
        &self,
        phase: PublicationPhase,
    ) -> Result<(), PublicationFailure<ProducerError>> {
        let observed = observe_entry(&self.directory, &self.location.target_name);
        if observed == self.initial_target {
            Ok(())
        } else {
            Err(PublicationFailure::IdentityChanged {
                phase,
                expected: self.initial_target,
                observed,
            })
        }
    }

    fn publish_absent<Summary, ProducerError>(
        mut self,
        summary: Option<Summary>,
        cancellation: &PublicationCancellation,
    ) -> PublicationReport<Summary, ProducerError> {
        let temp_name = self.temp_name().to_os_string();
        if let Err(error) = self
            .directory
            .rename_noreplace(&temp_name, &self.location.target_name)
        {
            return self.finish_precommit(
                summary,
                PublicationFailure::Io {
                    phase: PublicationPhase::PublishRename,
                    error,
                },
            );
        }
        self.temp
            .as_mut()
            .expect("temp is tracked")
            .expected_present = false;
        if let Err(error) = self.directory.sync() {
            return self.rollback_absent(
                summary,
                PublicationFailure::Io {
                    phase: PublicationPhase::SyncPublication,
                    error,
                },
            );
        }
        if cancellation.is_cancelled() {
            return self.rollback_absent(
                summary,
                PublicationFailure::Cancelled {
                    phase: PublicationPhase::SyncPublication,
                },
            );
        }
        let new_identity = self.temp_identity();
        let expected_target =
            NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, new_identity));
        let observed_target = observe_entry(&self.directory, &self.location.target_name);
        let observed_temp = observe_entry(&self.directory, &temp_name);
        if observed_target != expected_target || observed_temp != NodeObservation::Absent {
            return self.indeterminate(
                summary,
                PublicationFailure::IdentityChanged {
                    phase: PublicationPhase::VerifyPublication,
                    expected: expected_target,
                    observed: observed_target,
                },
                PublicationUncertainty::new(
                    PublicationPhase::VerifyPublication,
                    "published target or moved temporary identity cannot be proven",
                ),
            );
        }
        if let Err(failure) = self.revalidate_parent(PublicationPhase::VerifyPublication) {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::VerifyPublication,
                    "output parent identity changed after publication",
                ),
            );
        }
        self.report(
            PublicationState::Published,
            PublicationPhase::Complete,
            summary,
            None,
            None,
        )
    }

    fn publish_force<Summary, ProducerError>(
        mut self,
        summary: Option<Summary>,
        cancellation: &PublicationCancellation,
        original_identity: FileIdentity,
    ) -> PublicationReport<Summary, ProducerError> {
        let temp_name = self.temp_name().to_os_string();
        if let Err(error) = self
            .directory
            .exchange(&temp_name, &self.location.target_name)
        {
            return self.finish_precommit(
                summary,
                PublicationFailure::Io {
                    phase: PublicationPhase::PublishRename,
                    error,
                },
            );
        }
        let new_identity = self.temp_identity();
        self.temp.as_mut().expect("temp is tracked").identity = Some(original_identity);
        if let Err(error) = self.directory.sync() {
            return self.rollback_force(
                summary,
                PublicationFailure::Io {
                    phase: PublicationPhase::SyncPublication,
                    error,
                },
                original_identity,
                new_identity,
            );
        }
        if cancellation.is_cancelled() {
            return self.rollback_force(
                summary,
                PublicationFailure::Cancelled {
                    phase: PublicationPhase::SyncPublication,
                },
                original_identity,
                new_identity,
            );
        }
        if let Err(uncertainty) = self.verify_force_publication(original_identity, new_identity) {
            let observed_target = self.observe_target();
            return self
                .rollback_force(
                    summary,
                    PublicationFailure::IdentityChanged {
                        phase: PublicationPhase::VerifyPublication,
                        expected: NodeObservation::Present(NodeMetadata::new(
                            NodeKind::RegularFile,
                            new_identity,
                        )),
                        observed: observed_target,
                    },
                    original_identity,
                    new_identity,
                )
                .with_uncertainty_if_indeterminate(uncertainty);
        }

        if let Err(uncertainty) = self.cleanup_temp(PublicationPhase::CleanupTemp) {
            return self.indeterminate(
                summary,
                PublicationFailure::IdentityChanged {
                    phase: uncertainty.phase(),
                    expected: NodeObservation::Present(NodeMetadata::new(
                        NodeKind::RegularFile,
                        original_identity,
                    )),
                    observed: self.observe_temp(),
                },
                uncertainty,
            );
        }
        if cancellation.is_cancelled() {
            return self.rollback_force_from_backup(
                summary,
                PublicationFailure::Cancelled {
                    phase: PublicationPhase::CleanupTemp,
                },
                original_identity,
                new_identity,
            );
        }
        let expected_target =
            NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, new_identity));
        let observed_target = self.observe_target();
        if observed_target != expected_target {
            return self.indeterminate(
                summary,
                PublicationFailure::IdentityChanged {
                    phase: PublicationPhase::CleanupBackup,
                    expected: expected_target,
                    observed: observed_target,
                },
                PublicationUncertainty::new(
                    PublicationPhase::CleanupBackup,
                    "published target changed before backup cleanup; backup was retained",
                ),
            );
        }
        if let Err(uncertainty) = self.cleanup_backup(PublicationPhase::CleanupBackup) {
            return self.indeterminate(
                summary,
                PublicationFailure::IdentityChanged {
                    phase: uncertainty.phase(),
                    expected: NodeObservation::Present(NodeMetadata::new(
                        NodeKind::RegularFile,
                        original_identity,
                    )),
                    observed: self.observe_backup(),
                },
                uncertainty,
            );
        }
        if self.observe_target() != expected_target
            || self.observe_temp() != NodeObservation::Absent
            || self.observe_backup() != NodeObservation::Absent
        {
            return self.indeterminate(
                summary,
                PublicationFailure::IdentityChanged {
                    phase: PublicationPhase::VerifyFinalState,
                    expected: expected_target,
                    observed: self.observe_target(),
                },
                PublicationUncertainty::new(
                    PublicationPhase::VerifyFinalState,
                    "published target or cleanup state cannot be proven",
                ),
            );
        }
        if let Err(failure) = self.revalidate_parent(PublicationPhase::VerifyFinalState) {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::VerifyFinalState,
                    "output parent identity changed after cleanup",
                ),
            );
        }
        self.report(
            PublicationState::Published,
            PublicationPhase::Complete,
            summary,
            None,
            None,
        )
    }

    fn verify_force_publication(
        &self,
        original_identity: FileIdentity,
        new_identity: FileIdentity,
    ) -> Result<(), PublicationUncertainty> {
        let expected_target =
            NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, new_identity));
        let expected_temp =
            NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, original_identity));
        let expected_backup = expected_temp;
        if self.observe_target() != expected_target
            || self.observe_temp() != expected_temp
            || self.observe_backup() != expected_backup
        {
            return Err(PublicationUncertainty::new(
                PublicationPhase::VerifyPublication,
                "force publication identities changed before durable cleanup",
            ));
        }
        Ok(())
    }

    fn rollback_absent<Summary, ProducerError>(
        mut self,
        summary: Option<Summary>,
        failure: PublicationFailure<ProducerError>,
    ) -> PublicationReport<Summary, ProducerError> {
        let new_identity = self.temp_identity();
        let expected_target =
            NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, new_identity));
        if self.observe_target() != expected_target
            || self.observe_temp() != NodeObservation::Absent
        {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::RollbackVerify,
                    "cannot identify the published target for rollback",
                ),
            );
        }
        let temp_name = self.temp_name().to_os_string();
        if let Err(error) = self
            .directory
            .rename_noreplace(&self.location.target_name, &temp_name)
        {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::RollbackRename,
                    format!("rollback rename failed: {error}"),
                ),
            );
        }
        let temp = self.temp.as_mut().expect("temp is tracked");
        temp.expected_present = true;
        temp.identity = Some(new_identity);
        if let Err(error) = self.directory.sync() {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::SyncRollback,
                    format!("rollback directory sync failed: {error}"),
                ),
            );
        }
        let mut uncertainty = self.cleanup_temp(PublicationPhase::CleanupTemp).err();
        if uncertainty.is_none() {
            uncertainty = self.prove_original_state().err();
        }
        match uncertainty {
            Some(uncertainty) => self.indeterminate(summary, failure, uncertainty),
            None => self.report(
                PublicationState::NotPublished,
                failure.phase(),
                summary,
                Some(failure),
                None,
            ),
        }
    }

    fn rollback_force<Summary, ProducerError>(
        mut self,
        summary: Option<Summary>,
        failure: PublicationFailure<ProducerError>,
        original_identity: FileIdentity,
        new_identity: FileIdentity,
    ) -> PublicationReport<Summary, ProducerError> {
        if self
            .verify_force_publication(original_identity, new_identity)
            .is_err()
        {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::RollbackVerify,
                    "cannot identify force-publication paths for rollback",
                ),
            );
        }
        let temp_name = self.temp_name().to_os_string();
        if let Err(error) = self
            .directory
            .exchange(&temp_name, &self.location.target_name)
        {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::RollbackRename,
                    format!("force rollback exchange failed: {error}"),
                ),
            );
        }
        self.temp.as_mut().expect("temp is tracked").identity = Some(new_identity);
        if let Err(error) = self.directory.sync() {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::SyncRollback,
                    format!("force rollback directory sync failed: {error}"),
                ),
            );
        }
        let mut uncertainty = self.cleanup_temp(PublicationPhase::CleanupTemp).err();
        if uncertainty.is_none() {
            uncertainty = self.cleanup_backup(PublicationPhase::CleanupBackup).err();
        }
        if uncertainty.is_none() {
            uncertainty = self.prove_original_state().err();
        }
        match uncertainty {
            Some(uncertainty) => self.indeterminate(summary, failure, uncertainty),
            None => self.report(
                PublicationState::NotPublished,
                failure.phase(),
                summary,
                Some(failure),
                None,
            ),
        }
    }

    fn rollback_force_from_backup<Summary, ProducerError>(
        mut self,
        summary: Option<Summary>,
        failure: PublicationFailure<ProducerError>,
        original_identity: FileIdentity,
        new_identity: FileIdentity,
    ) -> PublicationReport<Summary, ProducerError> {
        let expected_target =
            NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, new_identity));
        let expected_backup =
            NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, original_identity));
        if self.observe_target() != expected_target || self.observe_backup() != expected_backup {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::RollbackVerify,
                    "cannot identify backup and target for rollback",
                ),
            );
        }
        let backup_name = self.backup_name().to_os_string();
        if let Err(error) = self
            .directory
            .exchange(&backup_name, &self.location.target_name)
        {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::RollbackRename,
                    format!("backup rollback exchange failed: {error}"),
                ),
            );
        }
        self.backup.as_mut().expect("backup is tracked").identity = Some(new_identity);
        if let Err(error) = self.directory.sync() {
            return self.indeterminate(
                summary,
                failure,
                PublicationUncertainty::new(
                    PublicationPhase::SyncRollback,
                    format!("backup rollback directory sync failed: {error}"),
                ),
            );
        }
        let mut uncertainty = self.cleanup_backup(PublicationPhase::CleanupBackup).err();
        if uncertainty.is_none() {
            uncertainty = self.prove_original_state().err();
        }
        match uncertainty {
            Some(uncertainty) => self.indeterminate(summary, failure, uncertainty),
            None => self.report(
                PublicationState::NotPublished,
                failure.phase(),
                summary,
                Some(failure),
                None,
            ),
        }
    }

    fn finish_precommit<Summary, ProducerError>(
        mut self,
        summary: Option<Summary>,
        failure: PublicationFailure<ProducerError>,
    ) -> PublicationReport<Summary, ProducerError> {
        let has_backup = self
            .backup
            .as_ref()
            .is_some_and(|entry| entry.expected_present);
        let target = if has_backup {
            self.observe_target()
        } else {
            self.initial_target
        };
        let retain_backup = has_backup && target != self.initial_target;
        let mut uncertainty = if retain_backup {
            Some(PublicationUncertainty::new(
                PublicationPhase::CleanupBackup,
                format!(
                    "target changed before pre-commit cleanup: expected {:?}, observed {target:?}; backup was retained",
                    self.initial_target
                ),
            ))
        } else {
            self.cleanup_backup(PublicationPhase::CleanupBackup).err()
        };
        if let Err(error) = self.cleanup_temp(PublicationPhase::CleanupTemp)
            && uncertainty.is_none()
        {
            uncertainty = Some(error);
        }
        if uncertainty.is_none() {
            uncertainty = self.prove_original_state().err();
        }
        match uncertainty {
            Some(uncertainty) => self.indeterminate(summary, failure, uncertainty),
            None => self.report(
                PublicationState::NotPublished,
                failure.phase(),
                summary,
                Some(failure),
                None,
            ),
        }
    }

    fn cleanup_temp(&mut self, phase: PublicationPhase) -> Result<(), PublicationUncertainty> {
        let Some(entry) = self.temp.clone().filter(|entry| entry.expected_present) else {
            return Ok(());
        };
        self.cleanup_entry(&entry, phase, PublicationPhase::SyncTempCleanup)?;
        self.temp
            .as_mut()
            .expect("temp is tracked")
            .expected_present = false;
        Ok(())
    }

    fn cleanup_backup(&mut self, phase: PublicationPhase) -> Result<(), PublicationUncertainty> {
        let Some(entry) = self.backup.clone().filter(|entry| entry.expected_present) else {
            return Ok(());
        };
        self.cleanup_entry(&entry, phase, PublicationPhase::SyncBackupCleanup)?;
        self.backup
            .as_mut()
            .expect("backup is tracked")
            .expected_present = false;
        Ok(())
    }

    fn cleanup_entry(
        &self,
        entry: &TrackedEntry,
        phase: PublicationPhase,
        sync_phase: PublicationPhase,
    ) -> Result<(), PublicationUncertainty> {
        let observed = observe_entry(&self.directory, &entry.name);
        let Some(identity) = entry.identity else {
            return Err(PublicationUncertainty::new(
                phase,
                format!(
                    "cannot identify owned path {}",
                    self.location.parent.join(&entry.name).display()
                ),
            ));
        };
        let expected = NodeObservation::Present(NodeMetadata::new(NodeKind::RegularFile, identity));
        if observed != expected {
            return Err(PublicationUncertainty::new(
                phase,
                format!(
                    "owned path identity changed: expected {expected:?}, observed {observed:?}"
                ),
            ));
        }
        self.directory.unlink(&entry.name).map_err(|error| {
            PublicationUncertainty::new(phase, format!("owned path unlink failed: {error}"))
        })?;
        self.directory.sync().map_err(|error| {
            PublicationUncertainty::new(
                sync_phase,
                format!("owned path cleanup sync failed: {error}"),
            )
        })
    }

    fn prove_original_state(&self) -> Result<(), PublicationUncertainty> {
        let target = self.observe_target();
        if target != self.initial_target {
            return Err(PublicationUncertainty::new(
                PublicationPhase::VerifyFinalState,
                format!(
                    "original target state cannot be proven: expected {:?}, observed {target:?}",
                    self.initial_target
                ),
            ));
        }
        let parent = self
            .filesystem
            .path_metadata(&self.location.parent)
            .map(NodeObservation::Present)
            .unwrap_or(NodeObservation::Unknown);
        let expected_parent =
            NodeObservation::Present(NodeMetadata::new(NodeKind::Directory, self.parent_identity));
        if parent != expected_parent {
            return Err(PublicationUncertainty::new(
                PublicationPhase::VerifyFinalState,
                "output parent identity cannot be proven",
            ));
        }
        if self.observe_temp() != NodeObservation::Absent
            || self.observe_backup() != NodeObservation::Absent
        {
            return Err(PublicationUncertainty::new(
                PublicationPhase::VerifyFinalState,
                "temporary or backup cleanup cannot be proven",
            ));
        }
        Ok(())
    }

    fn temp_name(&self) -> &OsStr {
        &self.temp.as_ref().expect("temp is tracked").name
    }

    fn backup_name(&self) -> &OsStr {
        &self.backup.as_ref().expect("backup is tracked").name
    }

    fn temp_identity(&self) -> FileIdentity {
        self.temp
            .as_ref()
            .and_then(|temp| temp.identity)
            .expect("a publishable temp has a known identity")
    }

    fn observe_target(&self) -> NodeObservation {
        observe_entry(&self.directory, &self.location.target_name)
    }

    fn observe_temp(&self) -> NodeObservation {
        self.temp.as_ref().map_or(NodeObservation::Absent, |entry| {
            observe_entry(&self.directory, &entry.name)
        })
    }

    fn observe_backup(&self) -> NodeObservation {
        self.backup
            .as_ref()
            .map_or(NodeObservation::Absent, |entry| {
                observe_entry(&self.directory, &entry.name)
            })
    }

    fn report<Summary, ProducerError>(
        &self,
        state: PublicationState,
        phase: PublicationPhase,
        summary: Option<Summary>,
        failure: Option<PublicationFailure<ProducerError>>,
        uncertainty: Option<PublicationUncertainty>,
    ) -> PublicationReport<Summary, ProducerError> {
        PublicationReport {
            state,
            phase,
            paths: PublicationPaths {
                target: self.location.output.clone(),
                temp: self
                    .temp
                    .as_ref()
                    .map(|entry| self.location.parent.join(&entry.name)),
                backup: self
                    .backup
                    .as_ref()
                    .map(|entry| self.location.parent.join(&entry.name)),
            },
            observations: PublicationObservations {
                target: self.observe_target(),
                temp: self.observe_temp(),
                backup: self.observe_backup(),
            },
            summary,
            failure,
            uncertainty,
        }
    }

    fn indeterminate<Summary, ProducerError>(
        &self,
        summary: Option<Summary>,
        failure: PublicationFailure<ProducerError>,
        uncertainty: PublicationUncertainty,
    ) -> PublicationReport<Summary, ProducerError> {
        self.report(
            PublicationState::PublicationIndeterminate,
            uncertainty.phase(),
            summary,
            Some(failure),
            Some(uncertainty),
        )
    }
}

trait ReportExtension {
    fn with_uncertainty_if_indeterminate(self, uncertainty: PublicationUncertainty) -> Self;
}

impl<Summary, ProducerError> ReportExtension for PublicationReport<Summary, ProducerError> {
    fn with_uncertainty_if_indeterminate(mut self, uncertainty: PublicationUncertainty) -> Self {
        if self.state == PublicationState::PublicationIndeterminate && self.uncertainty.is_none() {
            self.phase = uncertainty.phase();
            self.uncertainty = Some(uncertainty);
        }
        self
    }
}

fn early_report<Summary, ProducerError>(
    output: &Path,
    state: PublicationState,
    phase: PublicationPhase,
    failure: PublicationFailure<ProducerError>,
) -> PublicationReport<Summary, ProducerError> {
    PublicationReport {
        state,
        phase,
        paths: PublicationPaths {
            target: output.to_path_buf(),
            temp: None,
            backup: None,
        },
        observations: PublicationObservations {
            target: NodeObservation::Unknown,
            temp: NodeObservation::Absent,
            backup: NodeObservation::Absent,
        },
        summary: None,
        failure: Some(failure),
        uncertainty: (state == PublicationState::PublicationIndeterminate).then(|| {
            PublicationUncertainty::new(phase, "output parent identity changed while opening")
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn direct_report<Directory, Summary, ProducerError>(
    location: &OutputLocation,
    directory: &Directory,
    state: PublicationState,
    phase: PublicationPhase,
    summary: Option<Summary>,
    failure: PublicationFailure<ProducerError>,
    temp: Option<&TrackedEntry>,
    backup: Option<&TrackedEntry>,
) -> PublicationReport<Summary, ProducerError>
where
    Directory: AtomicTraceDirectory,
{
    PublicationReport {
        state,
        phase,
        paths: PublicationPaths {
            target: location.output.clone(),
            temp: temp.map(|entry| location.parent.join(&entry.name)),
            backup: backup.map(|entry| location.parent.join(&entry.name)),
        },
        observations: PublicationObservations {
            target: observe_entry(directory, &location.target_name),
            temp: temp.map_or(NodeObservation::Absent, |entry| {
                observe_entry(directory, &entry.name)
            }),
            backup: backup.map_or(NodeObservation::Absent, |entry| {
                observe_entry(directory, &entry.name)
            }),
        },
        summary,
        failure: Some(failure),
        uncertainty: None,
    }
}

fn observe_entry<Directory: AtomicTraceDirectory>(
    directory: &Directory,
    name: &OsStr,
) -> NodeObservation {
    match directory.entry_metadata(name) {
        Ok(Some(metadata)) => NodeObservation::Present(metadata),
        Ok(None) => NodeObservation::Absent,
        Err(_) => NodeObservation::Unknown,
    }
}

fn unique_name(suffix: &str) -> OsString {
    let nonce = NEXT_NAME_NONCE.fetch_add(1, Ordering::Relaxed);
    OsString::from(format!(
        ".troupe-pftrace-{}-{nonce}.{suffix}",
        std::process::id()
    ))
}

fn validate_entry_name(name: &OsStr) -> io::Result<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
        || bytes.contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trace publication entry name is invalid",
        ));
    }
    Ok(())
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
))]
fn renameat2(directory_fd: RawFd, source: &OsStr, target: &OsStr, flags: u32) -> io::Result<()> {
    validate_entry_name(source)?;
    validate_entry_name(target)?;
    let source = CString::new(source.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source contains NUL"))?;
    let target = CString::new(target.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target contains NUL"))?;
    // SAFETY: names are validated C strings and both operations stay within the live dirfd.
    let result = unsafe {
        syscall(
            SYS_RENAMEAT2,
            directory_fd,
            source.as_ptr(),
            directory_fd,
            target.as_ptr(),
            flags,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
)))]
fn renameat2(
    _directory_fd: RawFd,
    _source: &OsStr,
    _target: &OsStr,
    _flags: u32,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic trace publication is unsupported on this platform",
    ))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
