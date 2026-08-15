use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use troupe_diagnostics_core::id::CanonicalUuid;

use super::{
    model::{
        DecodedRegistryEntry, REGISTRY_SCHEMA_VERSION, RegistryEntry, SERVER_PROTOCOL_VERSION,
        SecurityScope, WebBaseUrl,
    },
    process_identity::ProcessIdentity,
};

#[derive(Deserialize)]
struct RegistryVersionEnvelope {
    registry_schema_version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryEntryWire {
    registry_schema_version: u64,
    server_protocol_version: u64,
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

pub fn encode_registry_entry(entry: &RegistryEntry) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(entry)
}

pub fn decode_registry_entry(
    path: &Path,
    bytes: &[u8],
) -> Result<RegistryEntry, RegistryCodecError> {
    let envelope: RegistryVersionEnvelope = serde_json::from_slice(bytes).map_err(|error| {
        RegistryCodecError::new(
            path,
            RegistryCodecErrorCode::InvalidEntry,
            None,
            error.to_string(),
        )
    })?;
    if envelope.registry_schema_version > u64::from(REGISTRY_SCHEMA_VERSION) {
        return Err(RegistryCodecError::new(
            path,
            RegistryCodecErrorCode::NewerSchema,
            Some(envelope.registry_schema_version),
            "registry schema is newer than this client".to_owned(),
        ));
    }
    if envelope.registry_schema_version != u64::from(REGISTRY_SCHEMA_VERSION) {
        return Err(RegistryCodecError::new(
            path,
            RegistryCodecErrorCode::UnsupportedSchema,
            Some(envelope.registry_schema_version),
            "registry schema is unsupported".to_owned(),
        ));
    }

    let wire: RegistryEntryWire = serde_json::from_slice(bytes).map_err(|error| {
        RegistryCodecError::new(
            path,
            RegistryCodecErrorCode::InvalidEntry,
            None,
            error.to_string(),
        )
    })?;
    if wire.registry_schema_version != u64::from(REGISTRY_SCHEMA_VERSION) {
        return Err(RegistryCodecError::new(
            path,
            RegistryCodecErrorCode::UnsupportedSchema,
            Some(wire.registry_schema_version),
            "registry schema changed during decode".to_owned(),
        ));
    }
    if wire.server_protocol_version != u64::from(SERVER_PROTOCOL_VERSION) {
        return Err(RegistryCodecError::new(
            path,
            RegistryCodecErrorCode::UnsupportedServerProtocol,
            Some(wire.registry_schema_version),
            "server protocol is unsupported".to_owned(),
        ));
    }

    RegistryEntry::from_decoded(DecodedRegistryEntry {
        run_id: wire.run_id,
        archive_directory: wire.archive_directory,
        owner_pid: wire.owner_pid,
        process_identity: wire.process_identity,
        bind_host: wire.bind_host,
        port: wire.port,
        local_endpoint: wire.local_endpoint,
        advertise_url: wire.advertise_url,
        security_scope: wire.security_scope,
        started_at: wire.started_at,
    })
    .map_err(|error| {
        RegistryCodecError::new(
            path,
            RegistryCodecErrorCode::InvalidEntry,
            Some(wire.registry_schema_version),
            error.to_string(),
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryCodecErrorCode {
    InvalidEntry,
    NewerSchema,
    UnsupportedSchema,
    UnsupportedServerProtocol,
}

#[derive(Debug)]
pub struct RegistryCodecError {
    path: PathBuf,
    code: RegistryCodecErrorCode,
    observed_schema_version: Option<u64>,
    detail: String,
}

impl RegistryCodecError {
    fn new(
        path: &Path,
        code: RegistryCodecErrorCode,
        observed_schema_version: Option<u64>,
        detail: String,
    ) -> Self {
        Self {
            path: path.to_path_buf(),
            code,
            observed_schema_version,
            detail,
        }
    }

    pub const fn code(&self) -> RegistryCodecErrorCode {
        self.code
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn observed_schema_version(&self) -> Option<u64> {
        self.observed_schema_version
    }
}

impl fmt::Display for RegistryCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic registry entry at {} is invalid: {}",
            self.path.display(),
            self.detail
        )
    }
}

impl std::error::Error for RegistryCodecError {}
