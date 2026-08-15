use std::{
    cell::{Cell, RefCell},
    fs, io,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::{
    archive::layout::ArchiveLayout,
    registry::{
        codec::decode_registry_entry,
        model::{BindEndpoint, RegistryEntry},
        process_identity::ProcessIdentity,
        publish::{
            ListenerState, RealRegistryDirectory, RealRegistryFile, RealRegistryFileSystem,
            RegistryDirectory, RegistryFile, RegistryFileSystem, RegistryNodeMetadata,
            RegistryPublicationReadiness, RegistryPublishErrorCode, publish_registry_entry,
            publish_registry_entry_with,
        },
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);
static UMASK_LOCK: Mutex<()> = Mutex::new(());

unsafe extern "C" {
    fn umask(mask: u32) -> u32;
}

struct UmaskGuard(u32);

impl UmaskGuard {
    fn set(mask: u32) -> Self {
        // SAFETY: UMASK_LOCK serializes the process-global mutation and Drop restores it.
        Self(unsafe { umask(mask) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: restore the value captured under UMASK_LOCK.
        unsafe {
            umask(self.0);
        }
    }
}

struct TestProductionRoot(PathBuf);

impl TestProductionRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-r01-registry-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestProductionRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_id(value: &str) -> CanonicalUuid {
    CanonicalUuid::parse(value).unwrap()
}

fn entry(layout: &ArchiveLayout) -> RegistryEntry {
    RegistryEntry::new(
        layout.run_id(),
        layout.run_directory(),
        std::process::id(),
        ProcessIdentity::new("test", &format!("boot:{}", layout.run_id())).unwrap(),
        BindEndpoint::new("0.0.0.0", 43120).unwrap(),
        None,
        "2026-08-14T10:00:00Z",
    )
    .unwrap()
}

fn ready() -> RegistryPublicationReadiness {
    RegistryPublicationReadiness::new(true, true)
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn set_mode(path: &Path, value: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(value)).unwrap();
}

fn directory_entries(path: &Path) -> Vec<String> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn publication_requires_store_and_listener_readiness_before_filesystem_access() {
    let root = TestProductionRoot::new("readiness");
    let layout = ArchiveLayout::prepare(root.path(), run_id(RUN_ID)).unwrap();
    let entry = entry(&layout);

    for readiness in [
        RegistryPublicationReadiness::new(false, true),
        RegistryPublicationReadiness::new(true, false),
        RegistryPublicationReadiness::new(false, false),
    ] {
        let filesystem = FaultFileSystem::new(None);
        let error =
            publish_registry_entry_with(&filesystem, &layout, &entry, readiness).unwrap_err();
        assert_eq!(error.code(), RegistryPublishErrorCode::NotReady);
        assert_eq!(filesystem.calls(), 0);
    }
}

#[test]
fn real_publish_and_unpublish_are_private_atomic_durable_and_exactly_scoped() {
    let _lock = UMASK_LOCK.lock().unwrap();
    let root = TestProductionRoot::new("real");
    let layout = ArchiveLayout::prepare(root.path(), run_id(RUN_ID)).unwrap();
    set_mode(layout.instances_directory(), 0o777);
    let _umask = UmaskGuard::set(0);

    let expected = entry(&layout);
    let mut publication = publish_registry_entry(&layout, &expected, ready()).unwrap();
    let expected_path = layout.instances_directory().join(format!("{RUN_ID}.json"));

    assert_eq!(publication.locator_path(), expected_path);
    assert!(publication.is_published());
    assert_eq!(mode(layout.instances_directory()), DIRECTORY_MODE);
    assert_eq!(mode(&expected_path), FILE_MODE);
    assert_eq!(
        directory_entries(layout.instances_directory()),
        [format!("{RUN_ID}.json")]
    );
    let decoded =
        decode_registry_entry(&expected_path, &fs::read(&expected_path).unwrap()).unwrap();
    assert_eq!(decoded, expected);

    publication.unpublish(ListenerState::Stopped).unwrap_err();
    assert!(expected_path.exists());
    assert!(publication.is_published());
    publication.unpublish(ListenerState::Running).unwrap();
    assert!(!expected_path.exists());
    assert!(!publication.is_published());
    assert!(layout.run_directory().is_dir());
}

#[test]
fn existing_locator_is_never_overwritten_and_temporary_names_are_cleaned() {
    let root = TestProductionRoot::new("collision");
    let layout = ArchiveLayout::prepare(root.path(), run_id(RUN_ID)).unwrap();
    let target = layout.instances_directory().join(format!("{RUN_ID}.json"));
    fs::write(&target, b"existing locator must survive").unwrap();

    let error = publish_registry_entry(&layout, &entry(&layout), ready()).unwrap_err();
    assert_eq!(error.code(), RegistryPublishErrorCode::TargetAlreadyExists);
    assert_eq!(fs::read(&target).unwrap(), b"existing locator must survive");
    assert_eq!(
        directory_entries(layout.instances_directory()),
        [format!("{RUN_ID}.json")]
    );
}

#[test]
fn every_publish_filesystem_failure_is_typed_and_exposes_no_partial_json() {
    let cases = [
        (
            FaultPoint::PathMetadata,
            RegistryPublishErrorCode::DirectoryInspectFailed,
        ),
        (
            FaultPoint::OpenDirectory,
            RegistryPublishErrorCode::DirectoryInspectFailed,
        ),
        (
            FaultPoint::DirectoryStat,
            RegistryPublishErrorCode::DirectoryInspectFailed,
        ),
        (
            FaultPoint::DirectoryChmod,
            RegistryPublishErrorCode::DirectoryPermissionFailed,
        ),
        (
            FaultPoint::TempCreate,
            RegistryPublishErrorCode::TempCreateFailed,
        ),
        (
            FaultPoint::TempChmod,
            RegistryPublishErrorCode::TempPermissionFailed,
        ),
        (
            FaultPoint::TempStat,
            RegistryPublishErrorCode::TempMetadataFailed,
        ),
        (
            FaultPoint::TempWrite,
            RegistryPublishErrorCode::TempWriteFailed,
        ),
        (
            FaultPoint::TempSync,
            RegistryPublishErrorCode::TempSyncFailed,
        ),
        (
            FaultPoint::TempClose,
            RegistryPublishErrorCode::TempCloseFailed,
        ),
        (FaultPoint::Rename, RegistryPublishErrorCode::RenameFailed),
        (
            FaultPoint::EntryMetadata,
            RegistryPublishErrorCode::TargetMetadataFailed,
        ),
        (
            FaultPoint::DirectorySync,
            RegistryPublishErrorCode::DirectorySyncFailed,
        ),
    ];

    for (fault, expected_code) in cases {
        let root = TestProductionRoot::new(&format!("fault-{fault:?}"));
        let layout = ArchiveLayout::prepare(root.path(), run_id(RUN_ID)).unwrap();
        let filesystem = FaultFileSystem::new(Some(fault));
        let error = publish_registry_entry_with(&filesystem, &layout, &entry(&layout), ready())
            .expect_err("fault must fail publication");
        assert_eq!(error.code(), expected_code, "wrong code for {fault:?}");
        assert!(
            directory_entries(layout.instances_directory()).is_empty(),
            "partial locator or temp survived {fault:?}"
        );
    }
}

#[test]
fn concurrent_runs_publish_independently_and_startup_rollback_removes_only_its_run() {
    let root = TestProductionRoot::new("concurrent");
    let first_layout = ArchiveLayout::prepare(root.path(), run_id(RUN_ID)).unwrap();
    let second_layout = ArchiveLayout::prepare(root.path(), run_id(OTHER_RUN_ID)).unwrap();
    let first_entry = entry(&first_layout);
    let second_entry = entry(&second_layout);

    let (mut first, mut second) = thread::scope(|scope| {
        let first =
            scope.spawn(|| publish_registry_entry(&first_layout, &first_entry, ready()).unwrap());
        let second =
            scope.spawn(|| publish_registry_entry(&second_layout, &second_entry, ready()).unwrap());
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(
        directory_entries(first_layout.instances_directory()),
        [format!("{RUN_ID}.json"), format!("{OTHER_RUN_ID}.json")]
    );

    first.rollback_startup().unwrap();
    assert!(!first.locator_path().exists());
    assert!(second.locator_path().exists());
    assert!(first_layout.run_directory().is_dir());
    assert!(second_layout.run_directory().is_dir());
    second.unpublish(ListenerState::Running).unwrap();
}

#[test]
fn unpublish_failures_are_typed_and_listener_order_is_enforced() {
    for (fault, expected_code, remains_visible) in [
        (
            FaultPoint::Unlink,
            RegistryPublishErrorCode::UnlinkFailed,
            true,
        ),
        (
            FaultPoint::EntryRead,
            RegistryPublishErrorCode::TargetReadFailed,
            true,
        ),
        (
            FaultPoint::DirectorySync,
            RegistryPublishErrorCode::DirectorySyncFailed,
            false,
        ),
    ] {
        let root = TestProductionRoot::new(&format!("unpublish-{fault:?}"));
        let layout = ArchiveLayout::prepare(root.path(), run_id(RUN_ID)).unwrap();
        let mut publication = publish_registry_entry(&layout, &entry(&layout), ready()).unwrap();
        let filesystem = FaultFileSystem::new(Some(fault));
        let error = publication
            .unpublish_with(&filesystem, ListenerState::Running)
            .unwrap_err();
        assert_eq!(error.code(), expected_code);
        assert_eq!(publication.locator_path().exists(), remains_visible);
        assert_eq!(publication.is_published(), remains_visible);
        if remains_visible {
            publication.unpublish(ListenerState::Running).unwrap();
        }
    }
}

#[test]
fn unpublish_revalidates_file_and_directory_identity_before_deleting() {
    let root = TestProductionRoot::new("replacement");
    let layout = ArchiveLayout::prepare(root.path(), run_id(RUN_ID)).unwrap();
    let mut publication = publish_registry_entry(&layout, &entry(&layout), ready()).unwrap();
    let locator = publication.locator_path().to_path_buf();
    fs::remove_file(&locator).unwrap();
    fs::write(&locator, b"replacement").unwrap();
    let error = publication.unpublish(ListenerState::Running).unwrap_err();
    assert_eq!(
        error.code(),
        RegistryPublishErrorCode::LocatorIdentityChanged
    );
    assert_eq!(fs::read(&locator).unwrap(), b"replacement");

    fs::remove_file(&locator).unwrap();
    let original_directory = layout.instances_directory().with_extension("original");
    fs::rename(layout.instances_directory(), &original_directory).unwrap();
    fs::create_dir(layout.instances_directory()).unwrap();
    set_mode(layout.instances_directory(), DIRECTORY_MODE);
    fs::write(&locator, b"new directory locator").unwrap();
    let error = publication.unpublish(ListenerState::Running).unwrap_err();
    assert_eq!(
        error.code(),
        RegistryPublishErrorCode::DirectoryIdentityChanged
    );
    assert_eq!(fs::read(&locator).unwrap(), b"new directory locator");
}

#[test]
fn symlinked_instances_directory_is_rejected_without_following_it() {
    let root = TestProductionRoot::new("symlink");
    let layout = ArchiveLayout::prepare(root.path(), run_id(RUN_ID)).unwrap();
    let original = layout.instances_directory().with_extension("original");
    fs::rename(layout.instances_directory(), &original).unwrap();
    symlink(&original, layout.instances_directory()).unwrap();

    let error = publish_registry_entry(&layout, &entry(&layout), ready()).unwrap_err();
    assert_eq!(
        error.code(),
        RegistryPublishErrorCode::DirectorySymlinkRejected
    );
    assert!(directory_entries(&original).is_empty());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultPoint {
    PathMetadata,
    OpenDirectory,
    DirectoryStat,
    DirectoryChmod,
    TempCreate,
    TempChmod,
    TempStat,
    TempWrite,
    TempSync,
    TempClose,
    Rename,
    EntryMetadata,
    EntryRead,
    DirectorySync,
    Unlink,
}

struct FaultState {
    fault: Option<FaultPoint>,
    fired: Cell<bool>,
    calls: Cell<usize>,
    paths: RefCell<Vec<PathBuf>>,
}

impl FaultState {
    fn call(&self, point: FaultPoint, path: &Path) -> io::Result<()> {
        self.calls.set(self.calls.get() + 1);
        self.paths.borrow_mut().push(path.to_path_buf());
        if self.fault == Some(point) && !self.fired.replace(true) {
            return Err(io::Error::other(format!("injected {point:?}")));
        }
        Ok(())
    }
}

struct FaultFileSystem {
    inner: RealRegistryFileSystem,
    state: Rc<FaultState>,
}

impl FaultFileSystem {
    fn new(fault: Option<FaultPoint>) -> Self {
        Self {
            inner: RealRegistryFileSystem,
            state: Rc::new(FaultState {
                fault,
                fired: Cell::new(false),
                calls: Cell::new(0),
                paths: RefCell::new(Vec::new()),
            }),
        }
    }

    fn calls(&self) -> usize {
        self.state.calls.get()
    }
}

struct FaultDirectory {
    inner: RealRegistryDirectory,
    state: Rc<FaultState>,
    path: PathBuf,
}

struct FaultFile {
    inner: RealRegistryFile,
    state: Rc<FaultState>,
    path: PathBuf,
}

impl RegistryFileSystem for FaultFileSystem {
    type Directory = FaultDirectory;

    fn path_metadata(&self, path: &Path) -> io::Result<RegistryNodeMetadata> {
        self.state.call(FaultPoint::PathMetadata, path)?;
        self.inner.path_metadata(path)
    }

    fn open_directory(&self, path: &Path) -> io::Result<Self::Directory> {
        self.state.call(FaultPoint::OpenDirectory, path)?;
        Ok(FaultDirectory {
            inner: self.inner.open_directory(path)?,
            state: Rc::clone(&self.state),
            path: path.to_path_buf(),
        })
    }
}

impl RegistryDirectory for FaultDirectory {
    type File = FaultFile;

    fn fstat(&self) -> io::Result<RegistryNodeMetadata> {
        self.state.call(FaultPoint::DirectoryStat, &self.path)?;
        self.inner.fstat()
    }

    fn chmod(&mut self, mode: u32) -> io::Result<()> {
        self.state.call(FaultPoint::DirectoryChmod, &self.path)?;
        self.inner.chmod(mode)
    }

    fn create_exclusive(&self, name: &str, mode: u32) -> io::Result<Self::File> {
        let path = self.path.join(name);
        self.state.call(FaultPoint::TempCreate, &path)?;
        Ok(FaultFile {
            inner: self.inner.create_exclusive(name, mode)?,
            state: Rc::clone(&self.state),
            path,
        })
    }

    fn entry_metadata(&self, name: &str) -> io::Result<RegistryNodeMetadata> {
        let path = self.path.join(name);
        self.state.call(FaultPoint::EntryMetadata, &path)?;
        self.inner.entry_metadata(name)
    }

    fn read_entry(&self, name: &str) -> io::Result<Vec<u8>> {
        self.state
            .call(FaultPoint::EntryRead, &self.path.join(name))?;
        self.inner.read_entry(name)
    }

    fn rename_noreplace(&self, source: &str, target: &str) -> io::Result<()> {
        self.state
            .call(FaultPoint::Rename, &self.path.join(target))?;
        self.inner.rename_noreplace(source, target)
    }

    fn unlink(&self, name: &str) -> io::Result<()> {
        self.state.call(FaultPoint::Unlink, &self.path.join(name))?;
        self.inner.unlink(name)
    }

    fn sync(&self) -> io::Result<()> {
        self.state.call(FaultPoint::DirectorySync, &self.path)?;
        self.inner.sync()
    }
}

impl RegistryFile for FaultFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.state.call(FaultPoint::TempWrite, &self.path)?;
        self.inner.write_all(bytes)
    }

    fn chmod(&mut self, mode: u32) -> io::Result<()> {
        self.state.call(FaultPoint::TempChmod, &self.path)?;
        self.inner.chmod(mode)
    }

    fn fstat(&self) -> io::Result<RegistryNodeMetadata> {
        self.state.call(FaultPoint::TempStat, &self.path)?;
        self.inner.fstat()
    }

    fn sync(&self) -> io::Result<()> {
        self.state.call(FaultPoint::TempSync, &self.path)?;
        self.inner.sync()
    }

    fn close(self) -> io::Result<()> {
        self.state.call(FaultPoint::TempClose, &self.path)?;
        self.inner.close()
    }
}
