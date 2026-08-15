use std::{collections::BTreeMap, fmt, net::IpAddr, time::Duration};

use hyper::Uri;
use serde::Serialize;
use troupe_diagnostics_core::{
    event::EVENT_SCHEMA_VERSION,
    id::CanonicalUuid,
    scalar::SchemaU64,
    view_protocol::{
        API_SCHEMA_VERSION, MAX_METRIC_SERIES, MAX_PAGE_ROWS, MAX_TIME_SERIES_POINTS,
        MAX_TIME_SERIES_SERIES, VIEW_SCHEMA_VERSION,
    },
};

use crate::{
    registry::{
        model::{BindEndpoint, SERVER_PROTOCOL_VERSION, SecurityScope, WebBaseUrl},
        process_identity::ProcessIdentity,
    },
    store::{
        admission::{MAX_UNCOMMITTED_CANONICAL_BYTES, MAX_UNCOMMITTED_EVENTS},
        batch::{MAX_BATCH_AGE, MAX_BATCH_CANONICAL_BYTES, MAX_BATCH_EVENTS},
        progress::{DEFAULT_SHUTDOWN_DRAIN_TIMEOUT, DEFAULT_WRITER_STALL_TIMEOUT},
    },
};

pub const IDENTITY_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct OperationalLimits(BTreeMap<String, SchemaU64>);

impl OperationalLimits {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn with_limit(mut self, name: &str, value: u64) -> Result<Self, IdentityError> {
        self.set_limit(name, value)?;
        Ok(self)
    }

    pub fn set_limit(&mut self, name: &str, value: u64) -> Result<(), IdentityError> {
        if !valid_limit_name(name) {
            return Err(IdentityError::new("operational limit name is invalid"));
        }
        self.0.insert(name.to_owned(), SchemaU64::new(value));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<u64> {
        self.0.get(name).map(|value| value.get())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.get()))
    }
}

impl Default for OperationalLimits {
    fn default() -> Self {
        let mut value = Self::new();
        for (name, limit) in [
            ("max_uncommitted_events", MAX_UNCOMMITTED_EVENTS as u64),
            (
                "max_uncommitted_canonical_bytes",
                MAX_UNCOMMITTED_CANONICAL_BYTES as u64,
            ),
            ("max_batch_age_ms", duration_millis(MAX_BATCH_AGE)),
            ("max_batch_events", MAX_BATCH_EVENTS as u64),
            (
                "max_batch_canonical_bytes",
                MAX_BATCH_CANONICAL_BYTES as u64,
            ),
            (
                "writer_stall_timeout_ms",
                duration_millis(DEFAULT_WRITER_STALL_TIMEOUT),
            ),
            (
                "shutdown_drain_timeout_ms",
                duration_millis(DEFAULT_SHUTDOWN_DRAIN_TIMEOUT),
            ),
            ("max_page_rows", u64::from(MAX_PAGE_ROWS)),
            ("max_metric_series", u64::from(MAX_METRIC_SERIES)),
            (
                "max_time_series_points",
                u64::from(MAX_TIME_SERIES_POINTS),
            ),
            (
                "max_time_series_series",
                u64::from(MAX_TIME_SERIES_SERIES),
            ),
        ] {
            value
                .set_limit(name, limit)
                .expect("built-in operational limit names are valid");
        }
        value
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ServerIdentity {
    identity_schema_version: u8,
    server_protocol_version: u16,
    event_schema_version: u8,
    view_schema_version: u8,
    api_schema_version: u8,
    run_id: CanonicalUuid,
    owner_pid: u32,
    process_identity: ProcessIdentity,
    bind_host: String,
    port: u16,
    local_endpoint: WebBaseUrl,
    advertise_url: Option<WebBaseUrl>,
    base_path: String,
    api_base_path: String,
    identity_path: String,
    security_scope: SecurityScope,
    operational_limits: OperationalLimits,
}

impl ServerIdentity {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        run_id: CanonicalUuid,
        owner_pid: u32,
        process_identity: ProcessIdentity,
        bind: BindEndpoint,
        advertise_url: Option<WebBaseUrl>,
        operational_limits: OperationalLimits,
    ) -> Result<Self, IdentityError> {
        if owner_pid == 0 {
            return Err(IdentityError::new("owner PID must be nonzero"));
        }
        let local_endpoint = derive_local_endpoint(&bind)?;
        let base_path = normalized_base_path(advertise_url.as_ref())?;
        let api_base_path = join_base_path(&base_path, "/api/v1");
        let identity_path = join_base_path(&base_path, "/api/v1/identity");
        Ok(Self {
            identity_schema_version: IDENTITY_SCHEMA_VERSION,
            server_protocol_version: SERVER_PROTOCOL_VERSION,
            event_schema_version: EVENT_SCHEMA_VERSION,
            view_schema_version: VIEW_SCHEMA_VERSION,
            api_schema_version: API_SCHEMA_VERSION,
            run_id,
            owner_pid,
            process_identity,
            bind_host: bind.host().to_owned(),
            port: bind.port(),
            local_endpoint,
            advertise_url,
            base_path,
            api_base_path,
            identity_path,
            security_scope: SecurityScope::TrustedNetwork,
            operational_limits,
        })
    }

    pub const fn identity_schema_version(&self) -> u8 {
        self.identity_schema_version
    }

    pub const fn server_protocol_version(&self) -> u16 {
        self.server_protocol_version
    }

    pub const fn event_schema_version(&self) -> u8 {
        self.event_schema_version
    }

    pub const fn view_schema_version(&self) -> u8 {
        self.view_schema_version
    }

    pub const fn api_schema_version(&self) -> u8 {
        self.api_schema_version
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn owner_pid(&self) -> u32 {
        self.owner_pid
    }

    pub const fn process_identity(&self) -> &ProcessIdentity {
        &self.process_identity
    }

    pub fn bind_host(&self) -> &str {
        &self.bind_host
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    pub const fn local_endpoint(&self) -> &WebBaseUrl {
        &self.local_endpoint
    }

    pub const fn advertise_url(&self) -> Option<&WebBaseUrl> {
        self.advertise_url.as_ref()
    }

    pub fn base_path(&self) -> &str {
        &self.base_path
    }

    pub fn api_base_path(&self) -> &str {
        &self.api_base_path
    }

    pub fn identity_path(&self) -> &str {
        &self.identity_path
    }

    pub const fn security_scope(&self) -> SecurityScope {
        self.security_scope
    }

    pub const fn operational_limits(&self) -> &OperationalLimits {
        &self.operational_limits
    }

    pub(crate) fn encoded(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityError(&'static str);

impl IdentityError {
    const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for IdentityError {}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("built-in duration fits u64 milliseconds")
}

fn valid_limit_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
        })
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_lowercase)
}

fn derive_local_endpoint(bind: &BindEndpoint) -> Result<WebBaseUrl, IdentityError> {
    let host = match bind.host().parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) if address.is_unspecified() => "127.0.0.1".to_owned(),
        Ok(IpAddr::V6(address)) if address.is_unspecified() => "[::1]".to_owned(),
        Ok(IpAddr::V6(address)) => format!("[{address}]"),
        Ok(address) => address.to_string(),
        Err(_) => bind.host().to_owned(),
    };
    WebBaseUrl::parse(&format!("http://{host}:{}/", bind.port()))
        .map_err(|_| IdentityError::new("local endpoint is invalid"))
}

fn normalized_base_path(advertise_url: Option<&WebBaseUrl>) -> Result<String, IdentityError> {
    let Some(advertise_url) = advertise_url else {
        return Ok("/".to_owned());
    };
    let uri = advertise_url
        .as_str()
        .parse::<Uri>()
        .map_err(|_| IdentityError::new("advertise URL is invalid"))?;
    let path = uri.path();
    if path == "/" || path.is_empty() {
        return Ok("/".to_owned());
    }
    if path.contains("//") {
        return Err(IdentityError::new(
            "advertise URL base path is not normalized",
        ));
    }
    let path = path.strip_suffix('/').unwrap_or(path);
    if path.is_empty()
        || path
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(IdentityError::new("advertise URL base path is not normalized"));
    }
    Ok(path.to_owned())
}

pub(crate) fn join_base_path(base_path: &str, relative_path: &str) -> String {
    if base_path == "/" {
        relative_path.to_owned()
    } else {
        format!("{base_path}{relative_path}")
    }
}
