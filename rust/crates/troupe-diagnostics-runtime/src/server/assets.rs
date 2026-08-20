use std::collections::BTreeMap;

use bytes::Bytes;
use hyper::{
    HeaderMap, StatusCode,
    header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, ETAG, IF_NONE_MATCH, VARY},
};

use super::{
    error::RouteConfigurationError,
    routes::{CachePolicy, RouteDefinition, RouteRequest, RouteResponse},
};

mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/generated/assets.rs"
    ));
}

const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'; worker-src 'self'; manifest-src 'self'";
const X_CONTENT_TYPE_OPTIONS: &str = "nosniff";
const REFERRER_POLICY: &str = "no-referrer";
const CROSS_ORIGIN_RESOURCE_POLICY: &str = "same-origin";
const CROSS_ORIGIN_OPENER_POLICY: &str = "same-origin";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

impl Encoding {
    const fn generated_name(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Gzip => "gzip",
            Self::Identity => "raw",
        }
    }

    const fn accepted_name(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Gzip => "gzip",
            Self::Identity => "identity",
        }
    }
}

pub fn route_definitions() -> Result<Vec<RouteDefinition>, RouteConfigurationError> {
    let mut routes = Vec::with_capacity(3);
    routes.push(RouteDefinition::read_only("/", |request| async move {
        Ok(html_response(&request))
    })?);

    let mut logical_assets = BTreeMap::new();
    for representation in generated::REPRESENTATIONS {
        logical_assets.insert(representation.url, representation.kind);
    }
    for (url, kind) in logical_assets {
        let path = url.strip_prefix('.').ok_or_else(|| {
            RouteConfigurationError::new("generated asset URL must be document-relative")
        })?;
        routes.push(RouteDefinition::read_only(
            path,
            move |request| async move { Ok(asset_response(&request, url, kind)) },
        )?);
    }
    Ok(routes)
}

pub const fn build_sha256() -> &'static str {
    generated::BUILD_SHA256
}

fn html_response(request: &RouteRequest) -> RouteResponse {
    let etag = strong_etag(generated::INDEX_HTML_SHA256);
    let status = if matches_if_none_match(request.headers(), &etag) {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let body = if status == StatusCode::NOT_MODIFIED {
        Bytes::new()
    } else {
        Bytes::from_static(generated::INDEX_HTML)
    };
    secure_response(RouteResponse::bytes(status, body))
        .with_header(
            CONTENT_TYPE,
            hyper::header::HeaderValue::from_static(generated::INDEX_HTML_MIME),
        )
        .with_header(ETAG, etag)
        .with_cache_policy(CachePolicy::NoCache)
}

fn asset_response(request: &RouteRequest, url: &str, kind: &str) -> RouteResponse {
    let Some(encoding) = preferred_encoding(request.headers()) else {
        return secure_response(RouteResponse::empty(StatusCode::NOT_ACCEPTABLE)).with_header(
            VARY,
            hyper::header::HeaderValue::from_static("Accept-Encoding"),
        );
    };
    let representation = generated::REPRESENTATIONS
        .iter()
        .find(|candidate| candidate.url == url && candidate.encoding == encoding.generated_name())
        .expect("generated assets contain raw, gzip, and Brotli representations");
    debug_assert_eq!(representation.kind, kind);
    let etag = strong_etag(representation.sha256);
    let status = if matches_if_none_match(request.headers(), &etag) {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let body = if status == StatusCode::NOT_MODIFIED {
        Bytes::new()
    } else {
        Bytes::from_static(representation.bytes)
    };
    let mut response = secure_response(RouteResponse::bytes(status, body))
        .with_header(
            CONTENT_TYPE,
            hyper::header::HeaderValue::from_static(representation.mime),
        )
        .with_header(ETAG, etag)
        .with_header(
            VARY,
            hyper::header::HeaderValue::from_static("Accept-Encoding"),
        )
        .with_cache_policy(CachePolicy::ImmutableOneYear);
    if let Some(content_encoding) = representation.content_encoding {
        response = response.with_header(
            CONTENT_ENCODING,
            hyper::header::HeaderValue::from_static(content_encoding),
        );
    }
    response
}

fn secure_response(response: RouteResponse) -> RouteResponse {
    response
        .with_header(
            hyper::header::HeaderName::from_static("content-security-policy"),
            hyper::header::HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        )
        .with_header(
            hyper::header::HeaderName::from_static("x-content-type-options"),
            hyper::header::HeaderValue::from_static(X_CONTENT_TYPE_OPTIONS),
        )
        .with_header(
            hyper::header::HeaderName::from_static("referrer-policy"),
            hyper::header::HeaderValue::from_static(REFERRER_POLICY),
        )
        .with_header(
            hyper::header::HeaderName::from_static("cross-origin-resource-policy"),
            hyper::header::HeaderValue::from_static(CROSS_ORIGIN_RESOURCE_POLICY),
        )
        .with_header(
            hyper::header::HeaderName::from_static("cross-origin-opener-policy"),
            hyper::header::HeaderValue::from_static(CROSS_ORIGIN_OPENER_POLICY),
        )
}

fn strong_etag(sha256: &str) -> hyper::header::HeaderValue {
    hyper::header::HeaderValue::from_str(&format!("\"sha256-{sha256}\""))
        .expect("generated SHA-256 is a valid strong ETag")
}

fn matches_if_none_match(headers: &HeaderMap, etag: &hyper::header::HeaderValue) -> bool {
    let Ok(expected) = etag.to_str() else {
        return false;
    };
    headers.get_all(IF_NONE_MATCH).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == "*"
                    || candidate == expected
                    || candidate
                        .strip_prefix("W/")
                        .is_some_and(|candidate| candidate == expected)
            })
        })
    })
}

fn preferred_encoding(headers: &HeaderMap) -> Option<Encoding> {
    let values: Vec<&str> = headers
        .get_all(ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    if values.is_empty() || values.iter().all(|value| value.trim().is_empty()) {
        return Some(Encoding::Identity);
    }

    let mut explicit = BTreeMap::new();
    let mut wildcard = None;
    for value in values {
        for item in value.split(',') {
            let mut parts = item.split(';');
            let coding = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
            if coding.is_empty() {
                continue;
            }
            let mut quality = 1000;
            let mut valid = true;
            for parameter in parts {
                let Some((name, value)) = parameter.trim().split_once('=') else {
                    valid = false;
                    break;
                };
                if name.trim().eq_ignore_ascii_case("q") {
                    let Some(parsed) = parse_quality(value.trim()) else {
                        valid = false;
                        break;
                    };
                    quality = parsed;
                }
            }
            if !valid {
                quality = 0;
            }
            if coding == "*" {
                wildcard = Some(wildcard.unwrap_or(0).max(quality));
            } else {
                explicit
                    .entry(coding)
                    .and_modify(|current: &mut u16| *current = (*current).max(quality))
                    .or_insert(quality);
            }
        }
    }

    [Encoding::Brotli, Encoding::Gzip, Encoding::Identity]
        .into_iter()
        .map(|encoding| {
            let name = encoding.accepted_name();
            let fallback = wildcard.unwrap_or(if encoding == Encoding::Identity {
                1000
            } else {
                0
            });
            (encoding, explicit.get(name).copied().unwrap_or(fallback))
        })
        .filter(|(_, quality)| *quality > 0)
        .max_by_key(|(encoding, quality)| {
            let preference = match encoding {
                Encoding::Brotli => 2,
                Encoding::Gzip => 1,
                Encoding::Identity => 0,
            };
            (*quality, preference)
        })
        .map(|(encoding, _)| encoding)
}

fn parse_quality(value: &str) -> Option<u16> {
    if value == "0" {
        return Some(0);
    }
    if value == "1" {
        return Some(1000);
    }
    let (whole, fraction) = value.split_once('.')?;
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match whole {
        "0" => {
            let mut padded = fraction.to_owned();
            while padded.len() < 3 {
                padded.push('0');
            }
            padded.parse().ok()
        }
        "1" if fraction.bytes().all(|byte| byte == b'0') => Some(1000),
        _ => None,
    }
}
