use std::{
    collections::HashSet,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, combinators::UnsyncBoxBody};
use hyper::{HeaderMap, Method, StatusCode, Uri, body::Body, header::HeaderValue};
use serde::Serialize;

use super::{
    error::{RequestError, RouteConfigurationError},
    identity::{ServerIdentity, join_base_path},
};

pub type RouteFuture = Pin<Box<dyn Future<Output = Result<RouteResponse, RequestError>> + Send>>;
pub type ResponseBodyError = Box<dyn Error + Send + Sync>;
pub type ResponseBody = UnsyncBoxBody<Bytes, ResponseBodyError>;

pub trait ReadOnlyRouteHandler: Send + Sync + 'static {
    fn handle(&self, request: RouteRequest) -> RouteFuture;
}

impl<F, Fut> ReadOnlyRouteHandler for F
where
    F: Fn(RouteRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RouteResponse, RequestError>> + Send + 'static,
{
    fn handle(&self, request: RouteRequest) -> RouteFuture {
        Box::pin(self(request))
    }
}

#[derive(Clone, Debug)]
pub struct RouteRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
}

impl RouteRequest {
    pub(crate) fn new(method: Method, uri: Uri, headers: HeaderMap) -> Self {
        Self {
            method,
            uri,
            headers,
        }
    }

    pub const fn method(&self) -> &Method {
        &self.method
    }

    pub const fn uri(&self) -> &Uri {
        &self.uri
    }

    pub const fn headers(&self) -> &HeaderMap {
        &self.headers
    }

}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CachePolicy {
    NoStore,
    NoCache,
    NoCacheNoTransform,
    ImmutableOneYear,
}

impl CachePolicy {
    pub(crate) const fn header_value(self) -> &'static str {
        match self {
            Self::NoStore => "no-store",
            Self::NoCache => "no-cache",
            Self::NoCacheNoTransform => "no-cache, no-transform",
            Self::ImmutableOneYear => "public, max-age=31536000, immutable",
        }
    }
}

pub struct RouteResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: ResponseBody,
    content_length: Option<u64>,
    cache_policy: CachePolicy,
}

impl RouteResponse {
    pub fn empty(status: StatusCode) -> Self {
        Self::bytes(status, Bytes::new())
    }

    pub fn bytes(status: StatusCode, body: impl Into<Bytes>) -> Self {
        let body = body.into();
        let content_length = u64::try_from(body.len()).ok();
        Self {
            status,
            headers: HeaderMap::new(),
            body: Full::new(body)
                .map_err(|never| match never {})
                .boxed_unsync(),
            content_length,
            cache_policy: CachePolicy::NoStore,
        }
    }

    pub fn stream<B>(status: StatusCode, body: B) -> Self
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Error + Send + Sync + 'static,
    {
        Self {
            status,
            headers: HeaderMap::new(),
            body: body
                .map_err(|error| -> ResponseBodyError { Box::new(error) })
                .boxed_unsync(),
            content_length: None,
            cache_policy: CachePolicy::NoStore,
        }
    }

    pub fn json<T>(status: StatusCode, value: &T) -> Result<Self, serde_json::Error>
    where
        T: Serialize + ?Sized,
    {
        let mut response = Self::bytes(status, serde_json::to_vec(value)?);
        response.headers.insert(
            hyper::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        Ok(response)
    }

    pub fn with_header(mut self, name: hyper::header::HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    pub(crate) const fn with_cache_policy(mut self, cache_policy: CachePolicy) -> Self {
        self.cache_policy = cache_policy;
        self
    }

    pub const fn status(&self) -> StatusCode {
        self.status
    }

    pub const fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub(crate) fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    pub const fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        StatusCode,
        HeaderMap,
        ResponseBody,
        Option<u64>,
        CachePolicy,
    ) {
        (
            self.status,
            self.headers,
            self.body,
            self.content_length,
            self.cache_policy,
        )
    }
}

impl fmt::Debug for RouteResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("content_length", &self.content_length)
            .field("cache_policy", &self.cache_policy)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct RouteDefinition {
    relative_path: String,
    handler: Arc<dyn ReadOnlyRouteHandler>,
}

impl RouteDefinition {
    pub fn read_only<F, Fut>(
        relative_path: &str,
        handler: F,
    ) -> Result<Self, RouteConfigurationError>
    where
        F: Fn(RouteRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<RouteResponse, RequestError>> + Send + 'static,
    {
        validate_relative_path(relative_path)?;
        if relative_path == "/api/v1/identity" {
            return Err(RouteConfigurationError::new(
                "the identity route is reserved by the server shell",
            ));
        }
        Ok(Self {
            relative_path: relative_path.to_owned(),
            handler: Arc::new(handler),
        })
    }

    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
}

pub(crate) fn validate_route_definitions(
    definitions: &[RouteDefinition],
) -> Result<(), RouteConfigurationError> {
    let mut paths = HashSet::with_capacity(definitions.len());
    for definition in definitions {
        if !paths.insert(definition.relative_path()) {
            return Err(RouteConfigurationError::new(
                "duplicate read-only route definition",
            ));
        }
    }
    Ok(())
}

pub(crate) struct Router {
    routes: std::collections::HashMap<String, RouteTarget>,
}

impl Router {
    pub(crate) fn new(
        identity: &ServerIdentity,
        identity_bytes: Bytes,
        definitions: Vec<RouteDefinition>,
    ) -> Result<Self, RouteConfigurationError> {
        validate_route_definitions(&definitions)?;
        let mut routes = std::collections::HashMap::with_capacity(definitions.len() + 1);
        routes.insert(
            identity.identity_path().to_owned(),
            RouteTarget::Identity(identity_bytes),
        );
        for definition in definitions {
            let path = join_base_path(identity.base_path(), &definition.relative_path);
            if routes
                .insert(path, RouteTarget::Injected(definition.handler))
                .is_some()
            {
                return Err(RouteConfigurationError::new(
                    "route conflicts with a server-owned route",
                ));
            }
        }
        Ok(Self { routes })
    }

    pub(crate) fn resolve(&self, path: &str) -> Option<RouteTarget> {
        self.routes.get(path).cloned()
    }
}

#[derive(Clone)]
pub(crate) enum RouteTarget {
    Identity(Bytes),
    Injected(Arc<dyn ReadOnlyRouteHandler>),
}

fn validate_relative_path(path: &str) -> Result<(), RouteConfigurationError> {
    if !path.starts_with('/')
        || !path.is_ascii()
        || path.contains('?')
        || path.contains('#')
        || path.contains("//")
        || path
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(RouteConfigurationError::new(
            "route path must be a normalized absolute ASCII path",
        ));
    }
    Ok(())
}
