use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime},
};

use fs4::TryLockError;
use troupe_diagnostics_runtime::archive::{
    constants::{
        ARCHIVE_LEASE_ANCHOR_FILENAME, ARCHIVE_LEASE_ANCHOR_MODE, ARCHIVE_LEASE_LOCK_PRIMITIVE,
    },
    lease::{
        ActiveArchiveLease, ActiveArchiveLeaseGuard, ArchiveLeaseErrorCode, ArchiveLeaseHandle,
        ArchiveLeaseMode, ArchiveLeaseOpener, CleanupArchiveLease, LeaseAnchorMetadata,
        RealArchiveLeaseOpener, SharedArchiveLease,
    },
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-s05-{label}-{}-{nonce}-{counter}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn exact_anchor_and_locking_primitive_are_frozen() {
    assert_eq!(ARCHIVE_LEASE_ANCHOR_FILENAME, "diagnostics.lease");
    assert_eq!(
        ARCHIVE_LEASE_LOCK_PRIMITIVE,
        "fs4::FileExt(flock/LockFileEx)"
    );
    assert_eq!(ARCHIVE_LEASE_ANCHOR_MODE, 0o600);
}

#[test]
fn active_exclusive_conflicts_and_drop_releases_for_inactive_readers() {
    let run = TestDirectory::new("active");
    let active = ActiveArchiveLease::acquire(run.path()).unwrap();
    assert!(active.anchor_path().is_file());
    assert_eq!(active.guard().anchor_path(), active.anchor_path());

    for error in [
        ActiveArchiveLease::acquire(run.path()).unwrap_err(),
        SharedArchiveLease::acquire(run.path()).unwrap_err(),
        CleanupArchiveLease::acquire(run.path()).unwrap_err(),
    ] {
        assert_eq!(error.code(), ArchiveLeaseErrorCode::Contended);
    }
    drop(active);

    let first = SharedArchiveLease::acquire(run.path()).unwrap();
    let second = SharedArchiveLease::acquire(run.path()).unwrap();
    assert_eq!(first.anchor_path(), second.anchor_path());
    assert_eq!(
        CleanupArchiveLease::acquire(run.path()).unwrap_err().code(),
        ArchiveLeaseErrorCode::Contended
    );
    drop((first, second));

    let cleanup = CleanupArchiveLease::acquire(run.path()).unwrap();
    assert_eq!(
        cleanup.anchor_path(),
        run.path().join(ARCHIVE_LEASE_ANCHOR_FILENAME)
    );
}

#[test]
fn shared_and_cleanup_require_an_existing_anchor() {
    let run = TestDirectory::new("missing");
    for error in [
        SharedArchiveLease::acquire(run.path()).unwrap_err(),
        CleanupArchiveLease::acquire(run.path()).unwrap_err(),
    ] {
        assert_eq!(error.code(), ArchiveLeaseErrorCode::AnchorOpenFailed);
        assert_eq!(error.source_kind(), Some(io::ErrorKind::NotFound));
    }
    ActiveArchiveLease::acquire(run.path()).unwrap();
    assert!(run.path().join(ARCHIVE_LEASE_ANCHOR_FILENAME).is_file());
}

#[cfg(unix)]
#[test]
fn anchor_is_private_and_symlinks_are_rejected() {
    use std::os::unix::fs::{MetadataExt, symlink};

    let run = TestDirectory::new("symlink");
    let active = ActiveArchiveLease::acquire(run.path()).unwrap();
    let anchor = active.anchor_path().to_path_buf();
    assert_eq!(fs::metadata(&anchor).unwrap().mode() & 0o777, 0o600);
    drop(active);
    fs::remove_file(&anchor).unwrap();
    let target = run.path().join("target");
    File::create(&target).unwrap();
    symlink(&target, &anchor).unwrap();

    let error = ActiveArchiveLease::acquire(run.path()).unwrap_err();
    assert_eq!(error.code(), ArchiveLeaseErrorCode::AnchorSymlinkRejected);
}

#[test]
fn lock_errors_are_not_treated_as_unlocked() {
    let run = TestDirectory::new("injected");
    ActiveArchiveLease::acquire(run.path()).unwrap();
    let opener = InjectedOpener {
        lock_error: io::ErrorKind::PermissionDenied,
    };
    let error = SharedArchiveLease::acquire_with(&opener, run.path()).unwrap_err();
    assert_eq!(error.code(), ArchiveLeaseErrorCode::LockFailed);
    assert_eq!(error.mode(), ArchiveLeaseMode::SharedReader);
    assert_eq!(error.source_kind(), Some(io::ErrorKind::PermissionDenied));
}

#[test]
fn a_static_archive_copy_has_independent_lock_state() {
    let source = TestDirectory::new("copy-source");
    let destination = TestDirectory::new("copy-destination");
    let active = ActiveArchiveLease::acquire(source.path()).unwrap();
    let destination_anchor = destination.path().join(ARCHIVE_LEASE_ANCHOR_FILENAME);
    fs::copy(active.anchor_path(), &destination_anchor).unwrap();

    let copied_reader = SharedArchiveLease::acquire(destination.path()).unwrap();
    assert_eq!(copied_reader.anchor_path(), destination_anchor);
    assert_eq!(
        SharedArchiveLease::acquire(source.path())
            .unwrap_err()
            .code(),
        ArchiveLeaseErrorCode::Contended
    );
}

#[test]
fn active_guard_is_a_borrowed_non_clone_capability() {
    let current = std::env::current_exe().unwrap();
    let deps = current.parent().unwrap();
    let runtime_rlib = fs::read_dir(deps)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("libtroupe_diagnostics_runtime-") && name.ends_with(".rlib")
                })
        })
        .max_by_key(|path| path.metadata().and_then(|value| value.modified()).unwrap())
        .unwrap();
    let scratch = TestDirectory::new("compile");
    let source = scratch.path().join("guard.rs");
    fs::write(
        &source,
        r#"
use troupe_diagnostics_runtime::archive::lease::ActiveArchiveLeaseGuard;
fn needs_clone<T: Clone>() {}
fn main() { needs_clone::<ActiveArchiveLeaseGuard<'static>>(); }
"#,
    )
    .unwrap();
    let output = Command::new("rustc")
        .arg("--edition=2024")
        .arg(&source)
        .arg("--extern")
        .arg(format!(
            "troupe_diagnostics_runtime={}",
            runtime_rlib.display()
        ))
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .arg("--out-dir")
        .arg(scratch.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("Clone"));

    fn consumes_borrow(_guard: ActiveArchiveLeaseGuard<'_>) {}
    let run = TestDirectory::new("guard-runtime");
    let active = ActiveArchiveLease::acquire(run.path()).unwrap();
    consumes_borrow(active.guard());
    assert_eq!(
        active.anchor_path(),
        run.path().join(ARCHIVE_LEASE_ANCHOR_FILENAME)
    );
}

#[test]
fn child_process_contention_and_crash_release_are_real() {
    let run = TestDirectory::new("child");
    let ready = run.path().join("child-ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("child_process_holds_active_lease")
        .arg("--nocapture")
        .env("TROUPE_S05_CHILD_RUN", run.path())
        .env("TROUPE_S05_CHILD_READY", &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.is_file() {
        assert!(
            Instant::now() < deadline,
            "child did not acquire the active lease"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        SharedArchiveLease::acquire(run.path()).unwrap_err().code(),
        ArchiveLeaseErrorCode::Contended
    );
    child.kill().unwrap();
    child.wait().unwrap();

    SharedArchiveLease::acquire(run.path()).unwrap();
}

#[test]
fn child_process_holds_active_lease() {
    let Some(run) = std::env::var_os("TROUPE_S05_CHILD_RUN") else {
        return;
    };
    let ready = PathBuf::from(std::env::var_os("TROUPE_S05_CHILD_READY").unwrap());
    let _lease = ActiveArchiveLease::acquire(Path::new(&run)).unwrap();
    fs::write(ready, b"ready").unwrap();
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

struct InjectedOpener {
    lock_error: io::ErrorKind,
}

impl ArchiveLeaseOpener for InjectedOpener {
    fn open(&self, path: &Path, create_new: bool) -> io::Result<Box<dyn ArchiveLeaseHandle>> {
        let real = RealArchiveLeaseOpener.open(path, create_new)?;
        Ok(Box::new(InjectedHandle {
            inner: real,
            lock_error: self.lock_error,
        }))
    }
}

struct InjectedHandle {
    inner: Box<dyn ArchiveLeaseHandle>,
    lock_error: io::ErrorKind,
}

impl ArchiveLeaseHandle for InjectedHandle {
    fn metadata(&self) -> io::Result<LeaseAnchorMetadata> {
        self.inner.metadata()
    }

    fn set_owner_only(&self) -> io::Result<()> {
        self.inner.set_owner_only()
    }

    fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(io::Error::from(self.lock_error)))
    }

    fn try_lock_exclusive(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(io::Error::from(self.lock_error)))
    }
}
