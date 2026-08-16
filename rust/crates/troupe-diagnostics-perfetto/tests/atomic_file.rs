use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt,
    future::pending,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
};

use futures::executor::block_on;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use troupe_diagnostics_perfetto::atomic_file::{
    AtomicTraceDirectory, AtomicTraceFile, AtomicTraceFileSystem, FileIdentity, NodeKind,
    NodeMetadata, PublicationCancellation, PublicationFailure, PublicationPhase, PublicationState,
    TraceProducerFuture, TraceStreamProducer, publish_atomic_trace, publish_atomic_trace_with,
};

const OUTPUT: &str = "production/trace.pftrace";
const TARGET: &str = "trace.pftrace";
const PARENT_IDENTITY: FileIdentity = FileIdentity::new(7, 11);
const ORIGINAL_IDENTITY: FileIdentity = FileIdentity::new(7, 12);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Step {
    PathMetadata,
    OpenDirectory,
    DirectoryMetadata,
    CreateTemp,
    TempMetadata,
    Write,
    Flush,
    FileSync,
    Close,
    EntryMetadata,
    Link,
    RenameNoReplace,
    Exchange,
    Unlink,
    DirectorySync,
}

#[derive(Clone)]
struct Entry {
    metadata: NodeMetadata,
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Entry {
    fn new(kind: NodeKind, identity: FileIdentity, bytes: &[u8]) -> Self {
        Self {
            metadata: NodeMetadata::new(kind, identity),
            bytes: Arc::new(Mutex::new(bytes.to_vec())),
        }
    }
}

struct FakeState {
    parent: NodeMetadata,
    opened_parent: Option<NodeMetadata>,
    entries: HashMap<OsString, Entry>,
    next_inode: u64,
    created_kind: NodeKind,
    calls: Vec<Step>,
    occurrences: HashMap<Step, usize>,
    faults: Vec<(Step, usize)>,
    cancel_after: Vec<(Step, usize, PublicationCancellation)>,
    replace_target_before: Vec<(Step, usize)>,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            parent: NodeMetadata::new(NodeKind::Directory, PARENT_IDENTITY),
            opened_parent: None,
            entries: HashMap::new(),
            next_inode: 100,
            created_kind: NodeKind::RegularFile,
            calls: Vec::new(),
            occurrences: HashMap::new(),
            faults: Vec::new(),
            cancel_after: Vec::new(),
            replace_target_before: Vec::new(),
        }
    }
}

impl FakeState {
    fn begin(&mut self, step: Step) -> io::Result<usize> {
        self.calls.push(step);
        let occurrence = self.occurrences.entry(step).or_default();
        *occurrence += 1;
        let occurrence = *occurrence;
        if self.replace_target_before.contains(&(step, occurrence)) {
            let identity = self.allocate_identity();
            self.entries.insert(
                OsString::from(TARGET),
                Entry::new(NodeKind::RegularFile, identity, b"external replacement"),
            );
        }
        if self.faults.contains(&(step, occurrence)) {
            Err(io::Error::other(format!(
                "injected {step:?} occurrence {occurrence}"
            )))
        } else {
            Ok(occurrence)
        }
    }

    fn finish(&self, step: Step, occurrence: usize) {
        for (_, _, cancellation) in self
            .cancel_after
            .iter()
            .filter(|(candidate, nth, _)| *candidate == step && *nth == occurrence)
        {
            cancellation.cancel();
        }
    }

    fn allocate_identity(&mut self) -> FileIdentity {
        let identity = FileIdentity::new(7, self.next_inode);
        self.next_inode += 1;
        identity
    }
}

#[derive(Clone, Default)]
struct FakeFileSystem {
    state: Arc<Mutex<FakeState>>,
}

impl FakeFileSystem {
    fn with_target(kind: NodeKind, bytes: &[u8]) -> Self {
        let filesystem = Self::default();
        filesystem.state().entries.insert(
            OsString::from(TARGET),
            Entry::new(kind, ORIGINAL_IDENTITY, bytes),
        );
        filesystem
    }

    fn fail(&self, step: Step, occurrence: usize) {
        self.state().faults.push((step, occurrence));
    }

    fn cancel_after(&self, step: Step, occurrence: usize, cancellation: &PublicationCancellation) {
        self.state()
            .cancel_after
            .push((step, occurrence, cancellation.clone()));
    }

    fn replace_target_before(&self, step: Step, occurrence: usize) {
        self.state().replace_target_before.push((step, occurrence));
    }

    fn set_parent_kind(&self, kind: NodeKind) {
        self.state().parent = NodeMetadata::new(kind, PARENT_IDENTITY);
    }

    fn set_opened_parent_identity(&self, identity: FileIdentity) {
        self.state().opened_parent = Some(NodeMetadata::new(NodeKind::Directory, identity));
    }

    fn set_created_kind(&self, kind: NodeKind) {
        self.state().created_kind = kind;
    }

    fn bytes(&self, name: &str) -> Option<Vec<u8>> {
        let entry = self.state().entries.get(OsStr::new(name)).cloned()?;
        Some(lock(&entry.bytes).clone())
    }

    fn metadata(&self, name: &str) -> Option<NodeMetadata> {
        self.state()
            .entries
            .get(OsStr::new(name))
            .map(|entry| entry.metadata)
    }

    fn residue_names(&self) -> Vec<OsString> {
        let mut names = self
            .state()
            .entries
            .keys()
            .filter(|name| name.as_os_str() != OsStr::new(TARGET))
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn residue_payloads(&self) -> Vec<Vec<u8>> {
        let entries = self
            .state()
            .entries
            .iter()
            .filter(|(name, _)| name.as_os_str() != OsStr::new(TARGET))
            .map(|(_, entry)| entry.clone())
            .collect::<Vec<_>>();
        entries
            .into_iter()
            .map(|entry| lock(&entry.bytes).clone())
            .collect()
    }

    fn calls(&self) -> Vec<Step> {
        self.state().calls.clone()
    }

    fn state(&self) -> MutexGuard<'_, FakeState> {
        lock(&self.state)
    }
}

struct FakeDirectory {
    state: Arc<Mutex<FakeState>>,
}

struct FakeFile {
    state: Arc<Mutex<FakeState>>,
    entry: Entry,
}

impl AtomicTraceFileSystem for FakeFileSystem {
    type Directory = FakeDirectory;

    fn path_metadata(&self, _path: &Path) -> io::Result<NodeMetadata> {
        let mut state = self.state();
        let occurrence = state.begin(Step::PathMetadata)?;
        let metadata = state.parent;
        state.finish(Step::PathMetadata, occurrence);
        Ok(metadata)
    }

    fn open_directory(&self, _path: &Path) -> io::Result<Self::Directory> {
        let mut state = self.state();
        let occurrence = state.begin(Step::OpenDirectory)?;
        state.finish(Step::OpenDirectory, occurrence);
        Ok(FakeDirectory {
            state: Arc::clone(&self.state),
        })
    }
}

impl AtomicTraceDirectory for FakeDirectory {
    type File = FakeFile;

    fn metadata(&self) -> io::Result<NodeMetadata> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::DirectoryMetadata)?;
        let metadata = state.opened_parent.unwrap_or(state.parent);
        state.finish(Step::DirectoryMetadata, occurrence);
        Ok(metadata)
    }

    fn create_exclusive(&self, name: &OsStr, mode: u32) -> io::Result<Self::File> {
        assert_eq!(mode, 0o600);
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::CreateTemp)?;
        if state.entries.contains_key(name) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "collision"));
        }
        let identity = state.allocate_identity();
        let entry = Entry::new(state.created_kind, identity, b"");
        state.entries.insert(name.to_os_string(), entry.clone());
        state.finish(Step::CreateTemp, occurrence);
        Ok(FakeFile {
            state: Arc::clone(&self.state),
            entry,
        })
    }

    fn entry_metadata(&self, name: &OsStr) -> io::Result<Option<NodeMetadata>> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::EntryMetadata)?;
        let metadata = state.entries.get(name).map(|entry| entry.metadata);
        state.finish(Step::EntryMetadata, occurrence);
        Ok(metadata)
    }

    fn hard_link_exclusive(&self, source: &OsStr, target: &OsStr) -> io::Result<()> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::Link)?;
        if state.entries.contains_key(target) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "collision"));
        }
        let entry = state
            .entries
            .get(source)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "source missing"))?;
        state.entries.insert(target.to_os_string(), entry);
        state.finish(Step::Link, occurrence);
        Ok(())
    }

    fn rename_noreplace(&self, source: &OsStr, target: &OsStr) -> io::Result<()> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::RenameNoReplace)?;
        if state.entries.contains_key(target) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "target exists",
            ));
        }
        let entry = state
            .entries
            .remove(source)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "source missing"))?;
        state.entries.insert(target.to_os_string(), entry);
        state.finish(Step::RenameNoReplace, occurrence);
        Ok(())
    }

    fn exchange(&self, first: &OsStr, second: &OsStr) -> io::Result<()> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::Exchange)?;
        let first_entry = state
            .entries
            .remove(first)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "first missing"))?;
        let second_entry = state
            .entries
            .remove(second)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "second missing"))?;
        state.entries.insert(first.to_os_string(), second_entry);
        state.entries.insert(second.to_os_string(), first_entry);
        state.finish(Step::Exchange, occurrence);
        Ok(())
    }

    fn unlink(&self, name: &OsStr) -> io::Result<()> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::Unlink)?;
        state
            .entries
            .remove(name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "entry missing"))?;
        state.finish(Step::Unlink, occurrence);
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::DirectorySync)?;
        state.finish(Step::DirectorySync, occurrence);
        Ok(())
    }
}

impl AsyncWrite for FakeFile {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = lock(&self.state);
        let occurrence = match state.begin(Step::Write) {
            Ok(occurrence) => occurrence,
            Err(error) => return Poll::Ready(Err(error)),
        };
        lock(&self.entry.bytes).extend_from_slice(bytes);
        state.finish(Step::Write, occurrence);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = lock(&self.state);
        let occurrence = match state.begin(Step::Flush) {
            Ok(occurrence) => occurrence,
            Err(error) => return Poll::Ready(Err(error)),
        };
        state.finish(Step::Flush, occurrence);
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(context)
    }
}

impl AtomicTraceFile for FakeFile {
    fn metadata(&self) -> io::Result<NodeMetadata> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::TempMetadata)?;
        let metadata = self.entry.metadata;
        state.finish(Step::TempMetadata, occurrence);
        Ok(metadata)
    }

    fn sync_all(&mut self) -> io::Result<()> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::FileSync)?;
        state.finish(Step::FileSync, occurrence);
        Ok(())
    }

    fn close(self) -> io::Result<()> {
        let mut state = lock(&self.state);
        let occurrence = state.begin(Step::Close)?;
        state.finish(Step::Close, occurrence);
        Ok(())
    }
}

#[derive(Debug)]
enum ProducerError {
    Io(io::Error),
    Forced,
}

impl fmt::Display for ProducerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Forced => formatter.write_str("forced producer failure"),
        }
    }
}

enum ProducerMode {
    Complete,
    Fail,
    CancelAndWait(PublicationCancellation),
}

struct BytesProducer {
    bytes: Vec<u8>,
    mode: ProducerMode,
    calls: usize,
}

impl BytesProducer {
    fn complete(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            mode: ProducerMode::Complete,
            calls: 0,
        }
    }

    fn fail(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            mode: ProducerMode::Fail,
            calls: 0,
        }
    }

    fn cancel_and_wait(bytes: &[u8], cancellation: &PublicationCancellation) -> Self {
        Self {
            bytes: bytes.to_vec(),
            mode: ProducerMode::CancelAndWait(cancellation.clone()),
            calls: 0,
        }
    }
}

impl TraceStreamProducer for BytesProducer {
    type Summary = usize;
    type Error = ProducerError;

    fn produce<'operation>(
        &'operation mut self,
        writer: &'operation mut (dyn AsyncWrite + Unpin),
    ) -> TraceProducerFuture<'operation, Self::Summary, Self::Error> {
        self.calls += 1;
        Box::pin(async move {
            writer
                .write_all(&self.bytes)
                .await
                .map_err(ProducerError::Io)?;
            match &self.mode {
                ProducerMode::Complete => Ok(self.bytes.len()),
                ProducerMode::Fail => Err(ProducerError::Forced),
                ProducerMode::CancelAndWait(cancellation) => {
                    cancellation.cancel();
                    pending::<()>().await;
                    unreachable!()
                }
            }
        })
    }
}

fn publish(
    filesystem: &FakeFileSystem,
    force: bool,
    cancellation: &PublicationCancellation,
    producer: &mut BytesProducer,
) -> troupe_diagnostics_perfetto::atomic_file::PublicationReport<usize, ProducerError> {
    block_on(publish_atomic_trace_with(
        filesystem,
        Path::new(OUTPUT),
        force,
        cancellation,
        producer,
    ))
}

fn assert_clean(filesystem: &FakeFileSystem) {
    assert!(
        filesystem.residue_names().is_empty(),
        "publication residues: {:?}",
        filesystem.residue_names()
    );
}

fn assert_mutations_are_followed_by_sync(calls: &[Step]) {
    let mut awaiting_sync = None;
    for step in calls {
        if matches!(
            step,
            Step::CreateTemp | Step::Link | Step::RenameNoReplace | Step::Exchange | Step::Unlink
        ) {
            assert!(
                awaiting_sync.is_none(),
                "mutation {step:?} followed {awaiting_sync:?} without a directory sync"
            );
            awaiting_sync = Some(*step);
        } else if *step == Step::DirectorySync {
            awaiting_sync = None;
        }
    }
    assert!(
        awaiting_sync.is_none(),
        "final mutation {awaiting_sync:?} lacked a directory sync"
    );
}

#[test]
fn absent_target_is_published_from_a_synced_temporary() {
    let filesystem = FakeFileSystem::default();
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"perfetto trace");

    let report = publish(&filesystem, false, &cancellation, &mut producer);

    assert_eq!(report.state(), PublicationState::Published);
    assert_eq!(report.phase(), PublicationPhase::Complete);
    assert_eq!(report.summary(), Some(&14));
    assert!(report.failure().is_none());
    assert!(report.uncertainty().is_none());
    assert_eq!(
        filesystem.bytes(TARGET).as_deref(),
        Some(&b"perfetto trace"[..])
    );
    assert_clean(&filesystem);
    assert_mutations_are_followed_by_sync(&filesystem.calls());
}

#[test]
fn force_publish_links_backup_exchanges_and_cleans_every_owned_name() {
    let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"new trace");

    let report = publish(&filesystem, true, &cancellation, &mut producer);

    assert_eq!(report.state(), PublicationState::Published);
    assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"new trace"[..]));
    assert_clean(&filesystem);
    let calls = filesystem.calls();
    assert!(calls.contains(&Step::Link));
    assert!(calls.contains(&Step::Exchange));
    assert_eq!(
        calls.iter().filter(|step| **step == Step::Unlink).count(),
        2
    );
    assert_mutations_are_followed_by_sync(&calls);
}

#[test]
fn existing_target_without_force_never_starts_the_producer() {
    let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"original");
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"replacement");

    let report = publish(&filesystem, false, &cancellation, &mut producer);

    assert_eq!(report.state(), PublicationState::NotPublished);
    assert!(matches!(
        report.failure(),
        Some(PublicationFailure::TargetAlreadyExists(_))
    ));
    assert_eq!(producer.calls, 0);
    assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"original"[..]));
    assert_clean(&filesystem);
}

#[test]
fn invalid_paths_and_non_regular_nodes_are_rejected_before_encoding() {
    for path in ["-", "../trace.pftrace", "production/"] {
        let filesystem = FakeFileSystem::default();
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"unused");
        let report = block_on(publish_atomic_trace_with(
            &filesystem,
            Path::new(path),
            false,
            &cancellation,
            &mut producer,
        ));
        assert_eq!(report.state(), PublicationState::NotPublished, "{path}");
        assert_eq!(producer.calls, 0, "{path}");
    }

    for kind in [NodeKind::Directory, NodeKind::Symlink, NodeKind::Other] {
        let filesystem = FakeFileSystem::with_target(kind, b"");
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"unused");
        let report = publish(&filesystem, true, &cancellation, &mut producer);
        assert_eq!(report.state(), PublicationState::NotPublished, "{kind:?}");
        assert!(matches!(
            report.failure(),
            Some(PublicationFailure::TargetTypeRejected(found)) if *found == kind
        ));
        assert_eq!(producer.calls, 0);
    }

    let filesystem = FakeFileSystem::default();
    filesystem.set_parent_kind(NodeKind::Symlink);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"unused");
    let report = publish(&filesystem, false, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::NotPublished);
    assert_eq!(producer.calls, 0);

    let filesystem = FakeFileSystem::default();
    filesystem.set_opened_parent_identity(FileIdentity::new(7, 99));
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"unused");
    let report = publish(&filesystem, false, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::NotPublished);
    assert!(matches!(
        report.failure(),
        Some(PublicationFailure::IdentityChanged {
            phase: PublicationPhase::OpenParent,
            ..
        })
    ));
    assert_eq!(producer.calls, 0);
}

#[test]
fn preparation_faults_clean_only_identified_temporary_files() {
    for step in [
        Step::CreateTemp,
        Step::Write,
        Step::Flush,
        Step::FileSync,
        Step::Close,
    ] {
        let filesystem = FakeFileSystem::default();
        filesystem.fail(step, 1);
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"trace");
        let report = publish(&filesystem, false, &cancellation, &mut producer);
        assert_eq!(report.state(), PublicationState::NotPublished, "{step:?}");
        assert_clean(&filesystem);
    }

    let filesystem = FakeFileSystem::default();
    filesystem.fail(Step::TempMetadata, 1);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"trace");
    let report = publish(&filesystem, false, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::PublicationIndeterminate);
    assert_eq!(filesystem.residue_names().len(), 1);

    let filesystem = FakeFileSystem::default();
    filesystem.set_created_kind(NodeKind::Other);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"trace");
    let report = publish(&filesystem, false, &cancellation, &mut producer);
    assert!(matches!(
        report.failure(),
        Some(PublicationFailure::TemporaryTypeRejected(NodeKind::Other))
    ));
    assert_eq!(report.state(), PublicationState::PublicationIndeterminate);
}

#[test]
fn adapter_faults_before_publication_cover_open_inspect_and_precommit_steps() {
    for (step, occurrence) in [
        (Step::PathMetadata, 1),
        (Step::OpenDirectory, 1),
        (Step::DirectoryMetadata, 1),
        (Step::EntryMetadata, 1),
        (Step::DirectorySync, 1),
        (Step::EntryMetadata, 2),
        (Step::EntryMetadata, 3),
        (Step::EntryMetadata, 4),
        (Step::PathMetadata, 2),
    ] {
        let filesystem = FakeFileSystem::default();
        filesystem.fail(step, occurrence);
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"trace");
        let report = publish(&filesystem, false, &cancellation, &mut producer);

        assert_eq!(
            report.state(),
            PublicationState::NotPublished,
            "{step:?} occurrence {occurrence}"
        );
        assert_eq!(filesystem.metadata(TARGET), None);
        assert_clean(&filesystem);
    }
}

#[test]
fn observation_faults_after_absent_rename_are_indeterminate() {
    for (step, occurrence) in [(Step::EntryMetadata, 5), (Step::PathMetadata, 3)] {
        let filesystem = FakeFileSystem::default();
        filesystem.fail(step, occurrence);
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"trace");
        let report = publish(&filesystem, false, &cancellation, &mut producer);

        assert_eq!(
            report.state(),
            PublicationState::PublicationIndeterminate,
            "{step:?} occurrence {occurrence}"
        );
        assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"trace"[..]));
        assert_clean(&filesystem);
    }
}

#[test]
fn producer_failure_and_cancellation_prove_not_published_after_cleanup() {
    let filesystem = FakeFileSystem::default();
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::fail(b"partial");
    let report = publish(&filesystem, false, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::NotPublished);
    assert!(matches!(
        report.failure(),
        Some(PublicationFailure::Producer(ProducerError::Forced))
    ));
    assert_clean(&filesystem);

    let filesystem = FakeFileSystem::default();
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::cancel_and_wait(b"partial", &cancellation);
    let report = publish(&filesystem, false, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::NotPublished);
    assert!(matches!(
        report.failure(),
        Some(PublicationFailure::Cancelled {
            phase: PublicationPhase::Encode
        })
    ));
    assert_clean(&filesystem);
}

#[test]
fn absent_publish_sync_failure_rolls_back_to_a_proven_absence() {
    let filesystem = FakeFileSystem::default();
    filesystem.fail(Step::DirectorySync, 2);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"trace");

    let report = publish(&filesystem, false, &cancellation, &mut producer);

    assert_eq!(report.state(), PublicationState::NotPublished);
    assert_eq!(report.phase(), PublicationPhase::SyncPublication);
    assert_eq!(filesystem.metadata(TARGET), None);
    assert_clean(&filesystem);
    assert_eq!(
        filesystem
            .calls()
            .iter()
            .filter(|step| **step == Step::RenameNoReplace)
            .count(),
        2
    );
}

#[test]
fn absent_rollback_faults_report_indeterminate_and_keep_paths_discoverable() {
    for (second_fault, expected_target, expected_residues) in [
        (Step::RenameNoReplace, true, 0),
        (Step::DirectorySync, false, 1),
        (Step::Unlink, false, 1),
    ] {
        let filesystem = FakeFileSystem::default();
        filesystem.fail(Step::DirectorySync, 2);
        let second_occurrence = if second_fault == Step::DirectorySync {
            3
        } else if second_fault == Step::RenameNoReplace {
            2
        } else {
            1
        };
        filesystem.fail(second_fault, second_occurrence);
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"trace");
        let report = publish(&filesystem, false, &cancellation, &mut producer);

        assert_eq!(
            report.state(),
            PublicationState::PublicationIndeterminate,
            "{second_fault:?}"
        );
        assert!(report.uncertainty().is_some());
        assert_eq!(filesystem.metadata(TARGET).is_some(), expected_target);
        assert_eq!(filesystem.residue_names().len(), expected_residues);
        assert!(
            report.paths().temp().is_some(),
            "temporary path must remain discoverable even after a rename"
        );
    }
}

#[test]
fn no_replace_race_preserves_the_external_target_and_owned_temp_identity() {
    let filesystem = FakeFileSystem::default();
    filesystem.replace_target_before(Step::RenameNoReplace, 1);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"trace");

    let report = publish(&filesystem, false, &cancellation, &mut producer);

    assert_eq!(report.state(), PublicationState::PublicationIndeterminate);
    assert_eq!(
        filesystem.bytes(TARGET).as_deref(),
        Some(&b"external replacement"[..])
    );
    assert_clean(&filesystem);
}

#[test]
fn force_failures_before_publication_restore_the_original() {
    for (step, occurrence) in [
        (Step::Link, 1),
        (Step::DirectorySync, 2),
        (Step::Exchange, 1),
    ] {
        let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
        filesystem.fail(step, occurrence);
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"new trace");
        let report = publish(&filesystem, true, &cancellation, &mut producer);

        assert_eq!(report.state(), PublicationState::NotPublished, "{step:?}");
        assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"old trace"[..]));
        assert_clean(&filesystem);
    }
}

#[test]
fn force_publication_failure_rolls_back_or_reports_exact_uncertainty() {
    let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
    filesystem.fail(Step::DirectorySync, 3);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"new trace");
    let report = publish(&filesystem, true, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::NotPublished);
    assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"old trace"[..]));
    assert_clean(&filesystem);

    let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
    filesystem.fail(Step::DirectorySync, 3);
    filesystem.fail(Step::Exchange, 2);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"new trace");
    let report = publish(&filesystem, true, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::PublicationIndeterminate);
    assert_eq!(report.phase(), PublicationPhase::RollbackRename);
    assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"new trace"[..]));
    assert_eq!(filesystem.residue_names().len(), 2);
}

#[test]
fn cancellation_after_each_force_commit_boundary_uses_the_same_rollback_machine() {
    let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
    let cancellation = PublicationCancellation::default();
    filesystem.cancel_after(Step::Exchange, 1, &cancellation);
    let mut producer = BytesProducer::complete(b"new trace");
    let report = publish(&filesystem, true, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::NotPublished);
    assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"old trace"[..]));
    assert_clean(&filesystem);

    let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
    let cancellation = PublicationCancellation::default();
    filesystem.cancel_after(Step::Unlink, 1, &cancellation);
    let mut producer = BytesProducer::complete(b"new trace");
    let report = publish(&filesystem, true, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::NotPublished);
    assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"old trace"[..]));
    assert_clean(&filesystem);
}

#[test]
fn cleanup_faults_and_target_races_never_delete_unproven_paths() {
    for unlink_occurrence in [1, 2] {
        let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
        filesystem.fail(Step::Unlink, unlink_occurrence);
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"new trace");
        let report = publish(&filesystem, true, &cancellation, &mut producer);
        assert_eq!(
            report.state(),
            PublicationState::PublicationIndeterminate,
            "unlink {unlink_occurrence}"
        );
        assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"new trace"[..]));
        assert!(!filesystem.residue_names().is_empty());
    }

    let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
    filesystem.replace_target_before(Step::EntryMetadata, 3);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"new trace");
    let report = publish(&filesystem, true, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::PublicationIndeterminate);
    assert_eq!(
        filesystem.bytes(TARGET).as_deref(),
        Some(&b"external replacement"[..])
    );
    assert_clean(&filesystem);

    let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
    filesystem.replace_target_before(Step::Exchange, 1);
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"new trace");
    let report = publish(&filesystem, true, &cancellation, &mut producer);
    assert_eq!(report.state(), PublicationState::PublicationIndeterminate);
    assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"new trace"[..]));
    assert!(
        filesystem
            .residue_payloads()
            .iter()
            .any(|bytes| bytes == b"external replacement")
    );
}

#[test]
fn force_target_races_after_backup_creation_retain_the_old_inode() {
    for entry_metadata_occurrence in [5, 11] {
        let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
        filesystem.replace_target_before(Step::EntryMetadata, entry_metadata_occurrence);
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"new trace");
        let report = publish(&filesystem, true, &cancellation, &mut producer);

        assert_eq!(
            report.state(),
            PublicationState::PublicationIndeterminate,
            "entry metadata {entry_metadata_occurrence}"
        );
        assert_eq!(
            filesystem.bytes(TARGET).as_deref(),
            Some(&b"external replacement"[..])
        );
        assert!(
            filesystem
                .residue_payloads()
                .iter()
                .any(|bytes| bytes == b"old trace")
        );
        assert!(report.paths().backup().is_some());
    }
}

#[test]
fn force_cleanup_sync_faults_preserve_closed_indeterminate_reports() {
    for directory_sync_occurrence in [4, 5] {
        let filesystem = FakeFileSystem::with_target(NodeKind::RegularFile, b"old trace");
        filesystem.fail(Step::DirectorySync, directory_sync_occurrence);
        let cancellation = PublicationCancellation::default();
        let mut producer = BytesProducer::complete(b"new trace");
        let report = publish(&filesystem, true, &cancellation, &mut producer);

        assert_eq!(
            report.state(),
            PublicationState::PublicationIndeterminate,
            "directory sync {directory_sync_occurrence}"
        );
        assert_eq!(filesystem.bytes(TARGET).as_deref(), Some(&b"new trace"[..]));
        assert!(report.paths().temp().is_some());
        assert!(report.paths().backup().is_some());
        if directory_sync_occurrence == 4 {
            assert_eq!(filesystem.residue_names().len(), 1);
        } else {
            assert_clean(&filesystem);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn real_filesystem_publishes_and_force_replaces_a_trace() {
    let directory = TestDirectory::new("real");
    let output = directory.path().join("trace.pftrace");
    let cancellation = PublicationCancellation::default();
    let mut producer = BytesProducer::complete(b"first");
    let report = block_on(publish_atomic_trace(
        &output,
        false,
        &cancellation,
        &mut producer,
    ));
    assert_eq!(report.state(), PublicationState::Published);
    assert_eq!(std::fs::read(&output).unwrap(), b"first");

    let mut producer = BytesProducer::complete(b"second");
    let report = block_on(publish_atomic_trace(
        &output,
        true,
        &cancellation,
        &mut producer,
    ));
    assert_eq!(report.state(), PublicationState::Published);
    assert_eq!(std::fs::read(&output).unwrap(), b"second");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "troupe-atomic-file-{label}-{}-{}",
            std::process::id(),
            PARENT_IDENTITY.inode()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
