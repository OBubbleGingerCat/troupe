use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, Metadata},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use hyper::Uri;
use serde::Deserialize;
use troupe_diagnostics_core::{event::EVENT_SCHEMA_VERSION, id::CanonicalUuid, scalar::SchemaU64};

use crate::{
    archive::lease::{ArchiveLeaseErrorCode, SharedArchiveLease},
    store::{
        connection::open_immutable_read_only,
        schema::{DIAGNOSTIC_DATABASE_FILENAME, STORE_SCHEMA_IDENTITY, STORE_SCHEMA_VERSION},
    },
};

use super::{
    codec::{RegistryCodecErrorCode, decode_registry_entry},
    model::{RegistryEntry, SERVER_PROTOCOL_VERSION, SecurityScope, WebBaseUrl},
    process_identity::{
        ObservedProcessIdentity, ProcessIdentityClassification, classify_process_identity,
        observe_process_identity,
    },
};

const MAX_REGISTRY_ENTRY_BYTES: u64 = 1024 * 1024;
const MAX_SERVER_IDENTITY_BYTES: usize = 1024 * 1024;
const SERVER_IDENTITY_SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CandidateClassification {
    Active,
    DefiniteStale,
    Unhealthy,
    IdentityMismatch,
    Invalid,
    Incompatible,
    Completed,
    Incomplete,
}

impl CandidateClassification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::DefiniteStale => "definite_stale",
            Self::Unhealthy => "unhealthy",
            Self::IdentityMismatch => "identity_mismatch",
            Self::Invalid => "invalid",
            Self::Incompatible => "incompatible",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateSource {
    Instance,
    Archive,
}

#[derive(Clone, Debug)]
pub struct DiscoveryCandidate {
    classification: CandidateClassification,
    source: CandidateSource,
    path: PathBuf,
    run_id: Option<CanonicalUuid>,
    path_run_id: Option<CanonicalUuid>,
    archive_directory: Option<PathBuf>,
    archive_present: bool,
    registry_entry: Option<RegistryEntry>,
    detail: Option<String>,
    registry_snapshot: Option<RegistrySnapshot>,
}

impl DiscoveryCandidate {
    pub const fn classification(&self) -> CandidateClassification {
        self.classification
    }

    pub const fn source(&self) -> CandidateSource {
        self.source
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the Run identity only when the candidate payload has authenticated it.
    pub const fn run_id(&self) -> Option<CanonicalUuid> {
        self.run_id
    }

    /// Returns a canonical Run identity encoded in the namespace path, even if the payload is
    /// invalid or incompatible and therefore cannot be trusted.
    pub const fn path_run_id(&self) -> Option<CanonicalUuid> {
        self.path_run_id
    }

    pub fn archive_directory(&self) -> Option<&Path> {
        self.archive_directory.as_deref()
    }

    pub const fn archive_present(&self) -> bool {
        self.archive_present
    }

    pub const fn registry_entry(&self) -> Option<&RegistryEntry> {
        self.registry_entry.as_ref()
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub const fn is_instance(&self) -> bool {
        matches!(self.source, CandidateSource::Instance)
    }

    pub fn is_potentially_live(&self) -> bool {
        self.is_instance() && self.classification != CandidateClassification::DefiniteStale
    }

    pub(crate) const fn registry_snapshot(&self) -> Option<&RegistrySnapshot> {
        self.registry_snapshot.as_ref()
    }

    pub(crate) fn revalidated(
        &self,
        classification: CandidateClassification,
        entry: RegistryEntry,
        snapshot: RegistrySnapshot,
        detail: Option<String>,
    ) -> Self {
        Self {
            classification,
            source: self.source,
            path: self.path.clone(),
            run_id: Some(entry.run_id()),
            path_run_id: self.path_run_id,
            archive_directory: self.archive_directory.clone(),
            archive_present: self.archive_present,
            registry_entry: Some(entry),
            detail,
            registry_snapshot: Some(snapshot),
        }
    }
}

pub trait ProcessIdentityProbe {
    fn observe(&self, pid: u32) -> ObservedProcessIdentity;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealProcessIdentityProbe;

impl ProcessIdentityProbe for RealProcessIdentityProbe {
    fn observe(&self, pid: u32) -> ObservedProcessIdentity {
        observe_process_identity(pid)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerProbeError {
    detail: String,
}

impl ServerProbeError {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ServerProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ServerProbeError {}

/// Fetches the byte body from the entry's server-owned `/identity` route.
///
/// HTTP transport is deliberately supplied by the caller. This module owns strict identity
/// decoding and classification, while the server/client transport remains outside registry
/// discovery.
pub trait ServerIdentityProbe {
    fn probe_identity(&self, entry: &RegistryEntry) -> Result<Vec<u8>, ServerProbeError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryErrorCode {
    InvalidProductionRoot,
    NamespaceInspectFailed,
    NamespaceNotDirectory,
    NamespaceChanged,
    NamespaceReadFailed,
}

impl DiscoveryErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidProductionRoot => "registry_discovery.invalid_production_root",
            Self::NamespaceInspectFailed => "registry_discovery.namespace_inspect_failed",
            Self::NamespaceNotDirectory => "registry_discovery.namespace_not_directory",
            Self::NamespaceChanged => "registry_discovery.namespace_changed",
            Self::NamespaceReadFailed => "registry_discovery.namespace_read_failed",
        }
    }
}

#[derive(Debug)]
pub struct DiscoveryError {
    code: DiscoveryErrorCode,
    path: PathBuf,
    source: Option<io::Error>,
}

impl DiscoveryError {
    fn logical(code: DiscoveryErrorCode, path: &Path) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            source: None,
        }
    }

    fn io(code: DiscoveryErrorCode, path: &Path, source: io::Error) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            source: Some(source),
        }
    }

    pub const fn code(&self) -> DiscoveryErrorCode {
        self.code
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source_kind(&self) -> Option<io::ErrorKind> {
        self.source.as_ref().map(io::Error::kind)
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic discovery failed [{}] at {}",
            self.code.as_str(),
            self.path.display()
        )
    }
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

pub fn discover(
    production_root: &Path,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<Vec<DiscoveryCandidate>, DiscoveryError> {
    discover_with(production_root, &RealProcessIdentityProbe, server_probe)
}

pub fn discover_with(
    production_root: &Path,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<Vec<DiscoveryCandidate>, DiscoveryError> {
    validate_production_root(production_root)?;
    let diagnostics_directory = production_root.join(".troupe/diagnostics");
    let instances_directory = diagnostics_directory.join("instances");
    let runs_directory = diagnostics_directory.join("runs");

    let mut candidates = Vec::new();
    let mut archives = scan_archives(&runs_directory, &mut candidates)?;
    for instance_path in scan_namespace(&instances_directory)?
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
    {
        candidates.push(classify_instance(
            &instance_path,
            &runs_directory,
            &mut archives,
            process_probe,
            server_probe,
        ));
    }
    candidates.extend(archives.into_values());
    candidates.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.classification.cmp(&right.classification))
    });
    Ok(candidates)
}

fn validate_production_root(production_root: &Path) -> Result<(), DiscoveryError> {
    if !production_root.is_absolute()
        || production_root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(DiscoveryError::logical(
            DiscoveryErrorCode::InvalidProductionRoot,
            production_root,
        ));
    }
    Ok(())
}

fn scan_archives(
    runs_directory: &Path,
    untrusted: &mut Vec<DiscoveryCandidate>,
) -> Result<BTreeMap<CanonicalUuid, DiscoveryCandidate>, DiscoveryError> {
    let mut archives = BTreeMap::new();
    for path in scan_namespace(runs_directory)? {
        let path_run_id = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| CanonicalUuid::parse(name).ok());
        let Some(path_run_id) = path_run_id else {
            untrusted.push(untrusted_candidate(
                CandidateClassification::Invalid,
                CandidateSource::Archive,
                path,
                None,
                None,
                false,
                "Run archive directory name is not a canonical UUID".to_owned(),
            ));
            continue;
        };

        let (classification, trusted, detail) = classify_archive(&path, path_run_id);
        archives.insert(
            path_run_id,
            DiscoveryCandidate {
                classification,
                source: CandidateSource::Archive,
                path: path.clone(),
                run_id: trusted.then_some(path_run_id),
                path_run_id: Some(path_run_id),
                archive_directory: Some(path),
                archive_present: true,
                registry_entry: None,
                detail,
                registry_snapshot: None,
            },
        );
    }
    Ok(archives)
}

fn classify_instance(
    path: &Path,
    runs_directory: &Path,
    archives: &mut BTreeMap<CanonicalUuid, DiscoveryCandidate>,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> DiscoveryCandidate {
    let path_run_id = registry_path_run_id(path);
    let Some(path_run_id) = path_run_id else {
        return untrusted_candidate(
            CandidateClassification::Invalid,
            CandidateSource::Instance,
            path.to_path_buf(),
            None,
            None,
            false,
            "instance filename is not <canonical-run-id>.json".to_owned(),
        );
    };
    let archive = archives.remove(&path_run_id);
    let archive_directory = archive
        .as_ref()
        .and_then(DiscoveryCandidate::archive_directory)
        .map(Path::to_path_buf);
    let archive_present = archive.is_some();

    let snapshot = match read_registry_snapshot(path) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return untrusted_candidate(
                CandidateClassification::Invalid,
                CandidateSource::Instance,
                path.to_path_buf(),
                Some(path_run_id),
                archive_directory,
                archive_present,
                error.to_string(),
            );
        }
    };
    let entry = match decode_registry_entry(path, snapshot.bytes()) {
        Ok(entry) => entry,
        Err(error) => {
            let classification = match error.code() {
                RegistryCodecErrorCode::NewerSchema
                | RegistryCodecErrorCode::UnsupportedServerProtocol => {
                    CandidateClassification::Incompatible
                }
                RegistryCodecErrorCode::InvalidEntry
                | RegistryCodecErrorCode::UnsupportedSchema => CandidateClassification::Invalid,
            };
            return untrusted_candidate(
                classification,
                CandidateSource::Instance,
                path.to_path_buf(),
                Some(path_run_id),
                archive_directory,
                archive_present,
                error.to_string(),
            );
        }
    };
    let expected_archive_directory = runs_directory.join(path_run_id.to_string());
    if entry.run_id() != path_run_id || entry.run_directory() != expected_archive_directory {
        return untrusted_candidate(
            CandidateClassification::Invalid,
            CandidateSource::Instance,
            path.to_path_buf(),
            Some(path_run_id),
            archive_directory,
            archive_present,
            "instance filename, Run identity, and fixed archive path do not agree".to_owned(),
        );
    }

    let (classification, detail) = classify_registry_entry(&entry, process_probe, server_probe);
    DiscoveryCandidate {
        classification,
        source: CandidateSource::Instance,
        path: path.to_path_buf(),
        run_id: Some(path_run_id),
        path_run_id: Some(path_run_id),
        archive_directory: Some(expected_archive_directory),
        archive_present,
        registry_entry: Some(entry),
        detail,
        registry_snapshot: Some(snapshot),
    }
}

fn untrusted_candidate(
    classification: CandidateClassification,
    source: CandidateSource,
    path: PathBuf,
    path_run_id: Option<CanonicalUuid>,
    archive_directory: Option<PathBuf>,
    archive_present: bool,
    detail: String,
) -> DiscoveryCandidate {
    DiscoveryCandidate {
        classification,
        source,
        path,
        run_id: None,
        path_run_id,
        archive_directory,
        archive_present,
        registry_entry: None,
        detail: Some(detail),
        registry_snapshot: None,
    }
}

pub(crate) fn classify_registry_entry(
    entry: &RegistryEntry,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> (CandidateClassification, Option<String>) {
    let first = classify_process_identity(
        entry.process_identity(),
        process_probe.observe(entry.owner_pid()),
    );
    match first {
        ProcessIdentityClassification::DefinitelyGone => {
            return (
                CandidateClassification::DefiniteStale,
                Some("owner process is definitely gone".to_owned()),
            );
        }
        ProcessIdentityClassification::PidReused => {
            return (
                CandidateClassification::DefiniteStale,
                Some("owner PID has been reused".to_owned()),
            );
        }
        ProcessIdentityClassification::Unknown => {
            return (
                CandidateClassification::Unhealthy,
                Some("owner process identity cannot be established".to_owned()),
            );
        }
        ProcessIdentityClassification::Alive => {}
    }

    let tentative = match server_probe.probe_identity(entry) {
        Ok(bytes) => classify_server_identity(entry, &bytes),
        Err(error) => (
            CandidateClassification::Unhealthy,
            Some(format!("server identity endpoint is unreachable: {error}")),
        ),
    };

    // The server round trip creates a PID-reuse window. Active is only emitted after a second
    // observation proves that the same owner still exists.
    match classify_process_identity(
        entry.process_identity(),
        process_probe.observe(entry.owner_pid()),
    ) {
        ProcessIdentityClassification::Alive => tentative,
        ProcessIdentityClassification::DefinitelyGone => (
            CandidateClassification::DefiniteStale,
            Some("owner process disappeared during identity validation".to_owned()),
        ),
        ProcessIdentityClassification::PidReused => (
            CandidateClassification::DefiniteStale,
            Some("owner PID was reused during identity validation".to_owned()),
        ),
        ProcessIdentityClassification::Unknown => (
            CandidateClassification::Unhealthy,
            Some("owner process identity became unavailable during validation".to_owned()),
        ),
    }
}

fn classify_server_identity(
    entry: &RegistryEntry,
    bytes: &[u8],
) -> (CandidateClassification, Option<String>) {
    let identity = match decode_server_identity(bytes) {
        Ok(identity) => identity,
        Err(error) => {
            let classification = match error.code() {
                ServerIdentityDecodeErrorCode::Incompatible => {
                    CandidateClassification::Incompatible
                }
                ServerIdentityDecodeErrorCode::ResponseTooLarge
                | ServerIdentityDecodeErrorCode::Invalid => CandidateClassification::Unhealthy,
            };
            return (classification, Some(error.to_string()));
        }
    };
    if identity.run_id() != entry.run_id()
        || identity.owner_pid() != entry.owner_pid()
        || identity.process_identity() != entry.process_identity()
    {
        return (
            CandidateClassification::IdentityMismatch,
            Some("server Run or process identity differs from the registry entry".to_owned()),
        );
    }
    if identity.server_protocol_version() != u64::from(entry.server_protocol_version())
        || identity.server_protocol_version() != u64::from(SERVER_PROTOCOL_VERSION)
    {
        return (
            CandidateClassification::Incompatible,
            Some("server protocol is incompatible with this client".to_owned()),
        );
    }

    let expected_base_path = registry_base_path(entry);
    let expected_api_path = join_base_path(&expected_base_path, "/api/v1");
    let expected_identity_path = join_base_path(&expected_base_path, "/api/v1/identity");
    let bind = entry.bind();
    if identity.bind_host() != bind.host()
        || identity.port() != bind.port()
        || identity.local_endpoint() != entry.local_endpoint()
        || identity.advertise_url() != entry.advertise_url()
        || identity.security_scope() != entry.security_scope()
        || identity.base_path() != expected_base_path
        || identity.api_base_path() != expected_api_path
        || identity.identity_path() != expected_identity_path
    {
        return (
            CandidateClassification::IdentityMismatch,
            Some("server locator identity differs from the registry entry".to_owned()),
        );
    }

    (CandidateClassification::Active, None)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerIdentityDecodeErrorCode {
    ResponseTooLarge,
    Invalid,
    Incompatible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerIdentityDecodeError {
    code: ServerIdentityDecodeErrorCode,
    detail: String,
}

impl ServerIdentityDecodeError {
    pub const fn code(&self) -> ServerIdentityDecodeErrorCode {
        self.code
    }
}

impl fmt::Display for ServerIdentityDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ServerIdentityDecodeError {}

pub fn decode_server_identity(
    bytes: &[u8],
) -> Result<DecodedServerIdentity, ServerIdentityDecodeError> {
    if bytes.len() > MAX_SERVER_IDENTITY_BYTES {
        return Err(ServerIdentityDecodeError {
            code: ServerIdentityDecodeErrorCode::ResponseTooLarge,
            detail: "server identity response exceeds the size limit".to_owned(),
        });
    }
    let envelope: ServerIdentityEnvelope =
        serde_json::from_slice(bytes).map_err(|error| ServerIdentityDecodeError {
            code: ServerIdentityDecodeErrorCode::Invalid,
            detail: format!("server identity response is invalid: {error}"),
        })?;
    if envelope.identity_schema_version > SERVER_IDENTITY_SCHEMA_VERSION {
        return Err(ServerIdentityDecodeError {
            code: ServerIdentityDecodeErrorCode::Incompatible,
            detail: format!(
                "server identity schema {} is newer than {}",
                envelope.identity_schema_version, SERVER_IDENTITY_SCHEMA_VERSION
            ),
        });
    }
    if envelope.identity_schema_version != SERVER_IDENTITY_SCHEMA_VERSION {
        return Err(ServerIdentityDecodeError {
            code: ServerIdentityDecodeErrorCode::Invalid,
            detail: "server identity schema is unsupported".to_owned(),
        });
    }

    let identity: DecodedServerIdentity =
        serde_json::from_slice(bytes).map_err(|error| ServerIdentityDecodeError {
            code: ServerIdentityDecodeErrorCode::Invalid,
            detail: format!("server identity response is invalid: {error}"),
        })?;
    if identity.identity_schema_version != SERVER_IDENTITY_SCHEMA_VERSION {
        return Err(ServerIdentityDecodeError {
            code: ServerIdentityDecodeErrorCode::Invalid,
            detail: "server identity schema changed during decode".to_owned(),
        });
    }
    Ok(identity)
}

fn registry_base_path(entry: &RegistryEntry) -> String {
    let Some(advertise_url) = entry.advertise_url() else {
        return "/".to_owned();
    };
    let uri = advertise_url
        .as_str()
        .parse::<Uri>()
        .expect("validated advertised URL remains a URI");
    let path = uri.path();
    if path == "/" || path.is_empty() {
        "/".to_owned()
    } else {
        path.strip_suffix('/').unwrap_or(path).to_owned()
    }
}

fn join_base_path(base_path: &str, relative_path: &str) -> String {
    if base_path == "/" {
        relative_path.to_owned()
    } else {
        format!("{base_path}{relative_path}")
    }
}

#[derive(Deserialize)]
struct ServerIdentityEnvelope {
    identity_schema_version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DecodedServerIdentity {
    identity_schema_version: u64,
    server_protocol_version: u64,
    #[serde(rename = "event_schema_version")]
    _event_schema_version: u64,
    #[serde(rename = "view_schema_version")]
    _view_schema_version: u64,
    #[serde(rename = "api_schema_version")]
    _api_schema_version: u64,
    run_id: CanonicalUuid,
    owner_pid: u32,
    process_identity: super::process_identity::ProcessIdentity,
    bind_host: String,
    port: u16,
    local_endpoint: WebBaseUrl,
    advertise_url: Option<WebBaseUrl>,
    base_path: String,
    api_base_path: String,
    identity_path: String,
    security_scope: SecurityScope,
    #[serde(rename = "operational_limits")]
    _operational_limits: BTreeMap<String, SchemaU64>,
}

impl DecodedServerIdentity {
    pub const fn identity_schema_version(&self) -> u64 {
        self.identity_schema_version
    }

    pub const fn server_protocol_version(&self) -> u64 {
        self.server_protocol_version
    }

    pub const fn event_schema_version(&self) -> u64 {
        self._event_schema_version
    }

    pub const fn view_schema_version(&self) -> u64 {
        self._view_schema_version
    }

    pub const fn api_schema_version(&self) -> u64 {
        self._api_schema_version
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn owner_pid(&self) -> u32 {
        self.owner_pid
    }

    pub const fn process_identity(&self) -> &super::process_identity::ProcessIdentity {
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

    pub const fn operational_limits(&self) -> &BTreeMap<String, SchemaU64> {
        &self._operational_limits
    }
}

fn classify_archive(
    run_directory: &Path,
    path_run_id: CanonicalUuid,
) -> (CandidateClassification, bool, Option<String>) {
    let directory_metadata = match fs::symlink_metadata(run_directory) {
        Ok(metadata) if metadata.file_type().is_dir() => metadata,
        Ok(_) => {
            return (
                CandidateClassification::Invalid,
                false,
                Some("Run archive path is not a regular directory".to_owned()),
            );
        }
        Err(error) => {
            return (
                CandidateClassification::Invalid,
                false,
                Some(format!("Run archive cannot be inspected: {error}")),
            );
        }
    };
    let directory_identity = FileIdentity::from_metadata(&directory_metadata);
    let database_path = run_directory.join(DIAGNOSTIC_DATABASE_FILENAME);
    let database_metadata = match fs::symlink_metadata(&database_path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => {
            return (
                CandidateClassification::Invalid,
                false,
                Some("diagnostic database is not a regular file".to_owned()),
            );
        }
        Err(error) => {
            return (
                CandidateClassification::Invalid,
                false,
                Some(format!("diagnostic database cannot be inspected: {error}")),
            );
        }
    };
    let database_identity = FileIdentity::from_metadata(&database_metadata);

    let _lease = match SharedArchiveLease::acquire(run_directory) {
        Ok(lease) => lease,
        Err(error) => {
            let classification = if error.code() == ArchiveLeaseErrorCode::Contended {
                CandidateClassification::Unhealthy
            } else {
                CandidateClassification::Invalid
            };
            return (
                classification,
                false,
                Some(format!(
                    "Run archive cannot be leased for read-only inspection: {error}"
                )),
            );
        }
    };

    let connection = match open_immutable_read_only(&database_path) {
        Ok(connection) => connection,
        Err(error) => {
            return (
                CandidateClassification::Invalid,
                false,
                Some(format!(
                    "diagnostic database cannot be opened read-only: {error}"
                )),
            );
        }
    };
    if let Err(error) = connection
        .busy_timeout(Duration::from_millis(250))
        .and_then(|()| connection.pragma_update(None, "query_only", true))
    {
        return (
            CandidateClassification::Invalid,
            false,
            Some(format!(
                "diagnostic database cannot enter query-only mode: {error}"
            )),
        );
    }
    if !connection.is_readonly("main").unwrap_or(false) {
        return (
            CandidateClassification::Invalid,
            false,
            Some("diagnostic database connection is not read-only".to_owned()),
        );
    }

    let user_version: i64 =
        match connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0)) {
            Ok(version) => version,
            Err(error) => {
                return (
                    CandidateClassification::Invalid,
                    false,
                    Some(format!("diagnostic store schema cannot be read: {error}")),
                );
            }
        };
    if user_version > i64::from(STORE_SCHEMA_VERSION) {
        return (
            CandidateClassification::Incompatible,
            false,
            Some(format!(
                "diagnostic store schema {user_version} is newer than {STORE_SCHEMA_VERSION}"
            )),
        );
    }
    if user_version != i64::from(STORE_SCHEMA_VERSION) {
        return (
            CandidateClassification::Invalid,
            false,
            Some("diagnostic store schema is unsupported".to_owned()),
        );
    }

    let row = connection.query_row(
        "SELECT store_schema_version, schema_identity, event_schema_version, run_id, \
                started_at, ended_at, production_outcome, clean_shutdown \
         FROM run_metadata WHERE singleton = 1",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, i64>(7)?,
            ))
        },
    );
    let row = match row {
        Ok(row) => row,
        Err(error) => {
            return (
                CandidateClassification::Invalid,
                false,
                Some(format!("diagnostic Run metadata cannot be read: {error}")),
            );
        }
    };
    if row.0 > i64::from(STORE_SCHEMA_VERSION) || row.2 > i64::from(EVENT_SCHEMA_VERSION) {
        return (
            CandidateClassification::Incompatible,
            false,
            Some("diagnostic store metadata uses a newer schema".to_owned()),
        );
    }
    if row.0 != i64::from(STORE_SCHEMA_VERSION)
        || row.1 != STORE_SCHEMA_IDENTITY
        || row.2 != i64::from(EVENT_SCHEMA_VERSION)
        || row.4.is_empty()
        || !matches!(row.7, 0 | 1)
    {
        return (
            CandidateClassification::Invalid,
            false,
            Some("diagnostic store metadata is invalid".to_owned()),
        );
    }
    let stored_run_id = match CanonicalUuid::parse(&row.3) {
        Ok(run_id) => run_id,
        Err(error) => {
            return (
                CandidateClassification::Invalid,
                false,
                Some(format!("diagnostic store Run identity is invalid: {error}")),
            );
        }
    };
    if stored_run_id != path_run_id {
        return (
            CandidateClassification::Invalid,
            false,
            Some("diagnostic store Run identity differs from its directory".to_owned()),
        );
    }
    if row
        .6
        .as_deref()
        .is_some_and(|outcome| !matches!(outcome, "completed" | "failed" | "cancelled"))
    {
        return (
            CandidateClassification::Invalid,
            false,
            Some("diagnostic store outcome is invalid".to_owned()),
        );
    }

    let rebound_directory = fs::symlink_metadata(run_directory).ok();
    let rebound_database = fs::symlink_metadata(&database_path).ok();
    if !rebound_directory.is_some_and(|metadata| {
        metadata.file_type().is_dir()
            && directory_identity == FileIdentity::from_metadata(&metadata)
    }) || !rebound_database.is_some_and(|metadata| {
        metadata.file_type().is_file()
            && database_identity == FileIdentity::from_metadata(&metadata)
    }) {
        return (
            CandidateClassification::Invalid,
            false,
            Some("Run archive identity changed while it was inspected".to_owned()),
        );
    }

    let complete = row.7 == 1 && row.5.is_some() && row.6.is_some();
    (
        if complete {
            CandidateClassification::Completed
        } else {
            CandidateClassification::Incomplete
        },
        true,
        None,
    )
}

fn registry_path_run_id(path: &Path) -> Option<CanonicalUuid> {
    let filename = path.file_name()?.to_str()?;
    let stem = filename.strip_suffix(".json")?;
    CanonicalUuid::parse(stem).ok()
}

fn scan_namespace(path: &Path) -> Result<Vec<PathBuf>, DiscoveryError> {
    let initial = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(DiscoveryError::io(
                DiscoveryErrorCode::NamespaceInspectFailed,
                path,
                error,
            ));
        }
    };
    if !initial.file_type().is_dir() {
        return Err(DiscoveryError::logical(
            DiscoveryErrorCode::NamespaceNotDirectory,
            path,
        ));
    }
    let identity = FileIdentity::from_metadata(&initial);
    let entries = fs::read_dir(path).map_err(|error| {
        DiscoveryError::io(DiscoveryErrorCode::NamespaceReadFailed, path, error)
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            DiscoveryError::io(DiscoveryErrorCode::NamespaceReadFailed, path, error)
        })?;
        paths.push(entry.path());
    }
    let rebound = fs::symlink_metadata(path).map_err(|error| {
        DiscoveryError::io(DiscoveryErrorCode::NamespaceInspectFailed, path, error)
    })?;
    if !rebound.file_type().is_dir() || identity != FileIdentity::from_metadata(&rebound) {
        return Err(DiscoveryError::logical(
            DiscoveryErrorCode::NamespaceChanged,
            path,
        ));
    }
    paths.sort();
    Ok(paths)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegistrySnapshot {
    identity: FileIdentity,
    bytes: Vec<u8>,
}

impl RegistrySnapshot {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    first: u64,
    second: u64,
}

impl FileIdentity {
    #[cfg(unix)]
    fn from_metadata(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            first: metadata.dev(),
            second: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            first: metadata.len(),
            second: u64::from(metadata.permissions().readonly()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotReadErrorCode {
    Inspect,
    NotRegular,
    Open,
    Changed,
    Read,
    TooLarge,
}

#[derive(Debug)]
pub(crate) struct SnapshotReadError {
    code: SnapshotReadErrorCode,
    path: PathBuf,
    source: Option<io::Error>,
}

impl fmt::Display for SnapshotReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "registry locator cannot be read [{:?}] at {}",
            self.code,
            self.path.display()
        )?;
        if let Some(source) = &self.source {
            write!(formatter, ": {source}")?;
        }
        Ok(())
    }
}

pub(crate) fn read_registry_snapshot(path: &Path) -> Result<RegistrySnapshot, SnapshotReadError> {
    let initial = fs::symlink_metadata(path).map_err(|error| SnapshotReadError {
        code: SnapshotReadErrorCode::Inspect,
        path: path.to_path_buf(),
        source: Some(error),
    })?;
    if !initial.file_type().is_file() {
        return Err(SnapshotReadError {
            code: SnapshotReadErrorCode::NotRegular,
            path: path.to_path_buf(),
            source: None,
        });
    }
    let identity = FileIdentity::from_metadata(&initial);
    let mut file = File::open(path).map_err(|error| SnapshotReadError {
        code: SnapshotReadErrorCode::Open,
        path: path.to_path_buf(),
        source: Some(error),
    })?;
    let opened = file.metadata().map_err(|error| SnapshotReadError {
        code: SnapshotReadErrorCode::Inspect,
        path: path.to_path_buf(),
        source: Some(error),
    })?;
    if !opened.file_type().is_file() || identity != FileIdentity::from_metadata(&opened) {
        return Err(SnapshotReadError {
            code: SnapshotReadErrorCode::Changed,
            path: path.to_path_buf(),
            source: None,
        });
    }

    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_REGISTRY_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SnapshotReadError {
            code: SnapshotReadErrorCode::Read,
            path: path.to_path_buf(),
            source: Some(error),
        })?;
    if bytes.len() as u64 > MAX_REGISTRY_ENTRY_BYTES {
        return Err(SnapshotReadError {
            code: SnapshotReadErrorCode::TooLarge,
            path: path.to_path_buf(),
            source: None,
        });
    }
    let final_opened = file.metadata().map_err(|error| SnapshotReadError {
        code: SnapshotReadErrorCode::Inspect,
        path: path.to_path_buf(),
        source: Some(error),
    })?;
    let rebound = fs::symlink_metadata(path).map_err(|error| SnapshotReadError {
        code: SnapshotReadErrorCode::Inspect,
        path: path.to_path_buf(),
        source: Some(error),
    })?;
    if identity != FileIdentity::from_metadata(&final_opened)
        || !rebound.file_type().is_file()
        || identity != FileIdentity::from_metadata(&rebound)
    {
        return Err(SnapshotReadError {
            code: SnapshotReadErrorCode::Changed,
            path: path.to_path_buf(),
            source: None,
        });
    }
    Ok(RegistrySnapshot { identity, bytes })
}
