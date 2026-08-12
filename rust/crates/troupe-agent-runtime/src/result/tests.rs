use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll};

use super::*;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyDictMethods, PyList, PyModule};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::schema::{CompiledActSchema, compile_act_schema};

#[test]
fn repairable_invalid_result_is_not_a_committed_handoff_failure() {
    let issue = ValidationIssue {
        path: "/value".to_owned(),
        code: "invalid_type",
        message: "expected int64".to_owned(),
    };
    let mut prepared = PreparedResultSettlement {
        outcome: Some(ResultAtSettlement::Rejected {
            issues: vec![issue.clone()],
            invalid_calls: 1,
            truncated: false,
        }),
        validation_bridge: None,
    };

    assert!(prepared.take_committed_failure().is_none());
    assert!(matches!(
        prepared.take_outcome(),
        ResultAtSettlement::Rejected {
            issues,
            invalid_calls: 1,
            truncated: false,
        } if issues == vec![issue]
    ));
}

struct PendingConnection {
    route: Arc<ResultRoute>,
    dropped: Arc<AtomicBool>,
}

struct ProbeBody {
    polled: Arc<AtomicBool>,
}

impl Body for ProbeBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        self.polled.store(true, Ordering::Release);
        Poll::Ready(None)
    }
}

struct FailFlushIo<T>(T);

impl<T> tokio::io::AsyncRead for FailFlushIo<T>
where
    T: tokio::io::AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buffer)
    }
}

impl<T> tokio::io::AsyncWrite for FailFlushIo<T>
where
    T: tokio::io::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected response flush failure",
        )))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl Future for PendingConnection {
    type Output = Result<(), ()>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl Drop for PendingConnection {
    fn drop(&mut self) {
        assert_eq!(
            *lock(&self.route.connections),
            1,
            "the route lease must outlive the connection future"
        );
        self.dropped.store(true, Ordering::Release);
    }
}

fn route_with(token: &str, generation: u64) -> Arc<ResultRoute> {
    route_for_test(token, generation)
}

fn route_with_revision(
    token: &str,
    generation: u64,
    mcp_revision: &'static str,
) -> Arc<ResultRoute> {
    route_for_test_with_revision(token, generation, mcp_revision)
}

fn origin_request<B>(body: B, origins: &[&'static str]) -> Request<B> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(MCP_PATH)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .header(AUTHORIZATION, "Bearer test-token")
        .body(body)
        .expect("the test request is valid");
    for origin in origins {
        request
            .headers_mut()
            .append(ORIGIN, hyper::header::HeaderValue::from_static(origin));
    }
    request
}

fn route() -> Arc<ResultRoute> {
    route_with("test-token", 1)
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {"name": "troupe-test-client", "version": "1"}
        }
    })
}

fn tool_call(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    })
}

async fn dispatch_json(
    route: &Arc<ResultRoute>,
    request: ResultRequestLease,
    message: Value,
) -> Value {
    let outcome = dispatch_message(route, request, message).await;
    assert!(outcome.transition.is_none());
    let bytes = outcome
        .response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn compiled_result_schema(py: Python<'_>) -> CompiledActSchema {
    let troupe = PyModule::new(py, "troupe").unwrap();
    troupe.setattr("__path__", PyList::empty(py)).unwrap();
    crate::schema::install(&troupe).unwrap();
    let act_schema = troupe.getattr("act_schema").unwrap();
    py.import("sys")
        .unwrap()
        .getattr("modules")
        .unwrap()
        .cast_into::<PyDict>()
        .unwrap()
        .set_item("troupe", &troupe)
        .unwrap();
    let kwargs = PyDict::new(py);
    kwargs.set_item("description", "decision").unwrap();
    kwargs
        .set_item("choices", vec!["approve", "reject"])
        .unwrap();
    let value = act_schema
        .getattr("StrValue")
        .unwrap()
        .call((), Some(&kwargs))
        .unwrap();
    let schema = PyDict::new(py);
    schema.set_item("decision", value).unwrap();
    compile_act_schema(schema.as_any()).unwrap()
}

#[test]
fn result_request_leases_enforce_invalid_budget_and_arm_generation() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let route = route();
        let no_slot = route.acquire_result_request();
        assert_eq!(
            no_slot.submit_value(&json!({"decision": "approve"})),
            ResultSubmission::NoActiveSlot,
        );

        let first = route
            .arm_result(
                uuid::Uuid::from_u128(1),
                1,
                Arc::new(compiled_result_schema(py)),
                None,
            )
            .unwrap();
        let stale = route.acquire_result_request();
        first.disarm();
        let second = route
            .arm_result(
                uuid::Uuid::from_u128(2),
                2,
                Arc::new(compiled_result_schema(py)),
                None,
            )
            .unwrap();
        assert_eq!(second.operation_id(), uuid::Uuid::from_u128(2));
        assert_eq!(second.turn_index(), 2);
        assert_eq!(
            stale.submit_value(&json!({"decision": "approve"})),
            ResultSubmission::TurnUnavailable,
        );

        let current = route.acquire_result_request();
        for attempt in 1..=8 {
            let ResultSubmission::Invalid {
                invalid_calls,
                truncated,
                ..
            } = current.submit_value(&json!({"decision": "maybe"}))
            else {
                panic!("the first eight invalid calls must remain repairable");
            };
            assert_eq!(invalid_calls, attempt);
            assert!(!truncated);
        }
        assert!(matches!(
            current.submit_value(&json!({"decision": "maybe"})),
            ResultSubmission::Rejected { invalid_calls: 9 },
        ));
        assert_eq!(
            current.submit_value(&json!({"decision": "approve"})),
            ResultSubmission::ResultContractRejected,
        );
        second.disarm();

        let accepted = route
            .arm_result(
                uuid::Uuid::from_u128(3),
                3,
                Arc::new(compiled_result_schema(py)),
                None,
            )
            .unwrap();
        let winner = route.acquire_result_request();
        assert_eq!(
            winner.submit_value(&json!({"decision": "approve"})),
            ResultSubmission::Accepted,
        );
        assert_eq!(
            winner.submit_value(&json!({"decision": "reject"})),
            ResultSubmission::AlreadySubmitted,
        );
        assert_eq!(
            accepted.accepted_result(),
            Some(crate::schema::ValidatedActValue::Object(vec![(
                "decision".to_owned(),
                crate::schema::ValidatedActValue::String("approve".to_owned()),
            )])),
        );
    });
}

#[tokio::test]
async fn tools_call_validates_base_envelope_and_maps_result_slot_outcomes() {
    let (route, schema) = {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let route = route();
            *lock(&route.phase) = RoutePhase::Ready;
            (route, compiled_result_schema(py))
        })
    };

    let no_slot = dispatch_json(
        &route,
        route.acquire_result_request(),
        tool_call(1, RESULT_TOOL, json!({"value": {"decision": "approve"}})),
    )
    .await;
    assert_eq!(no_slot.pointer("/result/isError"), Some(&json!(true)));

    let armed = route
        .arm_result(uuid::Uuid::from_u128(10), 1, Arc::new(schema), None)
        .unwrap();

    for invalid_id in [Value::Null, json!(true), json!([]), json!({})] {
        let malformed = json!({
            "jsonrpc": "2.0",
            "id": invalid_id,
            "method": "tools/call",
            "params": {
                "name": RESULT_TOOL,
                "arguments": {"value": {"decision": "maybe"}},
            },
        });
        let response = dispatch_json(&route, route.acquire_result_request(), malformed).await;
        assert_eq!(response.pointer("/error/code"), Some(&json!(-32600)));
        assert_eq!(response.pointer("/id"), Some(&Value::Null));
    }

    for malformed in [
        tool_call(2, "wrong_tool", json!({"value": {"decision": "approve"}})),
        tool_call(
            3,
            RESULT_TOOL,
            json!({"value": {"decision": "approve"}, "extra": true}),
        ),
    ] {
        let response = dispatch_json(&route, route.acquire_result_request(), malformed).await;
        assert_eq!(response.pointer("/error/code"), Some(&json!(-32602)));
    }

    let invalid = dispatch_json(
        &route,
        route.acquire_result_request(),
        tool_call(4, RESULT_TOOL, json!({"value": {"decision": "maybe"}})),
    )
    .await;
    assert_eq!(invalid.pointer("/result/isError"), Some(&json!(true)));
    assert!(
        invalid
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("/decision"))
    );

    let accepted = dispatch_json(
        &route,
        route.acquire_result_request(),
        tool_call(5, RESULT_TOOL, json!({"value": {"decision": "approve"}})),
    )
    .await;
    assert_eq!(accepted.pointer("/result/isError"), Some(&json!(false)));

    let duplicate = dispatch_json(
        &route,
        route.acquire_result_request(),
        tool_call(6, RESULT_TOOL, json!({"value": {"decision": "reject"}})),
    )
    .await;
    assert_eq!(duplicate.pointer("/result/isError"), Some(&json!(true)));
    assert!(
        duplicate
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("already submitted"))
    );
    assert!(armed.accepted_result().is_some());
}

#[tokio::test]
async fn tools_call_accepts_every_json_number_request_id() {
    let schema = {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| Arc::new(compiled_result_schema(py)))
    };
    let numeric_ids = [
        serde_json::from_str::<Value>("1.5").unwrap(),
        json!(u64::MAX),
        serde_json::from_str::<Value>("184467440737095516160").unwrap(),
        serde_json::from_str::<Value>("-1.25e2").unwrap(),
    ];

    for (index, id) in numeric_ids.into_iter().enumerate() {
        let route = route();
        *lock(&route.phase) = RoutePhase::Ready;
        let armed = route
            .arm_result(
                uuid::Uuid::from_u128(100 + index as u128),
                1,
                Arc::clone(&schema),
                None,
            )
            .unwrap();
        let mut message = tool_call(
            100 + index as u64,
            RESULT_TOOL,
            json!({"value": {"decision": "approve"}}),
        );
        message["id"] = id.clone();

        let response = dispatch_json(&route, route.acquire_result_request(), message).await;

        assert_eq!(response.get("id"), Some(&id));
        assert_eq!(response.pointer("/result/isError"), Some(&json!(false)));
        assert!(armed.accepted_result().is_some());
    }
}

#[tokio::test]
async fn tools_call_validates_request_metadata_before_result_submission() {
    let schema = {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| Arc::new(compiled_result_schema(py)))
    };

    for (index, metadata) in [
        json!(1),
        json!({"progressToken": []}),
        json!({"progressToken": 1.5}),
    ]
    .into_iter()
    .enumerate()
    {
        let route = route();
        *lock(&route.phase) = RoutePhase::Ready;
        let armed = route
            .arm_result(
                uuid::Uuid::from_u128(20 + index as u128),
                1,
                Arc::clone(&schema),
                None,
            )
            .unwrap();
        let mut message = tool_call(
            20 + index as u64,
            RESULT_TOOL,
            json!({"value": {"decision": "approve"}}),
        );
        message["params"]["_meta"] = metadata;

        let response = dispatch_json(&route, route.acquire_result_request(), message).await;

        assert_eq!(response.pointer("/error/code"), Some(&json!(-32602)));
        assert_eq!(armed.accepted_result(), None);
    }

    let arbitrary_size_integer = serde_json::from_str::<Value>("184467440737095516160").unwrap();
    for (index, metadata) in [
        json!({"progressToken": "turn", "vendorExtension": true}),
        json!({"progressToken": 7}),
        json!({"progressToken": arbitrary_size_integer}),
    ]
    .into_iter()
    .enumerate()
    {
        let route = route();
        *lock(&route.phase) = RoutePhase::Ready;
        let armed = route
            .arm_result(
                uuid::Uuid::from_u128(30 + index as u128),
                1,
                Arc::clone(&schema),
                None,
            )
            .unwrap();
        let mut message = tool_call(
            30 + index as u64,
            RESULT_TOOL,
            json!({"value": {"decision": "approve"}}),
        );
        message["params"]["_meta"] = metadata;

        let response = dispatch_json(&route, route.acquire_result_request(), message).await;

        assert_eq!(response.pointer("/result/isError"), Some(&json!(false)));
        assert!(armed.accepted_result().is_some());
    }
}

#[tokio::test]
async fn tools_call_validates_task_metadata_before_result_submission() {
    let schema = {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| Arc::new(compiled_result_schema(py)))
    };

    for (index, task) in [json!(1), json!({"ttl": []}), json!({"ttl": null})]
        .into_iter()
        .enumerate()
    {
        let route = route();
        *lock(&route.phase) = RoutePhase::Ready;
        let armed = route
            .arm_result(
                uuid::Uuid::from_u128(40 + index as u128),
                1,
                Arc::clone(&schema),
                None,
            )
            .unwrap();
        let mut message = tool_call(
            40 + index as u64,
            RESULT_TOOL,
            json!({"value": {"decision": "maybe"}}),
        );
        message["params"]["task"] = task;

        let response = dispatch_json(&route, route.acquire_result_request(), message).await;

        assert_eq!(response.pointer("/error/code"), Some(&json!(-32602)));
        assert_eq!(armed.accepted_result(), None);

        let first_counted = dispatch_json(
            &route,
            route.acquire_result_request(),
            tool_call(
                50 + index as u64,
                RESULT_TOOL,
                json!({"value": {"decision": "maybe"}}),
            ),
        )
        .await;
        assert!(
            first_counted
                .pointer("/result/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("invalid call 1/8"))
        );
        armed.disarm();
    }

    for (index, task) in [
        json!({}),
        json!({"ttl": 60_000}),
        json!({"ttl": 1.5, "vendorExtension": true}),
    ]
    .into_iter()
    .enumerate()
    {
        let route = route();
        *lock(&route.phase) = RoutePhase::Ready;
        let armed = route
            .arm_result(
                uuid::Uuid::from_u128(60 + index as u128),
                1,
                Arc::clone(&schema),
                None,
            )
            .unwrap();
        let mut message = tool_call(
            60 + index as u64,
            RESULT_TOOL,
            json!({"value": {"decision": "approve"}}),
        );
        message["params"]["task"] = task;

        let response = dispatch_json(&route, route.acquire_result_request(), message).await;

        assert_eq!(response.pointer("/result/isError"), Some(&json!(false)));
        assert!(armed.accepted_result().is_some());
    }
}

#[tokio::test]
async fn tools_call_echoes_large_string_ids_without_changing_submission_semantics() {
    let schema = {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| Arc::new(compiled_result_schema(py)))
    };
    let id_for_response_size = |target: usize, escaped: bool| {
        let fixed = encoded_validation_response_size(&json!(""), &[], true, 1);
        assert!(target >= fixed);
        let extra = target - fixed;
        let value = if escaped {
            let mut value = "\\".repeat(extra / 2);
            if !extra.is_multiple_of(2) {
                value.push('x');
            }
            value
        } else {
            "x".repeat(extra)
        };
        let id = json!(value);
        assert_eq!(encoded_validation_response_size(&id, &[], true, 1), target);
        id
    };

    for escaped in [false, true] {
        for (case, value, expected_error) in
            [("invalid", "maybe", true), ("accepted", "approve", false)]
        {
            let route = route();
            *lock(&route.phase) = RoutePhase::Ready;
            let armed = route
                .arm_result(uuid::Uuid::new_v4(), 1, Arc::clone(&schema), None)
                .unwrap();
            let id = id_for_response_size(VALIDATION_DETAIL_MAX_BYTES + 1, escaped);
            let message = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": RESULT_TOOL,
                    "arguments": {"value": {"decision": value}},
                },
            });
            let response = dispatch_json(&route, route.acquire_result_request(), message).await;

            assert_eq!(response.get("id"), Some(&id), "{case} response changed id");
            assert_eq!(
                response.pointer("/result/isError"),
                Some(&json!(expected_error)),
                "{case} response changed submission outcome",
            );
            if expected_error {
                assert!(
                    response
                        .pointer("/result/content/0/text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains("invalid call 1/8"))
                );
                assert!(armed.accepted_result().is_none());
            } else {
                assert!(armed.accepted_result().is_some());
            }
            armed.disarm();
        }

        let route = route();
        *lock(&route.phase) = RoutePhase::Ready;
        let armed = route
            .arm_result(uuid::Uuid::new_v4(), 1, Arc::clone(&schema), None)
            .unwrap();
        let id = id_for_response_size(VALIDATION_DETAIL_MAX_BYTES + 1, escaped);
        let response = dispatch_json(
            &route,
            route.acquire_result_request(),
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "not-a-result-tool",
                    "arguments": {"value": {"decision": "approve"}},
                },
            }),
        )
        .await;
        assert_eq!(response.get("id"), Some(&id));
        assert_eq!(response.pointer("/error/code"), Some(&json!(-32602)));
        assert!(armed.accepted_result().is_none());
        armed.disarm();
    }
}

#[test]
fn authentication_snapshots_the_result_lease_before_phase_or_body_work() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let service = ResultMcpService::new();
        let route = route();
        *lock(&route.phase) = RoutePhase::ToolsListWriting;
        lock(&service.routes).insert(route.token.clone(), Arc::clone(&route));
        let first = route
            .arm_result(
                uuid::Uuid::from_u128(20),
                1,
                Arc::new(compiled_result_schema(py)),
                None,
            )
            .unwrap();
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            hyper::header::HeaderValue::from_static("Bearer test-token"),
        );
        let control = ConnectionControl::new();

        let (bound, request) = service
            .bind_authorized_request(&headers, &control)
            .expect("authentication captures one route and result lease");
        assert!(Arc::ptr_eq(&bound, &route));
        first.disarm();
        let second = route
            .arm_result(
                uuid::Uuid::from_u128(21),
                2,
                Arc::new(compiled_result_schema(py)),
                None,
            )
            .unwrap();

        assert_eq!(
            request.submit_value(&json!({"decision": "approve"})),
            ResultSubmission::TurnUnavailable,
        );
        assert_eq!(second.accepted_result(), None);
    });
}

#[test]
fn settlement_and_result_acceptance_share_one_linearization_boundary() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let route = route();
        let losing_arm = route
            .arm_result(
                uuid::Uuid::from_u128(30),
                1,
                Arc::new(compiled_result_schema(py)),
                None,
            )
            .unwrap();
        let request = route.acquire_result_request();
        let ResultSubmissionStart::Validated(late_candidate) =
            request.start_submission(&json!({"decision": "approve"}))
        else {
            panic!("valid native data creates a candidate");
        };

        assert!(matches!(losing_arm.settle(), ResultAtSettlement::Missing));
        assert_eq!(late_candidate.accept(), ResultSubmission::TurnUnavailable);

        let winning_arm = route
            .arm_result(
                uuid::Uuid::from_u128(31),
                2,
                Arc::new(compiled_result_schema(py)),
                None,
            )
            .unwrap();
        let request = route.acquire_result_request();
        assert_eq!(
            request.submit_value(&json!({"decision": "approve"})),
            ResultSubmission::Accepted,
        );
        assert!(matches!(
            winning_arm.settle(),
            ResultAtSettlement::Accepted(ValidatedActValue::Object(fields))
                if fields == vec![(
                    "decision".to_owned(),
                    ValidatedActValue::String("approve".to_owned()),
                )]
        ));
    });
}

#[test]
fn callback_fault_is_terminal_without_consuming_invalid_budget() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let route = route();
        let armed = route
            .arm_result(
                uuid::Uuid::from_u128(32),
                1,
                Arc::new(compiled_result_schema(py)),
                None,
            )
            .unwrap();
        let request = route.acquire_result_request();
        let ResultSubmissionStart::Validated(candidate) =
            request.start_submission(&json!({"decision": "approve"}))
        else {
            panic!("valid native data creates a candidate");
        };

        assert_eq!(
            candidate.callback_failed(pyo3::exceptions::PyRuntimeError::new_err("fault")),
            ResultSubmission::SchemaCallbackFailed,
        );
        assert!(matches!(
            armed.settle(),
            ResultAtSettlement::SchemaCallbackFailed(_)
        ));
    });
}

fn complete_http_response_length(response: &[u8]) -> Option<usize> {
    let header_end = response.windows(4).position(|part| part == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&response[..header_end]).ok()?;
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())?
    })?;
    Some(header_end + 4 + content_length)
}

#[tokio::test]
async fn lifecycle_transition_commits_when_response_flush_completes() {
    let service = ResultMcpService::new();
    let route = route();
    lock(&service.routes).insert(route.token.clone(), Arc::clone(&route));
    let control = ConnectionControl::new();
    let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
    let io = TokioIo::new(ResponseTrackedIo::new(server_io, Arc::clone(&control)));
    let service_for_request = Arc::clone(&service);
    let control_for_request = Arc::clone(&control);
    let connection = http1::Builder::new().serve_connection(
        io,
        service_fn(move |request| {
            let service = Arc::clone(&service_for_request);
            let control = Arc::clone(&control_for_request);
            async move { service.handle(request, control).await }
        }),
    );
    let server = tokio::spawn({
        let service = Arc::clone(&service);
        let control = Arc::clone(&control);
        async move { complete_connection(connection, control, &service.shutdown).await }
    });

    let body = serde_json::to_vec(&initialize()).expect("initialize serializes");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    client_io
        .write_all(request.as_bytes())
        .await
        .expect("request headers are written");
    client_io
        .write_all(&body)
        .await
        .expect("request body is written");

    let mut response = Vec::new();
    loop {
        if complete_http_response_length(&response).is_some_and(|length| response.len() >= length) {
            break;
        }
        let read = client_io
            .read_buf(&mut response)
            .await
            .expect("response is readable");
        assert_ne!(read, 0, "connection stays live through the response");
    }

    assert_eq!(*lock(&route.phase), RoutePhase::Initialized);
    drop(client_io);
    server.await.expect("server task completes");
}

#[tokio::test]
async fn failed_response_flush_reverts_before_a_pipelined_request_is_dispatched() {
    let service = ResultMcpService::new();
    let route = route();
    lock(&service.routes).insert(route.token.clone(), Arc::clone(&route));
    let control = ConnectionControl::new();
    let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
    let io = TokioIo::new(ResponseTrackedIo::new(
        FailFlushIo(server_io),
        Arc::clone(&control),
    ));
    let service_for_request = Arc::clone(&service);
    let control_for_request = Arc::clone(&control);
    let connection = http1::Builder::new().serve_connection(
        io,
        service_fn(move |request| {
            let service = Arc::clone(&service_for_request);
            let control = Arc::clone(&control_for_request);
            async move { service.handle(request, control).await }
        }),
    );
    let server = tokio::spawn({
        let service = Arc::clone(&service);
        let control = Arc::clone(&control);
        async move { complete_connection(connection, control, &service.shutdown).await }
    });

    let initialize_body = serde_json::to_vec(&initialize()).expect("initialize serializes");
    let initialized_body = br#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    let requests = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nMCP-Protocol-Version: {}\r\nContent-Length: {}\r\n\r\n{}",
        initialize_body.len(),
        std::str::from_utf8(&initialize_body).expect("JSON is UTF-8"),
        MCP_REVISION,
        initialized_body.len(),
        std::str::from_utf8(initialized_body).expect("JSON is UTF-8"),
    );
    client_io
        .write_all(requests.as_bytes())
        .await
        .expect("pipelined requests fit in the test transport");

    server
        .await
        .expect("flush failure is handled by the server");
    assert_eq!(*lock(&route.phase), RoutePhase::New);
}

#[tokio::test]
async fn auth_route_drain_drops_connection_before_releasing_its_route_lease() {
    let route = route();
    let control = ConnectionControl::new();
    assert!(control.bind_route(&route));
    route.revoke();
    let dropped = Arc::new(AtomicBool::new(false));
    let shutdown = CancellationToken::new();

    complete_connection(
        PendingConnection {
            route: Arc::clone(&route),
            dropped: Arc::clone(&dropped),
        },
        Arc::clone(&control),
        &shutdown,
    )
    .await;

    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(*lock(&route.connections), 0);
}

#[tokio::test]
async fn shutdown_waits_for_every_accepted_connection_task() {
    let service = ResultMcpService::new();
    let connection_task = service.begin_connection_task();
    let shutdown = tokio::spawn({
        let service = Arc::clone(&service);
        async move { service.shutdown_and_wait().await }
    });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());

    drop(connection_task);

    shutdown.await.expect("shutdown task completes");
}

#[tokio::test]
async fn revoked_generation_cannot_bind_or_complete_its_successor() {
    let service = ResultMcpService::new();
    let old = route_with("old-token", 1);
    let successor = route_with("successor-token", 2);
    lock(&service.routes).insert(old.token.clone(), Arc::clone(&old));
    let stale_transition = dispatch(&old, initialize())
        .transition
        .expect("initialize has a response transition");
    service.revoke_route(&old).await;
    lock(&service.routes).insert(successor.token.clone(), Arc::clone(&successor));

    let mut stale_headers = hyper::HeaderMap::new();
    stale_headers.insert(
        AUTHORIZATION,
        hyper::header::HeaderValue::from_static("Bearer old-token"),
    );
    let stale_control = ConnectionControl::new();
    assert!(
        service
            .bind_authorized_request(&stale_headers, &stale_control)
            .is_none()
    );
    stale_transition.finish(true);
    assert_eq!(*lock(&old.phase), RoutePhase::Revoked);
    assert_eq!(*lock(&successor.phase), RoutePhase::New);
    assert_eq!(*lock(&successor.connections), 0);

    let mut successor_headers = hyper::HeaderMap::new();
    successor_headers.insert(
        AUTHORIZATION,
        hyper::header::HeaderValue::from_static("Bearer successor-token"),
    );
    let successor_control = ConnectionControl::new();
    let (bound, _) = service
        .bind_authorized_request(&successor_headers, &successor_control)
        .expect("the successor bearer remains live");
    assert!(Arc::ptr_eq(&bound, &successor));
    assert_eq!(*lock(&successor.connections), 1);
    drop(successor_control);
    assert_eq!(*lock(&successor.connections), 0);
}

#[test]
fn transport_headers_reject_mcp_session_id_without_touching_route_state() {
    let service = ResultMcpService::new();
    *lock(&service.state) = ServiceState::Ready {
        endpoint: "http://127.0.0.1:1/mcp".to_owned(),
        origin: "http://127.0.0.1:1".to_owned(),
    };
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        ACCEPT,
        hyper::header::HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(
        "mcp-session-id",
        hyper::header::HeaderValue::from_static("forbidden"),
    );

    assert!(!common_transport_headers_valid(&service, &headers));
}

#[tokio::test]
async fn origin_cardinality_is_enforced_before_authorization_and_body_polling() {
    for origins in [vec![], vec!["http://127.0.0.1:1"]] {
        let service = ResultMcpService::new();
        *lock(&service.state) = ServiceState::Ready {
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            origin: "http://127.0.0.1:1".to_owned(),
        };
        let route = route();
        lock(&service.routes).insert(route.token.clone(), Arc::clone(&route));
        let control = ConnectionControl::new();
        let body = Full::new(Bytes::from(
            serde_json::to_vec(&initialize()).expect("initialize serializes"),
        ));

        let outcome = service
            .handle_inner(origin_request(body, &origins), &control)
            .await;

        assert_eq!(outcome.response.status(), StatusCode::OK);
        outcome
            .transition
            .expect("accepted initialize has a transition")
            .finish(true);
        assert_eq!(*lock(&route.phase), RoutePhase::Initialized);
        drop(control);
        assert_eq!(*lock(&route.connections), 0);
    }

    for origins in [
        vec!["http://127.0.0.1:2"],
        vec!["http://127.0.0.1:1", "http://127.0.0.1:2"],
    ] {
        let service = ResultMcpService::new();
        *lock(&service.state) = ServiceState::Ready {
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            origin: "http://127.0.0.1:1".to_owned(),
        };
        let route = route();
        lock(&service.routes).insert(route.token.clone(), Arc::clone(&route));
        let control = ConnectionControl::new();
        let polled = Arc::new(AtomicBool::new(false));
        let body = ProbeBody {
            polled: Arc::clone(&polled),
        };

        let outcome = service
            .handle_inner(origin_request(body, &origins), &control)
            .await;

        assert_eq!(outcome.response.status(), StatusCode::BAD_REQUEST);
        assert!(outcome.transition.is_none());
        assert!(!polled.load(Ordering::Acquire));
        assert_eq!(*lock(&route.phase), RoutePhase::New);
        assert_eq!(*lock(&route.connections), 0);
    }
}

#[test]
fn accept_negotiation_honors_case_and_positive_quality() {
    let service = ResultMcpService::new();
    *lock(&service.state) = ServiceState::Ready {
        endpoint: "http://127.0.0.1:1/mcp".to_owned(),
        origin: "http://127.0.0.1:1".to_owned(),
    };
    let valid = |accept: &'static str| {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            hyper::header::HeaderValue::from_static("Application/JSON; charset=utf-8"),
        );
        headers.insert(ACCEPT, hyper::header::HeaderValue::from_static(accept));
        common_transport_headers_valid(&service, &headers)
    };

    assert!(valid("Application/JSON;Q=0.5, Text/Event-Stream; q=1.0"));
    assert!(valid("application/json;q=0.001, text/event-stream;q=1.000"));
    assert!(!valid("application/json;q=0, text/event-stream"));
    assert!(!valid("application/json, text/event-stream; q=0.000"));
    assert!(!valid("application/json;q=.5, text/event-stream"));
    assert!(!valid("application/json;q=0.0001, text/event-stream"));
    assert!(!valid("application/json;q=1e0, text/event-stream"));
    assert!(!valid("application/json;q=0.5;q=0.8, text/event-stream"));
}

#[test]
fn initialize_requires_typed_capabilities_and_client_info_without_advancing() {
    let invalid_params = [
        json!({"protocolVersion": MCP_REVISION}),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": [],
            "clientInfo": {"name": "client", "version": "1"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {"name": 1, "version": "1"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {"name": "client"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {"experimental": {"vendor": 1}},
            "clientInfo": {"name": "client", "version": "1"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {"roots": 1},
            "clientInfo": {"name": "client", "version": "1"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {"roots": {"listChanged": "yes"}},
            "clientInfo": {"name": "client", "version": "1"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {"sampling": {"tools": true}},
            "clientInfo": {"name": "client", "version": "1"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {"elicitation": {"form": []}},
            "clientInfo": {"name": "client", "version": "1"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {"tasks": {"requests": {"sampling": 1}}},
            "clientInfo": {"name": "client", "version": "1"},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {"name": "client", "version": "1", "title": 1},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {"name": "client", "version": "1", "icons": {}},
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {
                "name": "client",
                "version": "1",
                "icons": [{"src": 1}],
            },
        }),
    ];

    for params in invalid_params {
        let route = route();
        let outcome = dispatch(
            &route,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": params,
            }),
        );
        assert!(outcome.transition.is_none());
        assert_eq!(*lock(&route.phase), RoutePhase::New);
    }

    let valid_params = [
        json!({
            "protocolVersion": MCP_REVISION,
            "_meta": {"progressToken": "opening", "vendor": true},
            "capabilities": {
                "experimental": {"vendor": {}},
                "roots": {"listChanged": true},
                "sampling": {"context": {}, "tools": {}},
                "elicitation": {"form": {}, "url": {}},
                "tasks": {
                    "list": {},
                    "cancel": {},
                    "requests": {
                        "sampling": {"createMessage": {}},
                        "elicitation": {"create": {}},
                    },
                },
            },
            "clientInfo": {
                "name": "client",
                "title": "Client",
                "version": "1",
                "description": "test client",
                "websiteUrl": "https://example.invalid",
                "icons": [{
                    "src": "data:image/png;base64,AA==",
                    "mimeType": "image/png",
                    "sizes": ["16x16", "any"],
                    "theme": "dark",
                }],
            },
        }),
        json!({
            "protocolVersion": MCP_REVISION,
            "capabilities": {"vendor.example/capability": {"shape": "opaque"}},
            "clientInfo": {
                "name": "client",
                "version": "1",
                "vendorMetadata": [1, 2, 3],
            },
        }),
    ];
    for params in valid_params {
        let route = route();
        let outcome = dispatch(
            &route,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": params,
            }),
        );
        assert!(outcome.transition.is_some());
        assert_eq!(*lock(&route.phase), RoutePhase::InitializeWriting);
    }
}

#[tokio::test]
async fn route_enforces_its_adapter_pinned_protocol_revision() {
    const CODEX_REVISION: &str = "2025-06-18";
    let route = route_with_revision("test-token", 1, CODEX_REVISION);
    let outcome = dispatch(
        &route,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": CODEX_REVISION,
                "capabilities": {},
                "clientInfo": {"name": "codex-mcp-client", "version": "0.145.0"},
            },
        }),
    );
    let transition = outcome
        .transition
        .expect("the adapter-pinned revision initializes the route");
    let body = outcome
        .response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        response.pointer("/result/protocolVersion"),
        Some(&json!(CODEX_REVISION)),
    );
    transition.finish(true);

    let matching = hyper::HeaderMap::from_iter([(
        hyper::header::HeaderName::from_static("mcp-protocol-version"),
        hyper::header::HeaderValue::from_static(CODEX_REVISION),
    )]);
    assert!(protocol_header_valid(
        &matching,
        RoutePhase::Initialized,
        route.mcp_revision,
    ));

    let wrong = hyper::HeaderMap::from_iter([(
        hyper::header::HeaderName::from_static("mcp-protocol-version"),
        hyper::header::HeaderValue::from_static(MCP_REVISION),
    )]);
    assert!(!protocol_header_valid(
        &wrong,
        RoutePhase::Initialized,
        route.mcp_revision,
    ));
}

#[test]
fn validation_detail_enforces_the_encoded_thirty_two_kibibyte_boundary() {
    let generated_response_size = |id: &Value, issues: &[ValidationIssue], truncated: bool| {
        encoded_validation_response_size(id, issues, truncated, 1)
            - serde_json::to_vec(id).unwrap().len()
    };
    let issue_for_response_size = |id: &Value, size: usize| {
        let empty = [ValidationIssue {
            path: "/value".to_owned(),
            code: "custom_validation",
            message: String::new(),
        }];
        let fixed = generated_response_size(id, &empty, false);
        assert!(fixed < size);
        vec![ValidationIssue {
            path: "/value".to_owned(),
            code: "custom_validation",
            message: "x".repeat(size - fixed),
        }]
    };

    let ids = [
        json!(1),
        json!(format!("escaped-\\\"{}", "x".repeat(40_000))),
        serde_json::from_str::<Value>("184467440737095516160000000000000000000").unwrap(),
    ];
    for id in &ids {
        for size in [VALIDATION_DETAIL_MAX_BYTES - 1, VALIDATION_DETAIL_MAX_BYTES] {
            let original = issue_for_response_size(id, size);
            let expected_text = validation_tool_text(&original, false, 1);
            let (issues, truncated) = bound_validation_issues(original, false, 1);
            assert!(!truncated);
            assert_eq!(validation_tool_text(&issues, truncated, 1), expected_text);
            assert_eq!(generated_response_size(id, &issues, truncated), size);
        }
        let (issues, truncated) = bound_validation_issues(
            issue_for_response_size(id, VALIDATION_DETAIL_MAX_BYTES + 1),
            false,
            1,
        );
        assert!(truncated);
        assert!(generated_response_size(id, &issues, truncated) <= VALIDATION_DETAIL_MAX_BYTES);
    }

    let escaped = vec![ValidationIssue {
        path: "/value".to_owned(),
        code: "custom_validation",
        message: "\\\"".repeat(10_000),
    }];
    assert!(validation_tool_text(&escaped, false, 1).len() < VALIDATION_DETAIL_MAX_BYTES);
    let (issues, truncated) = bound_validation_issues(escaped, false, 1);
    assert!(truncated);
    assert!(generated_response_size(&json!(1), &issues, truncated) <= VALIDATION_DETAIL_MAX_BYTES);
}

#[test]
fn validation_issue_count_enforces_n_minus_one_n_and_n_plus_one() {
    let issues = |count: usize| {
        (0..count)
            .map(|index| ValidationIssue {
                path: format!("/field_{index}"),
                code: "type_mismatch",
                message: "expected string, got integer".to_owned(),
            })
            .collect::<Vec<_>>()
    };

    for count in [
        VALIDATION_DETAIL_MAX_ISSUES - 1,
        VALIDATION_DETAIL_MAX_ISSUES,
    ] {
        let (bounded, truncated) = bound_validation_issues(issues(count), false, 1);
        assert_eq!(bounded.len(), count);
        assert!(!truncated);
    }
    let (bounded, truncated) =
        bound_validation_issues(issues(VALIDATION_DETAIL_MAX_ISSUES + 1), false, 1);
    assert_eq!(bounded.len(), VALIDATION_DETAIL_MAX_ISSUES);
    assert!(truncated);
}

#[test]
fn live_connection_capacity_is_exactly_production_global_n() {
    let service = ResultMcpService::new();
    let first = Arc::clone(&service.connections)
        .try_acquire_many_owned((MAX_CONNECTIONS - 1) as u32)
        .expect("N-1 live connections fit");
    assert_eq!(service.connections.available_permits(), 1);
    let last = Arc::clone(&service.connections)
        .try_acquire_owned()
        .expect("the Nth live connection fits");
    assert_eq!(service.connections.available_permits(), 0);
    assert!(
        Arc::clone(&service.connections)
            .try_acquire_owned()
            .is_err()
    );
    drop(last);
    drop(first);
    assert_eq!(service.connections.available_permits(), MAX_CONNECTIONS);
}

#[test]
fn lifecycle_transition_commits_only_after_response_write_success() {
    let route = route();

    let failed = dispatch(&route, initialize());
    assert_eq!(*lock(&route.phase), RoutePhase::InitializeWriting);
    failed
        .transition
        .expect("initialize has a response transition")
        .finish(false);
    assert_eq!(*lock(&route.phase), RoutePhase::New);

    let initialized = dispatch(&route, initialize());
    initialized
        .transition
        .expect("initialize has a response transition")
        .finish(true);
    assert_eq!(*lock(&route.phase), RoutePhase::Initialized);

    let notification = dispatch(
        &route,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }),
    );
    notification
        .transition
        .expect("initialized notification has a response transition")
        .finish(true);
    assert_eq!(*lock(&route.phase), RoutePhase::ClientInitialized);

    let list = || {
        dispatch(
            &route,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        )
    };
    let failed = list();
    assert_eq!(*lock(&route.phase), RoutePhase::ToolsListWriting);
    failed
        .transition
        .expect("tools/list has a response transition")
        .finish(false);
    assert_eq!(*lock(&route.phase), RoutePhase::ClientInitialized);

    list()
        .transition
        .expect("tools/list has a response transition")
        .finish(true);
    assert_eq!(*lock(&route.phase), RoutePhase::Ready);
}

#[tokio::test]
async fn tools_list_declares_the_result_value_as_an_object() {
    let route = route();
    dispatch(&route, initialize())
        .transition
        .expect("initialize has a response transition")
        .finish(true);
    dispatch(
        &route,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }),
    )
    .transition
    .expect("initialized notification has a response transition")
    .finish(true);

    let outcome = dispatch(
        &route,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    let body = outcome
        .response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        response.pointer("/result/tools/0/inputSchema/properties/value/type"),
        Some(&json!("object")),
    );
}

#[test]
fn lifecycle_rejects_malformed_typed_parameters_without_advancing() {
    let initialized_route = || {
        let route = route();
        dispatch(&route, initialize())
            .transition
            .expect("initialize has a response transition")
            .finish(true);
        route
    };
    let client_initialized_route = || {
        let route = initialized_route();
        dispatch(
            &route,
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }),
        )
        .transition
        .expect("initialized notification has a response transition")
        .finish(true);
        route
    };

    for params in [json!(1), json!([]), json!({"_meta": 1})] {
        let route = initialized_route();
        let outcome = dispatch(
            &route,
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": params,
            }),
        );
        assert!(outcome.transition.is_none());
        assert_eq!(*lock(&route.phase), RoutePhase::Initialized);
    }

    for params in [
        json!(1),
        json!([]),
        json!({"cursor": 1}),
        json!({"_meta": 1}),
        json!({"_meta": {"progressToken": []}}),
    ] {
        let route = client_initialized_route();
        let outcome = dispatch(
            &route,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": params,
            }),
        );
        assert!(outcome.transition.is_none());
        assert_eq!(*lock(&route.phase), RoutePhase::ClientInitialized);
    }

    for params in [
        None,
        Some(json!({})),
        Some(json!({"_meta": {"trace": 1}, "extension": true})),
    ] {
        let route = initialized_route();
        let mut message = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        if let Some(params) = params {
            message["params"] = params;
        }
        assert!(dispatch(&route, message).transition.is_some());
    }

    for params in [
        None,
        Some(json!({})),
        Some(json!({"cursor": "next", "_meta": {"trace": 1}, "extension": true})),
    ] {
        let route = client_initialized_route();
        let mut message = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
        });
        if let Some(params) = params {
            message["params"] = params;
        }
        assert!(dispatch(&route, message).transition.is_some());
    }
}

#[tokio::test]
async fn body_collection_enforces_the_exact_eight_mibibyte_boundary() {
    for size in [MCP_HTTP_BODY_MAX_BYTES - 1, MCP_HTTP_BODY_MAX_BYTES] {
        let body = Full::new(Bytes::from(vec![0_u8; size]));
        let collected = collect_bounded_body(body).await.unwrap();
        assert_eq!(collected.len(), size);
    }
    let body = Full::new(Bytes::from(vec![0_u8; MCP_HTTP_BODY_MAX_BYTES + 1]));
    assert_eq!(
        collect_bounded_body(body).await,
        Err(BodyCollectionError::TooLarge),
    );
}

#[tokio::test]
async fn http_head_parser_enforces_the_exact_thirty_two_kibibyte_boundary() {
    async fn response_for_head_size(size: usize) -> (Vec<u8>, Option<String>) {
        let prefix = b"POST / HTTP/1.1\r\nHost: localhost\r\nX-Pad: ";
        let suffix = b"\r\n\r\n";
        let padding = size - prefix.len() - suffix.len();
        let mut request = Vec::with_capacity(size);
        request.extend_from_slice(prefix);
        request.extend(std::iter::repeat_n(b'x', padding));
        request.extend_from_slice(suffix);
        assert_eq!(request.len(), size);

        let (server_io, mut client_io) = tokio::io::duplex(128 * 1024);
        let control = ConnectionControl::new();
        let service_control = Arc::clone(&control);
        let connection = result_http1_builder().serve_connection(
            TokioIo::new(ResponseTrackedIo::new(server_io, control)),
            service_fn(move |request: Request<Incoming>| {
                let control = Arc::clone(&service_control);
                async move {
                    let framing = request
                        .body()
                        .size_hint()
                        .exact()
                        .map_or(RequestBodyFraming::Chunked, RequestBodyFraming::Fixed);
                    control.begin_request(framing);
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::new())))
                }
            }),
        );
        let server =
            tokio::spawn(async move { connection.await.map_err(|error| error.to_string()) });
        client_io.write_all(&request).await.unwrap();
        let mut response = Vec::new();
        loop {
            if complete_http_response_length(&response)
                .is_some_and(|length| response.len() >= length)
            {
                break;
            }
            let read = client_io.read_buf(&mut response).await.unwrap();
            if read == 0 {
                break;
            }
        }
        drop(client_io);
        let error = server.await.unwrap().err();
        (response, error)
    }

    for size in [MCP_HTTP_HEAD_MAX_BYTES - 1, MCP_HTTP_HEAD_MAX_BYTES] {
        let (response, error) = response_for_head_size(size).await;
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "response={response:?}, error={error:?}",
        );
    }
    let (response, error) = response_for_head_size(MCP_HTTP_HEAD_MAX_BYTES + 1).await;
    assert!(
        response.starts_with(b"HTTP/1.1 431 Request Header Fields Too Large\r\n"),
        "response={response:?}, error={error:?}",
    );
}

#[tokio::test]
async fn http_head_limit_restarts_after_fixed_and_chunked_keep_alive_bodies() {
    const EMPTY_BODY_REQUEST: &[u8] =
        b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n";
    const CHUNKED_BODY_REQUEST: &[u8] = b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n4;note=test\r\ndata\r\n0\r\nX-Trailer: done\r\n\r\n";

    fn exact_head(size: usize) -> Vec<u8> {
        let prefix = b"POST / HTTP/1.1\r\nHost: localhost\r\nX-Pad: ";
        let suffix = b"\r\n\r\n";
        let mut request = Vec::with_capacity(size);
        request.extend_from_slice(prefix);
        request.extend(std::iter::repeat_n(
            b'x',
            size - prefix.len() - suffix.len(),
        ));
        request.extend_from_slice(suffix);
        request
    }

    async fn read_response(stream: &mut tokio::io::DuplexStream) -> Vec<u8> {
        let mut response = Vec::new();
        loop {
            if complete_http_response_length(&response)
                .is_some_and(|length| response.len() >= length)
            {
                return response;
            }
            let read = stream.read_buf(&mut response).await.unwrap();
            assert_ne!(read, 0, "a complete response precedes connection close");
        }
    }

    async fn second_response(first_request: &[u8], second_head_size: usize) -> Vec<u8> {
        let (server_io, mut client_io) = tokio::io::duplex(128 * 1024);
        let control = ConnectionControl::new();
        let service_control = Arc::clone(&control);
        let connection = result_http1_builder().serve_connection(
            TokioIo::new(ResponseTrackedIo::new(server_io, control)),
            service_fn(move |request: Request<Incoming>| {
                let control = Arc::clone(&service_control);
                async move {
                    let framing = request
                        .body()
                        .size_hint()
                        .exact()
                        .map_or(RequestBodyFraming::Chunked, RequestBodyFraming::Fixed);
                    control.begin_request(framing);
                    request.into_body().collect().await.unwrap();
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::new())))
                }
            }),
        );
        let server = tokio::spawn(connection);

        client_io.write_all(first_request).await.unwrap();
        let first = read_response(&mut client_io).await;
        assert!(first.starts_with(b"HTTP/1.1 200 OK\r\n"));

        client_io
            .write_all(&exact_head(second_head_size))
            .await
            .unwrap();
        let second = read_response(&mut client_io).await;
        drop(client_io);
        let _ = server.await.unwrap();
        second
    }

    for first_request in [EMPTY_BODY_REQUEST, CHUNKED_BODY_REQUEST] {
        let accepted = second_response(first_request, MCP_HTTP_HEAD_MAX_BYTES).await;
        assert!(
            accepted.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "response={accepted:?}",
        );
        let rejected = second_response(first_request, MCP_HTTP_HEAD_MAX_BYTES + 1).await;
        assert!(
            rejected.starts_with(b"HTTP/1.1 431 Request Header Fields Too Large\r\n"),
            "response={rejected:?}",
        );
    }
}

#[test]
fn chunked_body_tracker_accepts_hyper_size_lws_extensions_and_trailers() {
    let mut tracker = ChunkedBodyTracker::new();
    let wire = b"4 \t;note=test\r\ndata\r\n0 \t\r\nX-Trailer: done\r\n\r\nnext";
    assert_eq!(
        tracker.consume(wire),
        ChunkedProgress::Complete(wire.len() - b"next".len()),
    );
}

#[test]
fn protocol_json_depth_enforces_the_exact_sixty_four_level_boundary() {
    let nested = |depth: usize| {
        let mut value = Value::Null;
        for _ in 1..depth {
            value = json!([value]);
        }
        value
    };
    assert!(json_depth_within(&nested(63), MCP_JSON_MAX_DEPTH));
    assert!(json_depth_within(&nested(64), MCP_JSON_MAX_DEPTH));
    assert!(!json_depth_within(&nested(65), MCP_JSON_MAX_DEPTH));
}
