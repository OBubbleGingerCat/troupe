#![allow(dead_code)]

use std::{fmt, time::Duration};

use reqwest::{Client, RequestBuilder, Url, redirect::Policy};
use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::registry::{
    discover::{
        DecodedServerIdentity, ServerIdentityDecodeErrorCode, ServerIdentityProbe,
        ServerProbeError, decode_server_identity,
    },
    model::{RegistryEntry, SERVER_PROTOCOL_VERSION, WebBaseUrl},
};

const IDENTITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IDENTITY_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HttpClientErrorCode {
    ClientBuild,
    RuntimeBuild,
    InvalidEndpoint,
    Transport,
    UnexpectedStatus,
    IdentityResponseTooLarge,
    InvalidIdentity,
    IncompatibleIdentity,
    IncompatibleProtocol,
    RunIdentityMismatch,
    LocatorIdentityMismatch,
}

impl HttpClientErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ClientBuild => "diagnostic_http.client_build",
            Self::RuntimeBuild => "diagnostic_http.runtime_build",
            Self::InvalidEndpoint => "diagnostic_http.invalid_endpoint",
            Self::Transport => "diagnostic_http.transport",
            Self::UnexpectedStatus => "diagnostic_http.unexpected_status",
            Self::IdentityResponseTooLarge => "diagnostic_http.identity_response_too_large",
            Self::InvalidIdentity => "diagnostic_http.invalid_identity",
            Self::IncompatibleIdentity => "diagnostic_http.incompatible_identity",
            Self::IncompatibleProtocol => "diagnostic_http.incompatible_protocol",
            Self::RunIdentityMismatch => "diagnostic_http.run_identity_mismatch",
            Self::LocatorIdentityMismatch => "diagnostic_http.locator_identity_mismatch",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HttpClientError {
    code: HttpClientErrorCode,
    detail: String,
}

impl HttpClientError {
    fn new(code: HttpClientErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub(crate) const fn code(&self) -> HttpClientErrorCode {
        self.code
    }
}

impl fmt::Display for HttpClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for HttpClientError {}

#[derive(Clone)]
pub(crate) struct DiagnosticHttpClient {
    client: Client,
    base_url: WebBaseUrl,
    run_id: CanonicalUuid,
}

impl fmt::Debug for DiagnosticHttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticHttpClient")
            .field("base_url", &self.base_url)
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

impl DiagnosticHttpClient {
    pub(crate) async fn connect(base_url: WebBaseUrl) -> Result<Self, HttpClientError> {
        let client = build_client()?;
        let endpoint = endpoint_url(&base_url, "/api/v1/identity")?;
        let bytes = fetch_identity_bytes(&client, endpoint).await?;
        Self::from_identity(client, base_url, &bytes, None)
    }

    pub(crate) fn from_validated_registry_entry(
        entry: &RegistryEntry,
    ) -> Result<Self, HttpClientError> {
        Ok(Self {
            client: build_client()?,
            base_url: registry_query_base_url(entry)?,
            run_id: entry.run_id(),
        })
    }

    pub(crate) fn from_identity_bytes(
        base_url: WebBaseUrl,
        bytes: &[u8],
        expected_run_id: Option<CanonicalUuid>,
    ) -> Result<Self, HttpClientError> {
        Self::from_identity(build_client()?, base_url, bytes, expected_run_id)
    }

    fn from_identity(
        client: Client,
        base_url: WebBaseUrl,
        bytes: &[u8],
        expected_run_id: Option<CanonicalUuid>,
    ) -> Result<Self, HttpClientError> {
        let identity = decode_server_identity(bytes).map_err(|error| {
            let code = match error.code() {
                ServerIdentityDecodeErrorCode::ResponseTooLarge => {
                    HttpClientErrorCode::IdentityResponseTooLarge
                }
                ServerIdentityDecodeErrorCode::Invalid => HttpClientErrorCode::InvalidIdentity,
                ServerIdentityDecodeErrorCode::Incompatible => {
                    HttpClientErrorCode::IncompatibleIdentity
                }
            };
            HttpClientError::new(code, error.to_string())
        })?;
        validate_identity(&base_url, &identity, expected_run_id)?;
        Ok(Self {
            client,
            base_url,
            run_id: identity.run_id(),
        })
    }

    pub(crate) async fn revalidate_identity(&self) -> Result<(), HttpClientError> {
        let endpoint = endpoint_url(&self.base_url, "/api/v1/identity")?;
        let bytes = fetch_identity_bytes(&self.client, endpoint).await?;
        let identity = decode_server_identity(&bytes).map_err(|error| {
            let code = match error.code() {
                ServerIdentityDecodeErrorCode::ResponseTooLarge => {
                    HttpClientErrorCode::IdentityResponseTooLarge
                }
                ServerIdentityDecodeErrorCode::Invalid => HttpClientErrorCode::InvalidIdentity,
                ServerIdentityDecodeErrorCode::Incompatible => {
                    HttpClientErrorCode::IncompatibleIdentity
                }
            };
            HttpClientError::new(code, error.to_string())
        })?;
        validate_identity(&self.base_url, &identity, Some(self.run_id))
    }

    pub(crate) const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub(crate) const fn base_url(&self) -> &WebBaseUrl {
        &self.base_url
    }

    pub(crate) fn endpoint(&self, relative_path: &str) -> Result<Url, HttpClientError> {
        endpoint_url(&self.base_url, relative_path)
    }

    pub(crate) fn get(&self, relative_path: &str) -> Result<RequestBuilder, HttpClientError> {
        self.endpoint(relative_path).map(|url| self.client.get(url))
    }
}

pub(crate) struct BlockingRegistryIdentityProbe {
    client: Client,
    runtime: tokio::runtime::Runtime,
}

impl BlockingRegistryIdentityProbe {
    pub(crate) fn new() -> Result<Self, HttpClientError> {
        let client = build_client()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                HttpClientError::new(
                    HttpClientErrorCode::RuntimeBuild,
                    format!("cannot create registry HTTP runtime: {error}"),
                )
            })?;
        Ok(Self { client, runtime })
    }
}

impl ServerIdentityProbe for BlockingRegistryIdentityProbe {
    fn probe_identity(&self, entry: &RegistryEntry) -> Result<Vec<u8>, ServerProbeError> {
        let base_url = registry_query_base_url(entry)
            .map_err(|error| ServerProbeError::new(error.to_string()))?;
        let endpoint = endpoint_url(&base_url, "/api/v1/identity")
            .map_err(|error| ServerProbeError::new(error.to_string()))?;
        self.runtime
            .block_on(fetch_identity_bytes(&self.client, endpoint))
            .map_err(|error| ServerProbeError::new(error.to_string()))
    }
}

fn build_client() -> Result<Client, HttpClientError> {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .map_err(|error| {
            HttpClientError::new(
                HttpClientErrorCode::ClientBuild,
                format!("cannot construct diagnostic HTTP client: {error}"),
            )
        })
}

async fn fetch_identity_bytes(client: &Client, endpoint: Url) -> Result<Vec<u8>, HttpClientError> {
    let mut response = client
        .get(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(IDENTITY_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            HttpClientError::new(
                HttpClientErrorCode::Transport,
                format!("identity request failed: {error}"),
            )
        })?;
    if response.status().as_u16() != 200 {
        return Err(HttpClientError::new(
            HttpClientErrorCode::UnexpectedStatus,
            format!("identity endpoint returned HTTP {}", response.status()),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_IDENTITY_RESPONSE_BYTES as u64)
    {
        return Err(HttpClientError::new(
            HttpClientErrorCode::IdentityResponseTooLarge,
            "identity response exceeds the size limit",
        ));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        HttpClientError::new(
            HttpClientErrorCode::Transport,
            format!("identity response body failed: {error}"),
        )
    })? {
        let next_length = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
            HttpClientError::new(
                HttpClientErrorCode::IdentityResponseTooLarge,
                "identity response length overflowed",
            )
        })?;
        if next_length > MAX_IDENTITY_RESPONSE_BYTES {
            return Err(HttpClientError::new(
                HttpClientErrorCode::IdentityResponseTooLarge,
                "identity response exceeds the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_identity(
    base_url: &WebBaseUrl,
    identity: &DecodedServerIdentity,
    expected_run_id: Option<CanonicalUuid>,
) -> Result<(), HttpClientError> {
    if identity.server_protocol_version() != u64::from(SERVER_PROTOCOL_VERSION) {
        return Err(HttpClientError::new(
            HttpClientErrorCode::IncompatibleProtocol,
            format!(
                "server protocol {} is incompatible with {}",
                identity.server_protocol_version(),
                SERVER_PROTOCOL_VERSION
            ),
        ));
    }
    if expected_run_id.is_some_and(|expected| expected != identity.run_id()) {
        return Err(HttpClientError::new(
            HttpClientErrorCode::RunIdentityMismatch,
            "server Run identity changed",
        ));
    }

    let base_path = normalized_base_path(base_url)?;
    let expected_api_path = join_base_path(&base_path, "/api/v1");
    let expected_identity_path = join_base_path(&base_path, "/api/v1/identity");
    if identity.base_path() != base_path
        || identity.api_base_path() != expected_api_path
        || identity.identity_path() != expected_identity_path
    {
        return Err(HttpClientError::new(
            HttpClientErrorCode::LocatorIdentityMismatch,
            "server identity paths differ from the requested base URL",
        ));
    }
    Ok(())
}

fn registry_query_base_url(entry: &RegistryEntry) -> Result<WebBaseUrl, HttpClientError> {
    let base_path = entry
        .advertise_url()
        .map(normalized_base_path)
        .transpose()?
        .unwrap_or_else(|| "/".to_owned());
    let local = entry.local_endpoint().as_str().trim_end_matches('/');
    WebBaseUrl::parse(&format!("{local}{base_path}")).map_err(|error| {
        HttpClientError::new(
            HttpClientErrorCode::InvalidEndpoint,
            format!("registry endpoint is invalid: {error}"),
        )
    })
}

fn endpoint_url(base_url: &WebBaseUrl, relative_path: &str) -> Result<Url, HttpClientError> {
    if !relative_path.starts_with('/') || relative_path.starts_with("//") {
        return Err(HttpClientError::new(
            HttpClientErrorCode::InvalidEndpoint,
            "diagnostic endpoint path must be absolute and origin-relative",
        ));
    }
    let base = base_url.as_str().trim_end_matches('/');
    Url::parse(&format!("{base}{relative_path}")).map_err(|error| {
        HttpClientError::new(
            HttpClientErrorCode::InvalidEndpoint,
            format!("diagnostic endpoint is invalid: {error}"),
        )
    })
}

fn normalized_base_path(base_url: &WebBaseUrl) -> Result<String, HttpClientError> {
    let url = Url::parse(base_url.as_str()).map_err(|error| {
        HttpClientError::new(
            HttpClientErrorCode::InvalidEndpoint,
            format!("diagnostic base URL is invalid: {error}"),
        )
    })?;
    let path = url.path();
    if path == "/" || path.is_empty() {
        Ok("/".to_owned())
    } else {
        Ok(path.strip_suffix('/').unwrap_or(path).to_owned())
    }
}

fn join_base_path(base_path: &str, relative_path: &str) -> String {
    if base_path == "/" {
        relative_path.to_owned()
    } else {
        format!("{base_path}{relative_path}")
    }
}
