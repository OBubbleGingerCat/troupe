use std::{convert::Infallible, sync::Arc};

use hyper::{
    Method, Request, Response, StatusCode,
    body::Incoming,
    header::{ALLOW, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue},
};
use serde_json::json;

use super::{
    routes::{CachePolicy, ResponseBody, RouteRequest, RouteResponse, RouteTarget, Router},
};

const FORWARDED_HEADERS: [&str; 4] = [
    "forwarded",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-prefix",
];

pub(crate) async fn handle_request(
    router: Arc<Router>,
    request: Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    Ok(dispatch(&router, request).await)
}

async fn dispatch(router: &Router, request: Request<Incoming>) -> Response<ResponseBody> {
    let Some(target) = router.resolve(request.uri().path()) else {
        return finalize(error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "route is not registered",
        ), false);
    };
    let is_head = request.method() == Method::HEAD;
    if request.method() != Method::GET && !is_head {
        let mut response = error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "route is read-only and accepts GET or HEAD",
        );
        response.headers_mut().insert(
            ALLOW,
            HeaderValue::from_static("GET, HEAD"),
        );
        return finalize(response, false);
    }

    let response = match target {
        RouteTarget::Identity(bytes) => {
            let mut response = RouteResponse::bytes(StatusCode::OK, bytes);
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            response
        }
        RouteTarget::Injected(handler) => {
            let (parts, _body) = request.into_parts();
            let mut headers = parts.headers;
            for name in FORWARDED_HEADERS {
                headers.remove(name);
            }
            let request = RouteRequest::new(parts.method, parts.uri, headers);
            match handler.handle(request).await {
                Ok(response) => response,
                Err(error) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error.code(),
                    error.message(),
                ),
            }
        }
    };
    finalize(response, is_head)
}

fn error_response(status: StatusCode, code: &str, message: &str) -> RouteResponse {
    RouteResponse::json(
        status,
        &json!({
            "error": {
                "code": code,
                "message": message,
            }
        }),
    )
    .expect("error response is JSON serializable")
}

fn finalize(response: RouteResponse, is_head: bool) -> Response<ResponseBody> {
    let (status, mut headers, body, content_length, cache_policy) = response.into_parts();
    remove_cors_headers(&mut headers);
    apply_cache_policy(&mut headers, cache_policy);
    if is_head && let Some(content_length) = content_length {
        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&content_length.to_string())
                .expect("response body length is a valid header value"),
        );
    }
    let mut response = Response::new(if is_head {
        RouteResponse::empty(status).into_parts().2
    } else {
        body
    });
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn apply_cache_policy(headers: &mut hyper::HeaderMap, cache_policy: CachePolicy) {
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(cache_policy.header_value()),
    );
}

fn remove_cors_headers(headers: &mut hyper::HeaderMap) {
    let cors_headers: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("access-control-"))
        .cloned()
        .collect();
    for name in cors_headers {
        headers.remove(name);
    }
}
