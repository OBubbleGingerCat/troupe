use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use troupe_diagnostics_core::id::CanonicalUuid;

#[path = "../src/archive/layout.rs"]
mod layout;
#[path = "../src/archive/probe.rs"]
mod probe;

use layout::{ArchiveLayout, ArchiveStartupErrorCode};
use probe::{
    ArchiveDirectory, ArchiveFileSystem, ArchiveProbeFile, NodeMetadata, RealDirectory,
    RealFileSystem, RealProbeFile, WRITE_PROBE_PREFIX,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const DIRECTORY_MODE: u32 = 0o700;
static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);
static UMASK_LOCK: Mutex<()> = Mutex::new(());

unsafe extern "C" {
    fn umask(mask: u32) -> u32;
}

struct UmaskGuard(u32);

impl UmaskGuard {
    fn set(mask: u32) -> Self {
        // SAFETY: the process-global mutation is serialized by UMASK_LOCK and restored by Drop.
        Self(unsafe { umask(mask) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: this restores the value captured by the serialized set call.
        unsafe {
            umask(self.0);
        }
    }
}

struct TestProductionRoot(PathBuf);

impl TestProductionRoot {
    fn new() -> Self {
        let sequence = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-s00-archive-layout-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test production root");
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

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("inspect mode")
        .permissions()
        .mode()
        & 0o7777
}

fn set_mode(path: &Path, value: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(value)).expect("set test mode");
}

fn archive_paths(root: &Path) -> [PathBuf; 5] {
    let troupe = root.join(".troupe");
    let diagnostics = troupe.join("diagnostics");
    [
        troupe,
        diagnostics.clone(),
        diagnostics.join("instances"),
        diagnostics.join("runs"),
        diagnostics.join("runs").join(RUN_ID),
    ]
}

fn create_existing_archive(root: &Path) {
    let paths = archive_paths(root);
    for path in &paths[..4] {
        fs::create_dir(path).expect("create existing archive directory");
        set_mode(path, 0o777);
    }
}

#[test]
fn creates_only_the_fixed_layout_and_real_probe_under_umask_zero() {
    let _lock = UMASK_LOCK.lock().expect("lock umask mutation");
    let root = TestProductionRoot::new();
    set_mode(root.path(), 0o777);
    let _umask = UmaskGuard::set(0);

    let layout = ArchiveLayout::prepare(root.path(), run_id()).expect("prepare archive layout");
    let expected = archive_paths(root.path());

    assert_eq!(layout.production_root(), root.path());
    assert_eq!(layout.troupe_directory(), expected[0]);
    assert_eq!(layout.diagnostics_directory(), expected[1]);
    assert_eq!(layout.instances_directory(), expected[2]);
    assert_eq!(layout.runs_directory(), expected[3]);
    assert_eq!(layout.run_directory(), expected[4]);
    assert_eq!(layout.run_id(), run_id());
    assert_eq!(
        mode(root.path()),
        0o777,
        "the caller-owned root is untouched"
    );
    for path in &expected {
        assert_eq!(
            mode(path),
            DIRECTORY_MODE,
            "wrong mode for {}",
            path.display()
        );
    }
    let probe_name = format!("{WRITE_PROBE_PREFIX}{}", run_id());
    assert!(!layout.troupe_directory().join(probe_name).exists());
}

#[test]
fn preserves_existing_state_root_mode_and_tightens_owned_archive_modes() {
    let root = TestProductionRoot::new();
    create_existing_archive(root.path());

    let layout = ArchiveLayout::prepare(root.path(), run_id()).expect("prepare existing archive");

    let paths = archive_paths(root.path());
    assert_eq!(
        mode(&paths[0]),
        0o777,
        "the existing state-root mode is outside S00's exact policy"
    );
    for path in &paths[1..] {
        assert_eq!(
            mode(path),
            DIRECTORY_MODE,
            "wrong mode for {}",
            path.display()
        );
    }
    assert!(layout.run_directory().is_dir());
}

#[test]
fn rejects_symlinks_and_regular_file_placeholders() {
    let root = TestProductionRoot::new();
    let symlink_target = root.path().join("symlink-target");
    fs::create_dir(&symlink_target).expect("create symlink target");
    symlink(&symlink_target, root.path().join(".troupe")).expect("create state-root symlink");

    let error = ArchiveLayout::prepare(root.path(), run_id()).expect_err("reject symlink");
    assert_eq!(error.code(), ArchiveStartupErrorCode::SymlinkRejected);
    assert_eq!(error.path(), root.path().join(".troupe"));

    for relative in [
        ".troupe",
        ".troupe/diagnostics",
        ".troupe/diagnostics/instances",
        ".troupe/diagnostics/runs",
    ] {
        let root = TestProductionRoot::new();
        let placeholder = root.path().join(relative);
        fs::create_dir_all(placeholder.parent().expect("placeholder parent"))
            .expect("create placeholder parents");
        fs::write(&placeholder, b"not a directory").expect("create regular file placeholder");

        let error = ArchiveLayout::prepare(root.path(), run_id())
            .expect_err("reject regular file placeholder");
        assert_eq!(error.code(), ArchiveStartupErrorCode::NotDirectory);
        assert_eq!(error.path(), placeholder);
    }
}

#[test]
fn rejects_existing_run_identity_without_deleting_it() {
    let root = TestProductionRoot::new();
    create_existing_archive(root.path());
    let run_directory = archive_paths(root.path())[4].clone();
    fs::create_dir(&run_directory).expect("create colliding Run directory");
    let marker = run_directory.join("existing-archive");
    fs::write(&marker, b"keep").expect("write existing archive marker");

    let error = ArchiveLayout::prepare(root.path(), run_id()).expect_err("reject Run collision");

    assert_eq!(error.code(), ArchiveStartupErrorCode::RunIdentityCollision);
    assert_eq!(error.path(), run_directory);
    assert_eq!(fs::read(marker).expect("read preserved marker"), b"keep");
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultPoint {
    Mkdir,
    Chmod,
    Fstat,
    ProbeCreate,
    Write,
    Fsync,
    Unlink,
    IdentityMismatch,
    ModeMismatch,
}

struct FaultState {
    point: FaultPoint,
    kind: io::ErrorKind,
    fired: Cell<bool>,
    paths: RefCell<Vec<PathBuf>>,
}

impl FaultState {
    fn record(&self, path: &Path) {
        self.paths.borrow_mut().push(path.to_path_buf());
    }

    fn fail(&self, point: FaultPoint) -> io::Result<()> {
        if self.point == point && !self.fired.replace(true) {
            return Err(io::Error::new(self.kind, format!("injected {point:?}")));
        }
        Ok(())
    }
}

struct FaultFileSystem {
    inner: RealFileSystem,
    state: Rc<FaultState>,
}

impl FaultFileSystem {
    fn new(point: FaultPoint) -> Self {
        Self::with_kind(point, io::ErrorKind::Other)
    }

    fn with_kind(point: FaultPoint, kind: io::ErrorKind) -> Self {
        Self {
            inner: RealFileSystem,
            state: Rc::new(FaultState {
                point,
                kind,
                fired: Cell::new(false),
                paths: RefCell::new(Vec::new()),
            }),
        }
    }

    fn assert_confined_to(&self, root: &Path) {
        let paths = self.state.paths.borrow();
        assert!(
            !paths.is_empty(),
            "fault adapter observed no filesystem operations"
        );
        assert!(
            paths.iter().all(|path| path.starts_with(root)),
            "filesystem operation escaped production root: {paths:?}"
        );
    }
}

struct FaultDirectory {
    inner: RealDirectory,
    path: PathBuf,
    state: Rc<FaultState>,
    chmod_called: Cell<bool>,
}

impl ArchiveDirectory for FaultDirectory {
    fn chmod(&mut self, mode: u32) -> io::Result<()> {
        self.state.record(&self.path);
        self.state.fail(FaultPoint::Chmod)?;
        self.inner.chmod(mode)?;
        self.chmod_called.set(true);
        Ok(())
    }

    fn fstat(&self) -> io::Result<NodeMetadata> {
        self.state.record(&self.path);
        self.state.fail(FaultPoint::Fstat)?;
        let mut metadata = self.inner.fstat()?;
        if self.state.point == FaultPoint::IdentityMismatch && !self.state.fired.replace(true) {
            metadata.inode ^= 1;
        }
        if self.state.point == FaultPoint::ModeMismatch
            && self.chmod_called.get()
            && !self.state.fired.replace(true)
        {
            metadata.mode = 0o777;
        }
        Ok(metadata)
    }
}

struct FaultProbeFile {
    inner: RealProbeFile,
    path: PathBuf,
    state: Rc<FaultState>,
}

impl ArchiveProbeFile for FaultProbeFile {
    fn write_all(&mut self, value: &[u8]) -> io::Result<()> {
        self.state.record(&self.path);
        self.state.fail(FaultPoint::Write)?;
        self.inner.write_all(value)
    }

    fn fsync(&self) -> io::Result<()> {
        self.state.record(&self.path);
        self.state.fail(FaultPoint::Fsync)?;
        self.inner.fsync()
    }

    fn close(self) -> io::Result<()> {
        self.inner.close()
    }
}

impl ArchiveFileSystem for FaultFileSystem {
    type Directory = FaultDirectory;
    type ProbeFile = FaultProbeFile;

    fn path_metadata(&self, path: &Path) -> io::Result<NodeMetadata> {
        self.state.record(path);
        self.inner.path_metadata(path)
    }

    fn mkdir(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.state.record(path);
        self.state.fail(FaultPoint::Mkdir)?;
        self.inner.mkdir(path, mode)
    }

    fn open_directory(&self, path: &Path) -> io::Result<Self::Directory> {
        self.state.record(path);
        Ok(FaultDirectory {
            inner: self.inner.open_directory(path)?,
            path: path.to_path_buf(),
            state: Rc::clone(&self.state),
            chmod_called: Cell::new(false),
        })
    }

    fn create_probe(&self, path: &Path, mode: u32) -> io::Result<Self::ProbeFile> {
        self.state.record(path);
        self.state.fail(FaultPoint::ProbeCreate)?;
        Ok(FaultProbeFile {
            inner: self.inner.create_probe(path, mode)?,
            path: path.to_path_buf(),
            state: Rc::clone(&self.state),
        })
    }

    fn unlink(&self, path: &Path) -> io::Result<()> {
        self.state.record(path);
        self.state.fail(FaultPoint::Unlink)?;
        self.inner.unlink(path)
    }

    fn remove_directory(&self, path: &Path) -> io::Result<()> {
        self.state.record(path);
        self.inner.remove_directory(path)
    }
}

#[test]
fn injected_filesystem_failures_are_stable_and_never_fall_back() {
    for (point, code) in [
        (
            FaultPoint::Mkdir,
            ArchiveStartupErrorCode::DirectoryCreateFailed,
        ),
        (
            FaultPoint::Chmod,
            ArchiveStartupErrorCode::DirectoryPermissionFailed,
        ),
        (
            FaultPoint::Fstat,
            ArchiveStartupErrorCode::DirectoryInspectFailed,
        ),
        (
            FaultPoint::ProbeCreate,
            ArchiveStartupErrorCode::ProbeCreateFailed,
        ),
        (FaultPoint::Write, ArchiveStartupErrorCode::ProbeWriteFailed),
        (FaultPoint::Fsync, ArchiveStartupErrorCode::ProbeSyncFailed),
        (
            FaultPoint::Unlink,
            ArchiveStartupErrorCode::ProbeUnlinkFailed,
        ),
        (
            FaultPoint::ModeMismatch,
            ArchiveStartupErrorCode::DirectoryModeMismatch,
        ),
    ] {
        let root = TestProductionRoot::new();
        let filesystem = FaultFileSystem::new(point);

        let error = ArchiveLayout::prepare_with(&filesystem, root.path(), run_id())
            .expect_err("injected operation must fail startup");

        assert_eq!(error.code(), code, "wrong stable code for {point:?}");
        assert!(
            filesystem.state.fired.get(),
            "fault did not fire: {point:?}"
        );
        filesystem.assert_confined_to(root.path());
        assert_eq!(
            error.to_string(),
            format!(
                "diagnostic archive startup failed [{}] at {}",
                code.as_str(),
                error.path().display()
            )
        );
    }
}

#[test]
fn relative_production_root_is_rejected_before_filesystem_access() {
    let filesystem = FaultFileSystem::new(FaultPoint::Mkdir);

    let error = ArchiveLayout::prepare_with(&filesystem, Path::new("relative"), run_id())
        .expect_err("relative production root must not resolve through cwd");

    assert_eq!(error.code(), ArchiveStartupErrorCode::InvalidProductionRoot);
    assert!(filesystem.state.paths.borrow().is_empty());
    assert!(!filesystem.state.fired.get());
}

#[test]
fn write_probe_targets_state_root_and_cleans_a_new_empty_root_on_failure() {
    let root = TestProductionRoot::new();
    let filesystem = FaultFileSystem::new(FaultPoint::ProbeCreate);
    let expected_probe = root
        .path()
        .join(".troupe")
        .join(format!("{WRITE_PROBE_PREFIX}{}", run_id()));

    let error = ArchiveLayout::prepare_with(&filesystem, root.path(), run_id())
        .expect_err("state-root probe failure must fail startup");

    assert_eq!(error.code(), ArchiveStartupErrorCode::ProbeCreateFailed);
    assert_eq!(error.path(), expected_probe);
    assert!(
        !root.path().join(".troupe").exists(),
        "a newly created empty state root should be removed after probe failure"
    );
}

#[test]
fn read_only_and_identity_mismatch_fail_closed() {
    let root = TestProductionRoot::new();
    create_existing_archive(root.path());
    let read_only =
        FaultFileSystem::with_kind(FaultPoint::ProbeCreate, io::ErrorKind::PermissionDenied);
    let error = ArchiveLayout::prepare_with(&read_only, root.path(), run_id())
        .expect_err("read-only filesystem must fail");
    assert_eq!(error.code(), ArchiveStartupErrorCode::ProbeCreateFailed);
    assert_eq!(error.source_kind(), io::ErrorKind::PermissionDenied);
    assert!(
        !archive_paths(root.path())[4].exists(),
        "new empty Run directory should be cleaned after probe-create failure"
    );
    read_only.assert_confined_to(root.path());

    let root = TestProductionRoot::new();
    let collision = FaultFileSystem::new(FaultPoint::IdentityMismatch);
    let error = ArchiveLayout::prepare_with(&collision, root.path(), run_id())
        .expect_err("path/fstat identity mismatch must fail");
    assert_eq!(
        error.code(),
        ArchiveStartupErrorCode::DirectoryIdentityChanged
    );
    collision.assert_confined_to(root.path());
}

#[test]
fn probe_failure_cleanup_preserves_an_existing_archive() {
    let root = TestProductionRoot::new();
    create_existing_archive(root.path());
    let marker = root
        .path()
        .join(".troupe/diagnostics/runs/existing-archive.marker");
    fs::write(&marker, b"preserve").expect("write existing archive marker");
    let filesystem = FaultFileSystem::new(FaultPoint::Write);

    let error = ArchiveLayout::prepare_with(&filesystem, root.path(), run_id())
        .expect_err("probe write failure must fail startup");

    assert_eq!(error.code(), ArchiveStartupErrorCode::ProbeWriteFailed);
    assert_eq!(fs::read(marker).expect("read existing marker"), b"preserve");
    assert!(
        !archive_paths(root.path())[4].exists(),
        "only the newly created Run directory should be cleaned"
    );
    for path in &archive_paths(root.path())[..4] {
        assert!(path.is_dir(), "existing archive directory was removed");
    }
}
