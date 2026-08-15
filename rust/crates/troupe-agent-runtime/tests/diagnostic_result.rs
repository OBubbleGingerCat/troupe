use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule, PyTuple};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use troupe_agent_runtime::diagnostics::result::{
    AgentResultCandidate, AgentResultMetadata, ResultDiagnosticCancellationForTest,
    ResultDiagnosticSettlementForTest, ResultDiagnosticTestDriver,
};
use troupe_agent_runtime::{
    AgentDiagnosticDestination, AgentDiagnosticErrorCode, AgentDiagnosticFailureOwner,
    AgentDiagnosticObservation, AgentDiagnosticObserver, AgentDiagnosticObserverFailure,
    AgentSessionDiagnosticContext, AgentTurnDiagnosticIdentity, CompiledActSchema,
    MAX_REPAIRABLE_INVALID_CALLS, ResolvedAgentProfile, compile_act_schema, install_schema,
    resolve_agent_profile,
};
use troupe_diagnostics_core::kinds::{CounterKind, InstantKind};
use uuid::Uuid;

const VALIDATED_VALUE_SENTINEL: &str = "validated-value-must-not-survive";
const RAW_VALIDATION_SENTINEL: &str = "raw-validation-payload-must-not-survive";
const SCRIPT_SENTINEL: &str = "script-must-not-survive";
const TOOL_PAYLOAD_SENTINEL: &str = "tool-payload-must-not-survive";
const MCP_REVISION: &str = "2025-11-25";

struct ResultMcpClient {
    address: String,
    authorization: String,
    next_request_id: u64,
}

impl ResultMcpClient {
    fn new(endpoint: &str, authorization: &str) -> Self {
        let address = endpoint
            .strip_prefix("http://")
            .and_then(|url| url.strip_suffix("/mcp"))
            .expect("the Result MCP endpoint is a loopback HTTP URL")
            .to_owned();
        assert!(
            !authorization.contains(['\r', '\n']),
            "the generated authorization header is one line"
        );
        Self {
            address,
            authorization: authorization.to_owned(),
            next_request_id: 100,
        }
    }

    async fn initialize(&self) {
        self.post(
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": MCP_REVISION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "troupe-diagnostic-result-test",
                        "version": "1"
                    }
                }
            }),
            false,
        )
        .await;
        self.post(
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }),
            true,
        )
        .await;
        self.post(
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
            true,
        )
        .await;
    }

    async fn submit(&mut self, value: Value) -> Value {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .expect("the Result MCP test request ID remains bounded");
        self.post(
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {
                    "name": "troupe_submit_result",
                    "arguments": {"value": value}
                }
            }),
            true,
        )
        .await
        .expect("a Result MCP tools/call response has a JSON body")
    }

    async fn post(&self, message: &Value, include_protocol_version: bool) -> Option<Value> {
        let body = serde_json::to_vec(message).expect("the Result MCP test request serializes");
        let protocol_header =
            include_protocol_version.then(|| format!("MCP-Protocol-Version: {MCP_REVISION}\r\n"));
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nAuthorization: {}\r\nContent-Type: application/json\r\n{}Connection: close\r\nContent-Length: {}\r\n\r\n",
            self.address,
            self.authorization,
            protocol_header.as_deref().unwrap_or_default(),
            body.len(),
        );
        let mut stream = TcpStream::connect(&self.address)
            .await
            .expect("the Result MCP test client connects");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("the Result MCP test request headers are written");
        stream
            .write_all(&body)
            .await
            .expect("the Result MCP test request body is written");
        stream
            .shutdown()
            .await
            .expect("the Result MCP test request write side closes");

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("the Result MCP test response is readable");
        let header_end = response
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .expect("the Result MCP test response has HTTP headers");
        let headers = std::str::from_utf8(&response[..header_end])
            .expect("the Result MCP test response headers are UTF-8");
        let status = headers
            .lines()
            .next()
            .expect("the Result MCP test response has a status line");
        assert!(
            status == "HTTP/1.1 200 OK" || status == "HTTP/1.1 202 Accepted",
            "unexpected Result MCP test status: {status}"
        );
        let body = &response[header_end + 4..];
        (!body.is_empty()).then(|| {
            serde_json::from_slice(body).expect("the Result MCP test response body is JSON")
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Default)]
struct RecordingDestination {
    observations: Mutex<Vec<AgentDiagnosticObservation>>,
}

impl RecordingDestination {
    fn take_result_candidates(&self) -> Vec<AgentResultCandidate> {
        std::mem::take(&mut *lock(&self.observations))
            .into_iter()
            .map(|observation| match observation {
                AgentDiagnosticObservation::Candidate(candidate) => candidate
                    .as_any()
                    .downcast_ref::<AgentResultCandidate>()
                    .expect("the focused driver emits only result candidates")
                    .clone(),
                other => panic!("unexpected non-result observation: {other:?}"),
            })
            .collect()
    }
}

impl AgentDiagnosticDestination for RecordingDestination {
    fn try_observe(
        &self,
        observation: AgentDiagnosticObservation,
    ) -> Result<(), AgentDiagnosticErrorCode> {
        lock(&self.observations).push(observation);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingFailureOwner {
    failures: Mutex<Vec<AgentDiagnosticObserverFailure>>,
}

impl AgentDiagnosticFailureOwner for RecordingFailureOwner {
    fn observer_failed(&self, failure: AgentDiagnosticObserverFailure) {
        lock(&self.failures).push(failure);
    }
}

fn observer(
    destination: &Arc<RecordingDestination>,
    failure_owner: &Arc<RecordingFailureOwner>,
) -> AgentDiagnosticObserver {
    AgentDiagnosticObserver::new(Arc::clone(destination), Arc::clone(failure_owner))
}

fn python_inputs() -> (ResolvedAgentProfile, Arc<CompiledActSchema>) {
    Python::initialize();
    Python::attach(|py| {
        let troupe = PyModule::new(py, "troupe").expect("create troupe test module");
        troupe
            .setattr("__path__", PyList::empty(py))
            .expect("make troupe a package");
        install_schema(&troupe).expect("install act schema");

        let profile_type = py
            .import("builtins")
            .expect("import builtins")
            .getattr("type")
            .expect("resolve type")
            .call1(("AgentProfile", PyTuple::empty(py), PyDict::new(py)))
            .expect("create AgentProfile test type");
        troupe
            .setattr("AgentProfile", &profile_type)
            .expect("install AgentProfile test type");
        py.import("sys")
            .expect("import sys")
            .getattr("modules")
            .expect("resolve sys.modules")
            .cast_into::<PyDict>()
            .expect("sys.modules is a dict")
            .set_item("troupe", &troupe)
            .expect("install troupe test module");

        let profile = profile_type
            .call0()
            .expect("create AgentProfile test value");
        profile.setattr("agent", "codex").expect("set agent");
        profile
            .setattr("model", "diagnostic-result-model")
            .expect("set model");
        profile.setattr("effort", py.None()).expect("set effort");
        profile
            .setattr(
                "workspace",
                std::env::current_dir()
                    .expect("resolve current directory")
                    .to_str()
                    .expect("workspace path is UTF-8"),
            )
            .expect("set workspace");
        let profile = resolve_agent_profile(&profile).expect("resolve test profile");

        let act_schema = troupe.getattr("act_schema").expect("resolve act schema");
        let string_value = |description: &str, choices: Option<Vec<&str>>| {
            let keywords = PyDict::new(py);
            keywords
                .set_item("description", description)
                .expect("set field description");
            if let Some(choices) = choices {
                keywords
                    .set_item("choices", choices)
                    .expect("set field choices");
            }
            act_schema
                .getattr("StrValue")
                .expect("resolve StrValue")
                .call((), Some(&keywords))
                .expect("create string field")
        };
        let schema = PyDict::new(py);
        schema
            .set_item(
                "decision",
                string_value("decision", Some(vec!["approve", "reject"])),
            )
            .expect("set decision field");
        schema
            .set_item("validated_value", string_value("validated value", None))
            .expect("set validated value field");
        schema
            .set_item("script_value", string_value("script value", None))
            .expect("set script value field");
        schema
            .set_item("tool_value", string_value("tool value", None))
            .expect("set tool value field");
        let schema = Arc::new(compile_act_schema(schema.as_any()).expect("compile test schema"));
        (profile, schema)
    })
}

fn identity(act_id: &str) -> AgentTurnDiagnosticIdentity {
    AgentTurnDiagnosticIdentity::new(
        AgentSessionDiagnosticContext::new("actor-result", "session-result"),
        act_id.to_owned(),
        format!("turn-{act_id}"),
    )
}

fn submitted_value(decision: &str) -> Value {
    json!({
        "decision": decision,
        "validated_value": VALIDATED_VALUE_SENTINEL,
        "script_value": SCRIPT_SENTINEL,
        "tool_value": TOOL_PAYLOAD_SENTINEL
    })
}

fn response_is_error(response: &Value) -> bool {
    response
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .expect("tools/call response has isError")
}

fn response_text(response: &Value) -> &str {
    response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .expect("tools/call response has text")
}

fn kinds(candidates: &[AgentResultCandidate]) -> Vec<&'static str> {
    candidates
        .iter()
        .map(troupe_agent_runtime::AgentDiagnosticCandidate::kind)
        .collect()
}

fn assert_metadata(
    metadata: &AgentResultMetadata,
    act_id: &str,
    session_generation: u64,
    operation_id: Uuid,
    turn_index: u64,
) {
    assert_eq!(metadata.identity().session().actor_id(), "actor-result");
    assert_eq!(metadata.identity().session().session_id(), "session-result");
    assert_eq!(metadata.identity().act_id(), act_id);
    assert_eq!(metadata.identity().turn_id(), format!("turn-{act_id}"));
    assert_eq!(metadata.session_generation(), session_generation);
    assert_eq!(metadata.operation_id(), operation_id);
    assert_eq!(metadata.turn_index(), turn_index);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_result_mcp_state_machine_emits_ordered_bounded_diagnostics() {
    let (profile, schema) = python_inputs();
    let destination = Arc::new(RecordingDestination::default());
    let failure_owner = Arc::new(RecordingFailureOwner::default());
    let mut driver = ResultDiagnosticTestDriver::start_for_test(profile).await;
    let mut client =
        ResultMcpClient::new(driver.endpoint_for_test(), driver.authorization_for_test());
    client.initialize().await;
    driver.wait_ready_for_test().await;
    let session_generation = driver.session_generation_for_test();

    let operation_id = Uuid::from_u128(1);
    driver.arm_for_test(
        identity("act-zero"),
        operation_id,
        1,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    let accepted = client.submit(submitted_value("approve")).await;
    assert!(!response_is_error(&accepted));
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Accepted
    );
    let candidates = destination.take_result_candidates();
    assert_eq!(kinds(&candidates), ["result.submitted", "result.accepted"]);
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.validation_rejections().is_none())
    );
    for candidate in &candidates {
        assert_metadata(
            candidate.metadata(),
            "act-zero",
            session_generation,
            operation_id,
            1,
        );
    }

    let operation_id = Uuid::from_u128(2);
    driver.arm_for_test(
        identity("act-missing"),
        operation_id,
        2,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Missing
    );
    let candidates = destination.take_result_candidates();
    assert_eq!(kinds(&candidates), ["result.missing"]);
    let missing = candidates[0].transition().expect("missing is a transition");
    assert_eq!(missing.instant_kind(), InstantKind::ResultMissing);
    assert_eq!(missing.error_code(), Some("missing_result"));
    assert!(missing.issue().is_none());
    assert_metadata(
        missing.metadata(),
        "act-missing",
        session_generation,
        operation_id,
        2,
    );

    let operation_id = Uuid::from_u128(3);
    driver.arm_for_test(
        identity("act-one"),
        operation_id,
        3,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    let invalid = client
        .submit(submitted_value(RAW_VALIDATION_SENTINEL))
        .await;
    assert!(response_is_error(&invalid));
    let raw_validation_text = response_text(&invalid);
    assert!(raw_validation_text.contains("/decision"));
    assert!(raw_validation_text.contains("approve"));
    let accepted = client.submit(submitted_value("approve")).await;
    assert!(!response_is_error(&accepted));
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Accepted
    );
    let candidates = destination.take_result_candidates();
    assert_eq!(
        kinds(&candidates),
        [
            "result.submitted",
            "result.rejected",
            "result.validation_rejections",
            "result.repair_requested",
            "result.submitted",
            "result.accepted",
        ]
    );
    let rejected = candidates[1]
        .transition()
        .expect("rejection is a transition");
    assert_eq!(rejected.instant_kind(), InstantKind::ResultRejected);
    assert_eq!(rejected.error_code(), Some("invalid_result"));
    let issue = rejected.issue().expect("rejection keeps one stable issue");
    assert_eq!(issue.code(), "not_in_choices");
    assert_eq!(issue.path(), "/decision");
    let counter = candidates[2]
        .validation_rejections()
        .expect("rejection is followed by its counter");
    assert_eq!(
        counter.counter_kind(),
        CounterKind::ResultValidationRejections
    );
    assert_eq!(counter.value(), 1);
    assert_eq!(rejected.metadata(), counter.metadata());
    for candidate in &candidates {
        assert_metadata(
            candidate.metadata(),
            "act-one",
            session_generation,
            operation_id,
            3,
        );
    }
    let debug = format!("{candidates:?}");
    for excluded in [
        VALIDATED_VALUE_SENTINEL,
        RAW_VALIDATION_SENTINEL,
        SCRIPT_SENTINEL,
        TOOL_PAYLOAD_SENTINEL,
        raw_validation_text,
    ] {
        assert!(!debug.contains(excluded), "candidate retained {excluded}");
    }

    let operation_id = Uuid::from_u128(4);
    driver.arm_for_test(
        identity("act-terminal-n"),
        operation_id,
        4,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    let terminal_rejections = u64::from(MAX_REPAIRABLE_INVALID_CALLS) + 1;
    for _ in 0..terminal_rejections {
        assert!(response_is_error(
            &client
                .submit(submitted_value(RAW_VALIDATION_SENTINEL))
                .await
        ));
    }
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Rejected {
            validation_rejections: MAX_REPAIRABLE_INVALID_CALLS + 1,
        }
    );
    let candidates = destination.take_result_candidates();
    let counters = candidates
        .iter()
        .filter_map(AgentResultCandidate::validation_rejections)
        .collect::<Vec<_>>();
    assert_eq!(
        counters
            .iter()
            .map(|counter| counter.value())
            .collect::<Vec<_>>(),
        (1..=terminal_rejections).collect::<Vec<_>>()
    );
    assert_eq!(
        candidates
            .iter()
            .filter_map(AgentResultCandidate::transition)
            .filter(|candidate| candidate.instant_kind() == InstantKind::ResultSubmitted)
            .count(),
        terminal_rejections as usize
    );
    assert_eq!(
        candidates
            .iter()
            .filter_map(AgentResultCandidate::transition)
            .filter(|candidate| candidate.instant_kind() == InstantKind::ResultRejected)
            .count(),
        terminal_rejections as usize
    );
    assert_eq!(
        candidates
            .iter()
            .filter_map(AgentResultCandidate::transition)
            .filter(|candidate| candidate.instant_kind() == InstantKind::ResultRepairRequested)
            .count(),
        usize::from(MAX_REPAIRABLE_INVALID_CALLS)
    );
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate
            .transition()
            .is_some_and(|candidate| candidate.instant_kind() == InstantKind::ResultRejected)
        {
            assert!(
                candidates[index + 1].validation_rejections().is_some(),
                "every rejection transition is immediately followed by its counter"
            );
        }
        assert_metadata(
            candidate.metadata(),
            "act-terminal-n",
            session_generation,
            operation_id,
            4,
        );
    }

    let operation_id = Uuid::from_u128(5);
    driver.arm_for_test(
        identity("act-terminal-one"),
        operation_id,
        5,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    client
        .submit(submitted_value(RAW_VALIDATION_SENTINEL))
        .await;
    let before_terminal = destination.take_result_candidates();
    assert_eq!(
        kinds(&before_terminal),
        [
            "result.submitted",
            "result.rejected",
            "result.validation_rejections",
            "result.repair_requested",
        ]
    );
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Rejected {
            validation_rejections: 1,
        }
    );
    assert!(destination.take_result_candidates().is_empty());

    let operation_id = Uuid::from_u128(6);
    driver.arm_for_test(
        identity("act-cancel-zero"),
        operation_id,
        6,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    assert_eq!(
        driver.cancel_for_test(),
        ResultDiagnosticCancellationForTest::Cancelled
    );
    assert_eq!(
        driver.cancel_for_test(),
        ResultDiagnosticCancellationForTest::Cancelled
    );
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Unavailable
    );
    assert!(destination.take_result_candidates().is_empty());

    let operation_id = Uuid::from_u128(7);
    driver.arm_for_test(
        identity("act-cancel-one"),
        operation_id,
        7,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    client
        .submit(submitted_value(RAW_VALIDATION_SENTINEL))
        .await;
    let before_cancel = destination.take_result_candidates();
    assert_eq!(
        kinds(&before_cancel),
        [
            "result.submitted",
            "result.rejected",
            "result.validation_rejections",
            "result.repair_requested",
        ]
    );
    assert_eq!(
        driver.cancel_for_test(),
        ResultDiagnosticCancellationForTest::Cancelled
    );
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Unavailable
    );
    assert!(destination.take_result_candidates().is_empty());

    let operation_id = Uuid::from_u128(8);
    driver.arm_for_test(
        identity("act-cancel-accepted"),
        operation_id,
        8,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    client.submit(submitted_value("approve")).await;
    assert_eq!(
        driver.cancel_for_test(),
        ResultDiagnosticCancellationForTest::Cancelled
    );
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Unavailable
    );
    assert_eq!(
        kinds(&destination.take_result_candidates()),
        ["result.submitted", "result.accepted"]
    );

    let operation_id = Uuid::from_u128(9);
    driver.arm_for_test(
        identity("act-cancel-terminal"),
        operation_id,
        9,
        Arc::clone(&schema),
        observer(&destination, &failure_owner),
    );
    for _ in 0..terminal_rejections {
        client
            .submit(submitted_value(RAW_VALIDATION_SENTINEL))
            .await;
    }
    let before_cancel = destination.take_result_candidates();
    assert_eq!(
        before_cancel
            .iter()
            .filter_map(AgentResultCandidate::validation_rejections)
            .count(),
        terminal_rejections as usize
    );
    assert_eq!(
        driver.cancel_for_test(),
        ResultDiagnosticCancellationForTest::FailurePreceded
    );
    assert_eq!(
        driver.settle_for_test(),
        ResultDiagnosticSettlementForTest::Rejected {
            validation_rejections: MAX_REPAIRABLE_INVALID_CALLS + 1,
        }
    );
    assert!(destination.take_result_candidates().is_empty());
    assert!(lock(&failure_owner.failures).is_empty());

    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(crate_root.join("src/diagnostics/result.rs"))
        .expect("read result normalizer source");
    for required in [
        "ResultIssue::new(issue.code.to_owned(), issue.path.clone())",
        "InstantKind::ResultRejected",
        "CounterKind::ResultValidationRejections",
        "ResultMcpService::new()",
        "arm_result_with_diagnostics",
        "install_diagnostic_context",
    ] {
        assert!(
            source.contains(required),
            "missing real result seam: {required}"
        );
    }

    driver.shutdown_for_test().await;
}
