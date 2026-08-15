use std::{
    fs,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use troupe_diagnostics_runtime::store::quota::{
    QuotaDecision, QuotaError, QuotaFailureCode, RunQuota,
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "troupe-store-quota-{label}-{}-{}",
            process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, bytes: usize) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, vec![b'x'; bytes]).unwrap();
        path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn unset_quota_performs_no_measurement_and_never_prunes() {
    let missing = std::env::temp_dir().join(format!(
        "troupe-store-quota-unset-missing-{}-{}",
        process::id(),
        uuid::Uuid::new_v4()
    ));
    let (quota, failures) = RunQuota::new(&missing, None).unwrap();

    assert_eq!(
        quota.precheck(Duration::ZERO, u64::MAX).unwrap(),
        QuotaDecision::Disabled
    );
    assert_eq!(
        quota
            .post_growth_measurement(Duration::from_secs(1))
            .unwrap(),
        QuotaDecision::Disabled
    );
    let status = quota.status().unwrap();
    assert_eq!(status.max_run_bytes(), None);
    assert_eq!(status.current_measured_bytes(), None);
    assert_eq!(status.last_measurement_at(), None);
    assert!(!status.sealed());
    assert!(failures.try_recv().is_none());

    let run = TestDirectory::new("unset-retention");
    let retained = run.write("events.sqlite3", 16);
    let (quota, _) = RunQuota::new(run.path(), None).unwrap();
    quota.precheck(Duration::ZERO, u64::MAX).unwrap();
    assert!(retained.is_file());
    assert_eq!(fs::metadata(retained).unwrap().len(), 16);
}

#[test]
fn prediction_allows_equality_but_post_write_equality_is_fatal_without_retention() {
    let run = TestDirectory::new("exact-boundary");
    let database = run.write("events.sqlite3", 9);
    let (quota, failures) = RunQuota::new(run.path(), Some(10)).unwrap();

    assert_eq!(
        quota.precheck(Duration::from_secs(1), 1).unwrap(),
        QuotaDecision::WithinLimit { measured_bytes: 9 }
    );
    fs::write(&database, vec![b'x'; 10]).unwrap();
    let error = quota
        .post_growth_measurement(Duration::from_secs(2))
        .unwrap_err();
    assert!(matches!(
        error,
        QuotaError::LimitReached(failure)
            if failure.code() == QuotaFailureCode::MeasuredLimitReached
    ));
    let failure = failures.try_recv().unwrap();
    assert_eq!(failure.code(), QuotaFailureCode::MeasuredLimitReached);
    assert_eq!(failure.code().as_str(), "run_quota_measured_limit_reached");
    assert_eq!(failure.current_bytes(), Some(10));
    assert_eq!(failure.limit_bytes(), 10);
    assert!(failures.try_recv().is_none());

    let status = quota.status().unwrap();
    assert_eq!(status.current_measured_bytes(), Some(10));
    assert_eq!(status.last_measurement_at(), Some(Duration::from_secs(2)));
    assert!(status.sealed());
    assert!(database.is_file());
    assert_eq!(fs::metadata(database).unwrap().len(), 10);
}

#[test]
fn conservative_prediction_overflow_seals_once_before_any_write() {
    let run = TestDirectory::new("prediction");
    let database = run.write("events.sqlite3", 9);
    let (quota, failures) = RunQuota::new(run.path(), Some(10)).unwrap();

    let error = quota.precheck(Duration::from_secs(3), 2).unwrap_err();
    assert!(matches!(
        error,
        QuotaError::LimitReached(failure)
            if failure.code() == QuotaFailureCode::PredictedLimitExceeded
    ));
    let failure = failures.try_recv().unwrap();
    assert_eq!(failure.code(), QuotaFailureCode::PredictedLimitExceeded);
    assert_eq!(failure.current_bytes(), Some(9));
    assert_eq!(failure.predicted_growth_bytes(), Some(2));
    assert_eq!(failure.limit_bytes(), 10);
    assert_eq!(fs::metadata(&database).unwrap().len(), 9);

    assert!(matches!(
        quota.precheck(Duration::from_secs(4), 0),
        Err(QuotaError::Sealed(existing)) if existing == failure
    ));
    assert!(failures.try_recv().is_none());
    assert_eq!(fs::metadata(database).unwrap().len(), 9);
}

#[test]
fn measurement_recurses_over_database_wal_shm_and_owned_metadata() {
    let run = TestDirectory::new("wal-growth");
    run.write("events.sqlite3", 11);
    run.write("events.sqlite3-wal", 13);
    run.write("events.sqlite3-shm", 17);
    run.write("metadata/configuration.json", 19);
    fs::create_dir_all(run.path().join("empty")).unwrap();
    let (quota, failures) = RunQuota::new(run.path(), Some(1_000)).unwrap();

    assert_eq!(
        quota
            .post_growth_measurement(Duration::from_millis(25))
            .unwrap(),
        QuotaDecision::WithinLimit { measured_bytes: 60 }
    );
    let status = quota.status().unwrap();
    assert_eq!(status.current_measured_bytes(), Some(60));
    assert_eq!(
        status.last_measurement_at(),
        Some(Duration::from_millis(25))
    );
    assert_eq!(status.max_run_bytes(), Some(1_000));
    assert!(!status.sealed());
    assert!(failures.try_recv().is_none());
}

#[cfg(unix)]
#[test]
fn symlink_is_rejected_without_following_and_measurement_errors_fail_closed() {
    use std::os::unix::fs::symlink;

    let outside = TestDirectory::new("outside");
    let target = outside.write("secret", 128);
    let run = TestDirectory::new("symlink");
    symlink(&target, run.path().join("linked")).unwrap();
    let (quota, failures) = RunQuota::new(run.path(), Some(1_000)).unwrap();

    let error = quota.post_growth_measurement(Duration::ZERO).unwrap_err();
    assert!(matches!(
        error,
        QuotaError::MeasurementFailed(failure)
            if failure.code() == QuotaFailureCode::MeasurementFailed
                && failure.path() == Some(run.path().join("linked").as_path())
    ));
    assert_eq!(
        failures.try_recv().unwrap().code(),
        QuotaFailureCode::MeasurementFailed
    );
    assert_eq!(fs::metadata(target).unwrap().len(), 128);

    let missing = std::env::temp_dir().join(format!(
        "troupe-store-quota-missing-{}-{}",
        process::id(),
        uuid::Uuid::new_v4()
    ));
    let (quota, failures) = RunQuota::new(&missing, Some(10)).unwrap();
    assert!(matches!(
        quota.precheck(Duration::ZERO, 1),
        Err(QuotaError::MeasurementFailed(_))
    ));
    assert!(failures.try_recv().is_some());
    assert!(quota.status().unwrap().sealed());
}

#[test]
fn concurrent_run_directories_are_accounted_independently() {
    let first = TestDirectory::new("run-a");
    let second = TestDirectory::new("run-b");
    first.write("events.sqlite3", 7);
    second.write("events.sqlite3", 23);
    let (first_quota, first_failures) = RunQuota::new(first.path(), Some(10)).unwrap();
    let (second_quota, second_failures) = RunQuota::new(second.path(), Some(100)).unwrap();

    assert!(matches!(
        first_quota.precheck(Duration::ZERO, 4),
        Err(QuotaError::LimitReached(_))
    ));
    assert_eq!(
        second_quota.precheck(Duration::ZERO, 4).unwrap(),
        QuotaDecision::WithinLimit { measured_bytes: 23 }
    );
    assert!(first_failures.try_recv().is_some());
    assert!(second_failures.try_recv().is_none());
    assert!(first_quota.status().unwrap().sealed());
    assert!(!second_quota.status().unwrap().sealed());
}
