use std::{fmt, io};

#[cfg(target_os = "linux")]
use std::{fs, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

const MAX_PROCESS_IDENTITY_BYTES: usize = 512;
#[cfg(target_os = "linux")]
const LINUX_PROCESS_IDENTITY_SCHEME: &str = "linux-proc-v1";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProcessIdentity(String);

impl ProcessIdentity {
    pub fn new(scheme: &str, discriminator: &str) -> Result<Self, ProcessIdentityError> {
        if scheme.is_empty()
            || scheme.len() > 64
            || !scheme.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            return Err(ProcessIdentityError::new(
                "process identity scheme is invalid",
            ));
        }
        if discriminator.is_empty()
            || !discriminator.is_ascii()
            || !discriminator.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ProcessIdentityError::new(
                "process identity discriminator is invalid",
            ));
        }
        let length = scheme
            .len()
            .checked_add(1)
            .and_then(|value| value.checked_add(discriminator.len()))
            .ok_or_else(|| ProcessIdentityError::new("process identity is too long"))?;
        if length > MAX_PROCESS_IDENTITY_BYTES {
            return Err(ProcessIdentityError::new("process identity is too long"));
        }
        Ok(Self(format!("{scheme}:{discriminator}")))
    }

    pub fn parse(value: &str) -> Result<Self, ProcessIdentityError> {
        let (scheme, discriminator) = value
            .split_once(':')
            .ok_or_else(|| ProcessIdentityError::new("process identity has no scheme"))?;
        Self::new(scheme, discriminator)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn scheme(&self) -> &str {
        self.0
            .split_once(':')
            .expect("validated process identity")
            .0
    }

    pub fn discriminator(&self) -> &str {
        self.0
            .split_once(':')
            .expect("validated process identity")
            .1
    }
}

impl Serialize for ProcessIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProcessIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentityError(&'static str);

impl ProcessIdentityError {
    const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for ProcessIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ProcessIdentityError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservedProcessIdentity {
    Alive(ProcessIdentity),
    DefinitelyGone,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessIdentityClassification {
    Alive,
    DefinitelyGone,
    PidReused,
    Unknown,
}

impl ProcessIdentityClassification {
    pub const fn is_definitely_stale(self) -> bool {
        matches!(self, Self::DefinitelyGone | Self::PidReused)
    }
}

pub fn classify_process_identity(
    expected: &ProcessIdentity,
    observed: ObservedProcessIdentity,
) -> ProcessIdentityClassification {
    match observed {
        ObservedProcessIdentity::Alive(actual) if actual == *expected => {
            ProcessIdentityClassification::Alive
        }
        ObservedProcessIdentity::Alive(_) => ProcessIdentityClassification::PidReused,
        ObservedProcessIdentity::DefinitelyGone => ProcessIdentityClassification::DefinitelyGone,
        ObservedProcessIdentity::Unknown => ProcessIdentityClassification::Unknown,
    }
}

pub fn current_process_identity() -> io::Result<ProcessIdentity> {
    match observe_process_identity(std::process::id()) {
        ObservedProcessIdentity::Alive(identity) => Ok(identity),
        ObservedProcessIdentity::DefinitelyGone => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "current process disappeared while capturing its identity",
        )),
        ObservedProcessIdentity::Unknown => Err(io::Error::other(
            "current platform process identity is unavailable",
        )),
    }
}

#[cfg(target_os = "linux")]
pub fn observe_process_identity(pid: u32) -> ObservedProcessIdentity {
    let boot_id = match fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(value) => value,
        Err(_) => return ObservedProcessIdentity::Unknown,
    };
    let boot_id = boot_id.trim();
    if boot_id.is_empty()
        || !boot_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return ObservedProcessIdentity::Unknown;
    }

    let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
    let stat = match fs::read_to_string(stat_path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ObservedProcessIdentity::DefinitelyGone;
        }
        Err(_) => return ObservedProcessIdentity::Unknown,
    };
    let Some(start_ticks) = linux_start_ticks(&stat) else {
        return ObservedProcessIdentity::Unknown;
    };
    ProcessIdentity::new(
        LINUX_PROCESS_IDENTITY_SCHEME,
        &format!("{boot_id}:{start_ticks}"),
    )
    .map_or(
        ObservedProcessIdentity::Unknown,
        ObservedProcessIdentity::Alive,
    )
}

#[cfg(not(target_os = "linux"))]
pub fn observe_process_identity(_pid: u32) -> ObservedProcessIdentity {
    ObservedProcessIdentity::Unknown
}

#[cfg(target_os = "linux")]
fn linux_start_ticks(stat: &str) -> Option<&str> {
    let command_end = stat.rfind(')')?;
    let fields_after_command = stat.get(command_end + 1..)?.split_ascii_whitespace();
    let start_ticks = fields_after_command.into_iter().nth(19)?;
    if start_ticks.is_empty() || !start_ticks.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(start_ticks)
}
