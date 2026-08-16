#![allow(dead_code)] // D07 wires this private command into the CLI dispatcher.

use std::{
    fmt::{self, Write as _},
    fs,
    path::{Path, PathBuf},
};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::registry::{
    discover::{
        CandidateClassification, CandidateSource, DiscoveryCandidate, ProcessIdentityProbe,
        RealProcessIdentityProbe, ServerIdentityProbe, discover_with,
    },
    model::{RegistryEntry, SecurityScope},
};

use super::{
    args::{DocumentFormat, RunsArgs},
    http_client::{BlockingRegistryIdentityProbe, HttpClientError},
};

const RUNS_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunsErrorCode {
    InvalidProductionRoot,
    DiscoveryFailed,
    HttpFailed,
    TaskFailed,
}

impl RunsErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidProductionRoot => "diagnostic_runs.invalid_production_root",
            Self::DiscoveryFailed => "diagnostic_runs.discovery_failed",
            Self::HttpFailed => "diagnostic_runs.http_failed",
            Self::TaskFailed => "diagnostic_runs.task_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunsError {
    code: RunsErrorCode,
    detail: String,
}

impl RunsError {
    fn new(code: RunsErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn http(error: HttpClientError) -> Self {
        Self::new(RunsErrorCode::HttpFailed, error.to_string())
    }

    pub(crate) const fn code(&self) -> RunsErrorCode {
        self.code
    }
}

impl fmt::Display for RunsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for RunsError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunsListing {
    production: PathBuf,
    candidates: Vec<RunCandidate>,
}

impl RunsListing {
    pub(crate) fn production(&self) -> &Path {
        &self.production
    }

    pub(crate) fn candidates(&self) -> &[RunCandidate] {
        &self.candidates
    }

    pub(crate) fn render(&self, format: DocumentFormat) -> String {
        match format {
            DocumentFormat::Human => self.render_human(),
            DocumentFormat::Json => self.render_json(),
        }
    }

    fn render_human(&self) -> String {
        let mut output = String::new();
        writeln!(output, "production: {}", self.production.display()).expect("write to String");
        writeln!(output, "candidate_count: {}", self.candidates.len()).expect("write to String");

        for (index, candidate) in self.candidates.iter().enumerate() {
            writeln!(output).expect("write to String");
            writeln!(output, "candidate {}:", index + 1).expect("write to String");
            writeln!(
                output,
                "  classification: {}",
                candidate.classification.as_str()
            )
            .expect("write to String");
            writeln!(output, "  source: {}", candidate.source.as_str()).expect("write to String");
            write_optional_human(&mut output, "run_id", candidate.run_id.as_ref());
            write_optional_human(&mut output, "path_run_id", candidate.path_run_id.as_ref());
            writeln!(output, "  path: {}", candidate.path.display()).expect("write to String");
            write_optional_path_human(
                &mut output,
                "archive_directory",
                candidate.archive_directory.as_deref(),
            );
            writeln!(output, "  archive_present: {}", candidate.archive_present)
                .expect("write to String");
            write_optional_human(&mut output, "detail", candidate.detail.as_deref());
            match &candidate.registry {
                Some(registry) => registry.render_human(&mut output),
                None => writeln!(output, "  registry: null").expect("write to String"),
            }
        }

        output
    }

    fn render_json(&self) -> String {
        let mut output = String::new();
        write!(
            output,
            "{{\"runs_schema_version\":{RUNS_SCHEMA_VERSION},\"production\":"
        )
        .expect("write to String");
        push_json_path(&mut output, &self.production);
        write!(
            output,
            ",\"candidate_count\":\"{}\",\"candidates\":[",
            self.candidates.len()
        )
        .expect("write to String");
        for (index, candidate) in self.candidates.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            candidate.render_json(&mut output);
        }
        output.push_str("]}\n");
        output
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunCandidate {
    classification: CandidateClassification,
    source: RunCandidateSource,
    run_id: Option<CanonicalUuid>,
    path_run_id: Option<CanonicalUuid>,
    path: PathBuf,
    archive_directory: Option<PathBuf>,
    archive_present: bool,
    detail: Option<String>,
    registry: Option<RunRegistry>,
}

impl RunCandidate {
    fn from_discovery(candidate: DiscoveryCandidate) -> Self {
        Self {
            classification: candidate.classification(),
            source: RunCandidateSource::from(candidate.source()),
            run_id: candidate.run_id(),
            path_run_id: candidate.path_run_id(),
            path: candidate.path().to_path_buf(),
            archive_directory: candidate.archive_directory().map(Path::to_path_buf),
            archive_present: candidate.archive_present(),
            detail: candidate.detail().map(ToOwned::to_owned),
            registry: candidate.registry_entry().map(RunRegistry::from),
        }
    }

    pub(crate) const fn classification(&self) -> CandidateClassification {
        self.classification
    }

    pub(crate) const fn source(&self) -> &'static str {
        self.source.as_str()
    }

    pub(crate) const fn run_id(&self) -> Option<CanonicalUuid> {
        self.run_id
    }

    pub(crate) const fn path_run_id(&self) -> Option<CanonicalUuid> {
        self.path_run_id
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) const fn archive_present(&self) -> bool {
        self.archive_present
    }

    fn render_json(&self, output: &mut String) {
        output.push_str("{\"classification\":");
        push_json_string(output, self.classification.as_str());
        output.push_str(",\"source\":");
        push_json_string(output, self.source.as_str());
        output.push_str(",\"run_id\":");
        push_optional_display_json(output, self.run_id.as_ref());
        output.push_str(",\"path_run_id\":");
        push_optional_display_json(output, self.path_run_id.as_ref());
        output.push_str(",\"path\":");
        push_json_path(output, &self.path);
        output.push_str(",\"archive_directory\":");
        push_optional_path_json(output, self.archive_directory.as_deref());
        write!(
            output,
            ",\"archive_present\":{},\"detail\":",
            self.archive_present
        )
        .expect("write to String");
        push_optional_string_json(output, self.detail.as_deref());
        output.push_str(",\"registry\":");
        match &self.registry {
            Some(registry) => registry.render_json(output),
            None => output.push_str("null"),
        }
        output.push('}');
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunCandidateSource {
    Instance,
    Archive,
}

impl RunCandidateSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::Archive => "archive",
        }
    }
}

impl From<CandidateSource> for RunCandidateSource {
    fn from(source: CandidateSource) -> Self {
        match source {
            CandidateSource::Instance => Self::Instance,
            CandidateSource::Archive => Self::Archive,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RunRegistry {
    registry_schema_version: u16,
    server_protocol_version: u16,
    run_id: CanonicalUuid,
    archive_directory: PathBuf,
    owner_pid: u32,
    process_identity: String,
    bind_host: String,
    port: u16,
    local_endpoint: String,
    advertise_url: Option<String>,
    security_scope: &'static str,
    started_at: String,
}

impl From<&RegistryEntry> for RunRegistry {
    fn from(entry: &RegistryEntry) -> Self {
        let bind = entry.bind();
        Self {
            registry_schema_version: entry.registry_schema_version(),
            server_protocol_version: entry.server_protocol_version(),
            run_id: entry.run_id(),
            archive_directory: entry.run_directory(),
            owner_pid: entry.owner_pid(),
            process_identity: entry.process_identity().as_str().to_owned(),
            bind_host: bind.host().to_owned(),
            port: bind.port(),
            local_endpoint: entry.local_endpoint().as_str().to_owned(),
            advertise_url: entry.advertise_url().map(|url| url.as_str().to_owned()),
            security_scope: match entry.security_scope() {
                SecurityScope::TrustedNetwork => "trusted_network",
            },
            started_at: entry.started_at().to_owned(),
        }
    }
}

impl RunRegistry {
    fn render_human(&self, output: &mut String) {
        writeln!(output, "  registry:").expect("write to String");
        writeln!(
            output,
            "    registry_schema_version: {}",
            self.registry_schema_version
        )
        .expect("write to String");
        writeln!(
            output,
            "    server_protocol_version: {}",
            self.server_protocol_version
        )
        .expect("write to String");
        writeln!(output, "    run_id: {}", self.run_id).expect("write to String");
        writeln!(
            output,
            "    archive_directory: {}",
            self.archive_directory.display()
        )
        .expect("write to String");
        writeln!(output, "    owner_pid: {}", self.owner_pid).expect("write to String");
        writeln!(output, "    process_identity: {}", self.process_identity)
            .expect("write to String");
        writeln!(output, "    bind_host: {}", self.bind_host).expect("write to String");
        writeln!(output, "    port: {}", self.port).expect("write to String");
        writeln!(output, "    local_endpoint: {}", self.local_endpoint).expect("write to String");
        write_optional_human(output, "  advertise_url", self.advertise_url.as_deref());
        writeln!(output, "    security_scope: {}", self.security_scope).expect("write to String");
        writeln!(output, "    started_at: {}", self.started_at).expect("write to String");
    }

    fn render_json(&self, output: &mut String) {
        write!(
            output,
            "{{\"registry_schema_version\":{},\"server_protocol_version\":{},\"run_id\":",
            self.registry_schema_version, self.server_protocol_version
        )
        .expect("write to String");
        push_json_string(output, &self.run_id.to_string());
        output.push_str(",\"archive_directory\":");
        push_json_path(output, &self.archive_directory);
        write!(output, ",\"owner_pid\":\"{}\",", self.owner_pid).expect("write to String");
        output.push_str("\"process_identity\":");
        push_json_string(output, &self.process_identity);
        output.push_str(",\"bind_host\":");
        push_json_string(output, &self.bind_host);
        write!(output, ",\"port\":\"{}\",", self.port).expect("write to String");
        output.push_str("\"local_endpoint\":");
        push_json_string(output, &self.local_endpoint);
        output.push_str(",\"advertise_url\":");
        push_optional_string_json(output, self.advertise_url.as_deref());
        output.push_str(",\"security_scope\":");
        push_json_string(output, self.security_scope);
        output.push_str(",\"started_at\":");
        push_json_string(output, &self.started_at);
        output.push('}');
    }
}

pub(crate) async fn execute(arguments: RunsArgs) -> Result<String, RunsError> {
    let production = arguments.production;
    let format = arguments.format;
    tokio::task::spawn_blocking(move || {
        let production = canonical_production_root(&production)?;
        let server_probe = BlockingRegistryIdentityProbe::new().map_err(RunsError::http)?;
        list_canonical_runs_with(&production, &RealProcessIdentityProbe, &server_probe)
            .map(|listing| listing.render(format))
    })
    .await
    .map_err(|error| {
        RunsError::new(
            RunsErrorCode::TaskFailed,
            format!("runs discovery task failed: {error}"),
        )
    })?
}

pub(crate) fn list_runs_with(
    production: &Path,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<RunsListing, RunsError> {
    let production = canonical_production_root(production)?;
    list_canonical_runs_with(&production, process_probe, server_probe)
}

fn canonical_production_root(production: &Path) -> Result<PathBuf, RunsError> {
    let canonical = fs::canonicalize(production).map_err(|error| {
        RunsError::new(
            RunsErrorCode::InvalidProductionRoot,
            format!("cannot resolve {}: {error}", production.display()),
        )
    })?;
    if !canonical.is_dir() {
        return Err(RunsError::new(
            RunsErrorCode::InvalidProductionRoot,
            format!(
                "production root is not a directory: {}",
                canonical.display()
            ),
        ));
    }
    Ok(canonical)
}

fn list_canonical_runs_with(
    production: &Path,
    process_probe: &dyn ProcessIdentityProbe,
    server_probe: &dyn ServerIdentityProbe,
) -> Result<RunsListing, RunsError> {
    let candidates = discover_with(production, process_probe, server_probe)
        .map_err(|error| RunsError::new(RunsErrorCode::DiscoveryFailed, error.to_string()))?;
    Ok(RunsListing {
        production: production.to_path_buf(),
        candidates: candidates
            .into_iter()
            .map(RunCandidate::from_discovery)
            .collect(),
    })
}

fn write_optional_human<T: fmt::Display + ?Sized>(
    output: &mut String,
    field: &str,
    value: Option<&T>,
) {
    match value {
        Some(value) => writeln!(output, "  {field}: {value}"),
        None => writeln!(output, "  {field}: null"),
    }
    .expect("write to String");
}

fn write_optional_path_human(output: &mut String, field: &str, value: Option<&Path>) {
    match value {
        Some(value) => writeln!(output, "  {field}: {}", value.display()),
        None => writeln!(output, "  {field}: null"),
    }
    .expect("write to String");
}

fn push_optional_display_json<T: fmt::Display + ?Sized>(output: &mut String, value: Option<&T>) {
    match value {
        Some(value) => push_json_string(output, &value.to_string()),
        None => output.push_str("null"),
    }
}

fn push_optional_path_json(output: &mut String, value: Option<&Path>) {
    match value {
        Some(value) => push_json_path(output, value),
        None => output.push_str("null"),
    }
}

fn push_optional_string_json(output: &mut String, value: Option<&str>) {
    match value {
        Some(value) => push_json_string(output, value),
        None => output.push_str("null"),
    }
}

fn push_json_path(output: &mut String, value: &Path) {
    push_json_string(output, &value.to_string_lossy());
}

fn push_json_string(output: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                let byte = character as u8;
                output.push_str("\\u00");
                output.push(HEX[(byte >> 4) as usize] as char);
                output.push(HEX[(byte & 0x0f) as usize] as char);
            }
            character => output.push(character),
        }
    }
    output.push('"');
}
