use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Component, Path, PathBuf},
};

use hyper::Uri;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use troupe_diagnostics_core::id::CanonicalUuid;

use super::process_identity::ProcessIdentity;

pub const REGISTRY_SCHEMA_VERSION: u16 = 1;
pub const SERVER_PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SecurityScope {
    #[serde(rename = "trusted_network")]
    TrustedNetwork,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindEndpoint {
    host: String,
    port: u16,
}

impl BindEndpoint {
    pub fn new(host: &str, port: u16) -> Result<Self, RegistryModelError> {
        if port == 0 {
            return Err(RegistryModelError::new(
                RegistryModelErrorCode::InvalidPort,
                "published listener port must be nonzero",
            ));
        }
        let host = normalize_bind_host(host)?;
        Ok(Self { host, port })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WebBaseUrl(String);

impl WebBaseUrl {
    pub fn parse(value: &str) -> Result<Self, RegistryModelError> {
        if value.is_empty()
            || value.contains('#')
            || value.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(RegistryModelError::invalid_url());
        }
        let uri: Uri = value
            .parse()
            .map_err(|_| RegistryModelError::invalid_url())?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.authority().is_none()
            || uri.host().is_none()
            || uri.query().is_some()
            || uri
                .authority()
                .is_some_and(|authority| authority.as_str().contains('@'))
        {
            return Err(RegistryModelError::invalid_url());
        }
        if uri
            .path()
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
        {
            return Err(RegistryModelError::invalid_url());
        }
        let authority_offset = value
            .find("://")
            .expect("absolute HTTP(S) URI has a scheme")
            + 3;
        let normalized = if !value[authority_offset..].contains('/') {
            format!("{value}/")
        } else {
            value.to_owned()
        };
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for WebBaseUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WebBaseUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RegistryEntry {
    registry_schema_version: u16,
    server_protocol_version: u16,
    run_id: CanonicalUuid,
    archive_directory: String,
    owner_pid: u32,
    process_identity: ProcessIdentity,
    bind_host: String,
    port: u16,
    local_endpoint: WebBaseUrl,
    advertise_url: Option<WebBaseUrl>,
    security_scope: SecurityScope,
    started_at: String,
}

impl RegistryEntry {
    pub fn new(
        run_id: CanonicalUuid,
        run_directory: &Path,
        owner_pid: u32,
        process_identity: ProcessIdentity,
        bind: BindEndpoint,
        advertise_url: Option<WebBaseUrl>,
        started_at: &str,
    ) -> Result<Self, RegistryModelError> {
        let archive_directory = validate_run_directory(run_id, run_directory)?;
        if owner_pid == 0 {
            return Err(RegistryModelError::new(
                RegistryModelErrorCode::InvalidPid,
                "owner PID must be nonzero",
            ));
        }
        validate_started_at(started_at)?;
        let local_endpoint = derive_local_endpoint(&bind)?;
        Ok(Self {
            registry_schema_version: REGISTRY_SCHEMA_VERSION,
            server_protocol_version: SERVER_PROTOCOL_VERSION,
            run_id,
            archive_directory,
            owner_pid,
            process_identity,
            bind_host: bind.host,
            port: bind.port,
            local_endpoint,
            advertise_url,
            security_scope: SecurityScope::TrustedNetwork,
            started_at: started_at.to_owned(),
        })
    }

    pub(crate) fn from_decoded(fields: DecodedRegistryEntry) -> Result<Self, RegistryModelError> {
        let entry = Self::new(
            fields.run_id,
            Path::new(&fields.archive_directory),
            fields.owner_pid,
            fields.process_identity,
            BindEndpoint::new(&fields.bind_host, fields.port)?,
            fields.advertise_url,
            &fields.started_at,
        )?;
        if fields.local_endpoint != entry.local_endpoint {
            return Err(RegistryModelError::new(
                RegistryModelErrorCode::LocalEndpointMismatch,
                "local endpoint does not match the published bind endpoint",
            ));
        }
        if fields.security_scope != SecurityScope::TrustedNetwork {
            return Err(RegistryModelError::new(
                RegistryModelErrorCode::InvalidSecurityScope,
                "registry security scope is unsupported",
            ));
        }
        Ok(entry)
    }

    pub const fn registry_schema_version(&self) -> u16 {
        self.registry_schema_version
    }

    pub const fn server_protocol_version(&self) -> u16 {
        self.server_protocol_version
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn run_directory(&self) -> PathBuf {
        PathBuf::from(&self.archive_directory)
    }

    pub const fn owner_pid(&self) -> u32 {
        self.owner_pid
    }

    pub const fn process_identity(&self) -> &ProcessIdentity {
        &self.process_identity
    }

    pub fn bind(&self) -> BindEndpoint {
        BindEndpoint {
            host: self.bind_host.clone(),
            port: self.port,
        }
    }

    pub const fn local_endpoint(&self) -> &WebBaseUrl {
        &self.local_endpoint
    }

    pub const fn advertise_url(&self) -> Option<&WebBaseUrl> {
        self.advertise_url.as_ref()
    }

    pub const fn security_scope(&self) -> SecurityScope {
        self.security_scope
    }

    pub fn started_at(&self) -> &str {
        &self.started_at
    }
}

pub(crate) struct DecodedRegistryEntry {
    pub run_id: CanonicalUuid,
    pub archive_directory: String,
    pub owner_pid: u32,
    pub process_identity: ProcessIdentity,
    pub bind_host: String,
    pub port: u16,
    pub local_endpoint: WebBaseUrl,
    pub advertise_url: Option<WebBaseUrl>,
    pub security_scope: SecurityScope,
    pub started_at: String,
}

fn normalize_bind_host(host: &str) -> Result<String, RegistryModelError> {
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty()
        || !host.is_ascii()
        || host.bytes().any(|byte| byte.is_ascii_whitespace())
        || host
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b'@'))
    {
        return Err(RegistryModelError::new(
            RegistryModelErrorCode::InvalidBindHost,
            "bind host is invalid",
        ));
    }
    if host.parse::<IpAddr>().is_err() {
        let probe = format!("http://{host}:1/");
        let uri = probe.parse::<Uri>().map_err(|_| {
            RegistryModelError::new(
                RegistryModelErrorCode::InvalidBindHost,
                "bind host is invalid",
            )
        })?;
        if uri.host() != Some(host) {
            return Err(RegistryModelError::new(
                RegistryModelErrorCode::InvalidBindHost,
                "bind host is invalid",
            ));
        }
    }
    Ok(host.to_owned())
}

fn derive_local_endpoint(bind: &BindEndpoint) -> Result<WebBaseUrl, RegistryModelError> {
    let host = match bind.host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) if address.is_unspecified() => {
            IpAddr::V4(Ipv4Addr::LOCALHOST).to_string()
        }
        Ok(IpAddr::V6(address)) if address.is_unspecified() => {
            format!("[{}]", Ipv6Addr::LOCALHOST)
        }
        Ok(IpAddr::V6(address)) => format!("[{address}]"),
        Ok(address) => address.to_string(),
        Err(_) => bind.host.clone(),
    };
    WebBaseUrl::parse(&format!("http://{host}:{}/", bind.port))
}

fn validate_run_directory(
    run_id: CanonicalUuid,
    run_directory: &Path,
) -> Result<String, RegistryModelError> {
    if !run_directory.is_absolute()
        || run_directory
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(RegistryModelError::new(
            RegistryModelErrorCode::InvalidRunDirectory,
            "Run directory must be an absolute normalized path",
        ));
    }
    let expected_name = run_id.to_string();
    if run_directory.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return Err(RegistryModelError::new(
            RegistryModelErrorCode::StoreIdentityMismatch,
            "Run directory does not match the Run identity",
        ));
    }
    run_directory.to_str().map(str::to_owned).ok_or_else(|| {
        RegistryModelError::new(
            RegistryModelErrorCode::InvalidRunDirectory,
            "Run directory is not valid UTF-8",
        )
    })
}

fn validate_started_at(value: &str) -> Result<(), RegistryModelError> {
    if value.is_empty()
        || value.len() > 128
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || !value.contains('T')
    {
        return Err(RegistryModelError::new(
            RegistryModelErrorCode::InvalidStartedAt,
            "started_at is invalid",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryModelErrorCode {
    InvalidBindHost,
    InvalidPort,
    InvalidUrl,
    InvalidRunDirectory,
    StoreIdentityMismatch,
    InvalidPid,
    InvalidStartedAt,
    LocalEndpointMismatch,
    InvalidSecurityScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistryModelError {
    code: RegistryModelErrorCode,
    message: &'static str,
}

impl RegistryModelError {
    const fn new(code: RegistryModelErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    const fn invalid_url() -> Self {
        Self::new(
            RegistryModelErrorCode::InvalidUrl,
            "URL must be an absolute HTTP(S) base URL without userinfo, query, or fragment",
        )
    }

    pub const fn code(self) -> RegistryModelErrorCode {
        self.code
    }
}

impl fmt::Display for RegistryModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for RegistryModelError {}
