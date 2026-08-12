use std::ffi::CString;
use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyString};

use crate::fork_fd_registry::ForkTracked;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AgentKind {
    Codex,
    Claude,
    Kimi,
}

impl AgentKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "kimi" => Some(Self::Kimi),
            _ => None,
        }
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kimi => "kimi",
        }
    }
}

fn required_string(value: &Bound<'_, PyAny>, field: &str) -> PyResult<String> {
    let value = value
        .cast::<PyString>()
        .map_err(|_| PyTypeError::new_err(format!("{field} must be a str")))?;
    if value.len()? == 0 {
        return Err(PyValueError::new_err(format!("{field} must not be empty")));
    }
    let value = value
        .to_str()
        .map_err(|_| PyValueError::new_err(format!("{field} must be valid Unicode")))?;
    Ok(value.to_owned())
}

pub(crate) struct WorkspaceLeaseV1 {
    pub(crate) canonical_path: PathBuf,
    pub(crate) owner_pid: u32,
    pub(crate) st_dev: u64,
    pub(crate) st_ino: u64,
    pub(crate) directory: ForkTracked<OwnedFd>,
    pub(crate) acp_cwd_alias: PathBuf,
}

pub(crate) struct ResolvedAgentProfile {
    pub(crate) agent: AgentKind,
    pub(crate) workspace: WorkspaceLeaseV1,
    pub(crate) requested_model: String,
    pub(crate) requested_effort: Option<String>,
}

fn workspace_error(path: &Path, error: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(format!(
        "workspace is not an accessible directory '{}': {error}",
        path.display()
    ))
}

pub(crate) fn resolve_agent_profile(profile: &Bound<'_, PyAny>) -> PyResult<ResolvedAgentProfile> {
    let py = profile.py();
    let profile_type = py.import("troupe")?.getattr("AgentProfile")?;
    if !profile.is_instance(&profile_type)? {
        return Err(PyTypeError::new_err(
            "agent_profile must be an AgentProfile",
        ));
    }
    let agent_value = profile.getattr("agent")?;
    let agent_name = required_string(&agent_value, "agent")?;
    let agent = AgentKind::parse(&agent_name)
        .ok_or_else(|| PyValueError::new_err("agent must be one of: 'codex', 'claude', 'kimi'"))?;
    let model_value = profile.getattr("model")?;
    let requested_model = required_string(&model_value, "model")?;
    let effort_value = profile.getattr("effort")?;
    let requested_effort = if effort_value.is_none() {
        None
    } else {
        Some(required_string(&effort_value, "effort")?)
    };
    let workspace = profile.getattr("workspace")?;
    let value = py.import("os")?.getattr("fspath")?.call1((workspace,))?;
    let value = value.cast::<PyString>().map_err(|_| {
        PyTypeError::new_err("agent_profile.workspace must resolve to str, not bytes")
    })?;
    let supplied: String = value
        .extract()
        .map_err(|_| PyValueError::new_err("agent_profile.workspace must be valid Unicode"))?;
    if supplied.is_empty() {
        return Err(PyValueError::new_err(
            "agent_profile.workspace must not be empty",
        ));
    }
    if supplied.contains('\0') {
        return Err(PyValueError::new_err(
            "agent_profile.workspace must not contain NUL",
        ));
    }
    let supplied = PathBuf::from(supplied);
    if !supplied.is_absolute() {
        return Err(PyValueError::new_err(
            "agent_profile.workspace must be an absolute path",
        ));
    }

    let canonical_path =
        std::fs::canonicalize(&supplied).map_err(|error| workspace_error(&supplied, error))?;
    if canonical_path.to_str().is_none() {
        return Err(PyValueError::new_err(
            "agent_profile.workspace canonical path must be valid Unicode",
        ));
    }
    let access_path = CString::new(canonical_path.as_os_str().as_bytes())
        .expect("a canonical path cannot contain NUL");
    if unsafe { libc::access(access_path.as_ptr(), libc::R_OK | libc::X_OK) } != 0 {
        return Err(workspace_error(
            &canonical_path,
            std::io::Error::last_os_error(),
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(&canonical_path)
        .map_err(|error| workspace_error(&canonical_path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| workspace_error(&canonical_path, error))?;
    if !metadata.is_dir() {
        return Err(PyValueError::new_err(
            "agent_profile.workspace must identify a directory",
        ));
    }
    let owner_pid = std::process::id();
    let raw_fd = file.as_raw_fd();
    let acp_cwd_alias = PathBuf::from(format!("/proc/{owner_pid}/fd/{raw_fd}"));
    let directory: OwnedFd = file.into();
    let directory = ForkTracked::new(directory);

    Ok(ResolvedAgentProfile {
        agent,
        workspace: WorkspaceLeaseV1 {
            canonical_path,
            owner_pid,
            st_dev: metadata.dev(),
            st_ino: metadata.ino(),
            directory,
            acp_cwd_alias,
        },
        requested_model,
        requested_effort,
    })
}
