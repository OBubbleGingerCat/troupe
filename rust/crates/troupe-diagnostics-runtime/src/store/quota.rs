use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};

#[derive(Clone)]
pub struct RunQuota {
    shared: Arc<SharedQuota>,
}

struct SharedQuota {
    run_directory: PathBuf,
    max_run_bytes: Option<u64>,
    state: Mutex<QuotaState>,
    failure_sender: SyncSender<QuotaFailure>,
}

#[derive(Default)]
struct QuotaState {
    current_measured_bytes: Option<u64>,
    last_measurement_at: Option<Duration>,
    failure: Option<QuotaFailure>,
}

impl RunQuota {
    pub fn new(
        run_directory: &Path,
        max_run_bytes: Option<u64>,
    ) -> Result<(Self, QuotaFailureReceiver), QuotaConfigurationError> {
        if !run_directory.is_absolute() {
            return Err(QuotaConfigurationError::RunDirectoryNotAbsolute);
        }
        if max_run_bytes == Some(0) {
            return Err(QuotaConfigurationError::LimitNotPositive);
        }
        let (failure_sender, receiver) = mpsc::sync_channel(1);
        Ok((
            Self {
                shared: Arc::new(SharedQuota {
                    run_directory: run_directory.to_path_buf(),
                    max_run_bytes,
                    state: Mutex::new(QuotaState::default()),
                    failure_sender,
                }),
            },
            QuotaFailureReceiver { receiver },
        ))
    }

    pub fn precheck(
        &self,
        now: Duration,
        conservative_growth_bytes: u64,
    ) -> Result<QuotaDecision, QuotaError> {
        let Some(limit) = self.shared.max_run_bytes else {
            return Ok(QuotaDecision::Disabled);
        };
        let mut state = self.lock_active_state()?;
        let measured = self.measure(&mut state, now, limit)?;

        if measured >= limit {
            return Err(self.limit_failure(
                &mut state,
                QuotaFailure::measured(limit, measured, &self.shared.run_directory),
            ));
        }
        let predicted = measured.checked_add(conservative_growth_bytes);
        if predicted.is_none_or(|bytes| bytes > limit) {
            return Err(self.limit_failure(
                &mut state,
                QuotaFailure::predicted(
                    limit,
                    measured,
                    conservative_growth_bytes,
                    &self.shared.run_directory,
                ),
            ));
        }
        Ok(QuotaDecision::WithinLimit {
            measured_bytes: measured,
        })
    }

    pub fn post_growth_measurement(&self, now: Duration) -> Result<QuotaDecision, QuotaError> {
        let Some(limit) = self.shared.max_run_bytes else {
            return Ok(QuotaDecision::Disabled);
        };
        let mut state = self.lock_active_state()?;
        let measured = self.measure(&mut state, now, limit)?;
        if measured >= limit {
            return Err(self.limit_failure(
                &mut state,
                QuotaFailure::measured(limit, measured, &self.shared.run_directory),
            ));
        }
        Ok(QuotaDecision::WithinLimit {
            measured_bytes: measured,
        })
    }

    pub fn status(&self) -> Result<QuotaStatus, QuotaStateError> {
        let state = self
            .shared
            .state
            .lock()
            .map_err(|_| QuotaStateError::StatePoisoned)?;
        Ok(QuotaStatus {
            max_run_bytes: self.shared.max_run_bytes,
            current_measured_bytes: state.current_measured_bytes,
            last_measurement_at: state.last_measurement_at,
            failure: state.failure.clone(),
        })
    }

    fn lock_active_state(&self) -> Result<std::sync::MutexGuard<'_, QuotaState>, QuotaError> {
        let state = self
            .shared
            .state
            .lock()
            .map_err(|_| QuotaError::StatePoisoned)?;
        if let Some(failure) = &state.failure {
            return Err(QuotaError::Sealed(failure.clone()));
        }
        Ok(state)
    }

    fn measure(
        &self,
        state: &mut QuotaState,
        now: Duration,
        limit: u64,
    ) -> Result<u64, QuotaError> {
        if state.last_measurement_at.is_some_and(|last| now < last) {
            let failure = QuotaFailure::measurement(
                limit,
                state.current_measured_bytes,
                &self.shared.run_directory,
            );
            return Err(self.measurement_failure(state, failure));
        }
        match measure_run_directory(&self.shared.run_directory) {
            Ok(measured) => {
                state.current_measured_bytes = Some(measured);
                state.last_measurement_at = Some(now);
                Ok(measured)
            }
            Err(error) => {
                let failure =
                    QuotaFailure::measurement(limit, state.current_measured_bytes, error.path());
                Err(self.measurement_failure(state, failure))
            }
        }
    }

    fn measurement_failure(&self, state: &mut QuotaState, failure: QuotaFailure) -> QuotaError {
        latch_failure(&self.shared, state, failure.clone());
        QuotaError::MeasurementFailed(failure)
    }

    fn limit_failure(&self, state: &mut QuotaState, failure: QuotaFailure) -> QuotaError {
        latch_failure(&self.shared, state, failure.clone());
        QuotaError::LimitReached(failure)
    }
}

fn latch_failure(shared: &SharedQuota, state: &mut QuotaState, failure: QuotaFailure) {
    if state.failure.is_some() {
        return;
    }
    state.failure = Some(failure.clone());
    match shared.failure_sender.try_send(failure) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

fn measure_run_directory(run_directory: &Path) -> Result<u64, QuotaMeasurementError> {
    let mut pending = vec![run_directory.to_path_buf()];
    let mut apparent_bytes = 0_u64;
    while let Some(path) = pending.pop() {
        let before =
            fs::symlink_metadata(&path).map_err(|error| QuotaMeasurementError::io(&path, error))?;
        let file_type = before.file_type();
        if file_type.is_symlink() {
            return Err(QuotaMeasurementError::logical(&path, "symlink"));
        }
        if file_type.is_file() {
            apparent_bytes = apparent_bytes
                .checked_add(before.len())
                .ok_or_else(|| QuotaMeasurementError::logical(&path, "size overflow"))?;
            continue;
        }
        if !file_type.is_dir() {
            return Err(QuotaMeasurementError::logical(&path, "special node"));
        }

        let mut children = fs::read_dir(&path)
            .map_err(|error| QuotaMeasurementError::io(&path, error))?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|error| QuotaMeasurementError::io(&path, error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        children.sort();

        let after =
            fs::symlink_metadata(&path).map_err(|error| QuotaMeasurementError::io(&path, error))?;
        if after.file_type().is_symlink()
            || !after.file_type().is_dir()
            || !same_file_identity(&before, &after)
        {
            return Err(QuotaMeasurementError::logical(
                &path,
                "directory identity changed",
            ));
        }
        pending.extend(children.into_iter().rev());
    }
    Ok(apparent_bytes)
}

#[cfg(unix)]
fn same_file_identity(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    first.dev() == second.dev() && first.ino() == second.ino()
}

#[cfg(not(unix))]
fn same_file_identity(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    first.file_type() == second.file_type()
        && first.permissions().readonly() == second.permissions().readonly()
}

#[derive(Debug)]
struct QuotaMeasurementError {
    path: PathBuf,
    _source: io::Error,
}

impl QuotaMeasurementError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            _source: source,
        }
    }

    fn logical(path: &Path, message: &'static str) -> Self {
        Self::io(path, io::Error::new(io::ErrorKind::InvalidData, message))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaDecision {
    Disabled,
    WithinLimit { measured_bytes: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaFailureCode {
    PredictedLimitExceeded,
    MeasuredLimitReached,
    MeasurementFailed,
}

impl QuotaFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PredictedLimitExceeded => "run_quota_predicted_limit_exceeded",
            Self::MeasuredLimitReached => "run_quota_measured_limit_reached",
            Self::MeasurementFailed => "run_quota_measurement_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaFailure {
    code: QuotaFailureCode,
    limit_bytes: u64,
    current_bytes: Option<u64>,
    predicted_growth_bytes: Option<u64>,
    path: Option<PathBuf>,
}

impl QuotaFailure {
    fn predicted(limit: u64, current: u64, growth: u64, path: &Path) -> Self {
        Self {
            code: QuotaFailureCode::PredictedLimitExceeded,
            limit_bytes: limit,
            current_bytes: Some(current),
            predicted_growth_bytes: Some(growth),
            path: Some(path.to_path_buf()),
        }
    }

    fn measured(limit: u64, current: u64, path: &Path) -> Self {
        Self {
            code: QuotaFailureCode::MeasuredLimitReached,
            limit_bytes: limit,
            current_bytes: Some(current),
            predicted_growth_bytes: None,
            path: Some(path.to_path_buf()),
        }
    }

    fn measurement(limit: u64, current: Option<u64>, path: &Path) -> Self {
        Self {
            code: QuotaFailureCode::MeasurementFailed,
            limit_bytes: limit,
            current_bytes: current,
            predicted_growth_bytes: None,
            path: Some(path.to_path_buf()),
        }
    }

    pub const fn code(&self) -> QuotaFailureCode {
        self.code
    }

    pub const fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    pub const fn current_bytes(&self) -> Option<u64> {
        self.current_bytes
    }

    pub const fn predicted_growth_bytes(&self) -> Option<u64> {
        self.predicted_growth_bytes
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

pub struct QuotaFailureReceiver {
    receiver: Receiver<QuotaFailure>,
}

impl QuotaFailureReceiver {
    pub fn try_recv(&self) -> Option<QuotaFailure> {
        self.receiver.try_recv().ok()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuotaError {
    StatePoisoned,
    Sealed(QuotaFailure),
    LimitReached(QuotaFailure),
    MeasurementFailed(QuotaFailure),
}

impl fmt::Display for QuotaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StatePoisoned => formatter.write_str("Run quota state is poisoned"),
            Self::Sealed(failure) => write!(
                formatter,
                "Run quota is sealed [{}]",
                failure.code().as_str()
            ),
            Self::LimitReached(failure) | Self::MeasurementFailed(failure) => {
                write!(formatter, "Run quota failed [{}]", failure.code().as_str())
            }
        }
    }
}

impl std::error::Error for QuotaError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaConfigurationError {
    RunDirectoryNotAbsolute,
    LimitNotPositive,
}

impl fmt::Display for QuotaConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunDirectoryNotAbsolute => {
                formatter.write_str("Run quota directory must be absolute")
            }
            Self::LimitNotPositive => formatter.write_str("Run quota limit must be positive"),
        }
    }
}

impl std::error::Error for QuotaConfigurationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaStatus {
    max_run_bytes: Option<u64>,
    current_measured_bytes: Option<u64>,
    last_measurement_at: Option<Duration>,
    failure: Option<QuotaFailure>,
}

impl QuotaStatus {
    pub const fn max_run_bytes(&self) -> Option<u64> {
        self.max_run_bytes
    }

    pub const fn current_measured_bytes(&self) -> Option<u64> {
        self.current_measured_bytes
    }

    pub const fn last_measurement_at(&self) -> Option<Duration> {
        self.last_measurement_at
    }

    pub const fn sealed(&self) -> bool {
        self.failure.is_some()
    }

    pub const fn failure(&self) -> Option<&QuotaFailure> {
        self.failure.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaStateError {
    StatePoisoned,
}

impl fmt::Display for QuotaStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Run quota state is poisoned")
    }
}

impl std::error::Error for QuotaStateError {}
