use std::ffi::{CString, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule, PyTuple};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use troupe_agent_runtime::diagnostics::result::{AgentResultCandidate, AgentResultMetadata};
use troupe_agent_runtime::{
    AgentActError, AgentCastPermit, AgentDiagnosticCandidate, AgentDiagnosticDestination,
    AgentDiagnosticErrorCode, AgentDiagnosticFailureOwner, AgentDiagnosticObservation,
    AgentDiagnosticObservationKind, AgentDiagnosticObserver, AgentDiagnosticObserverFailure,
    AgentSessionDiagnosticContext, AgentSupervisor, AgentTurnCancelDecision, AgentTurnControl,
    AgentTurnDiagnosticIdentity, AgentTurnDiagnosticMetadata, AgentTurnOutcome, CompiledActSchema,
    MAX_REPAIRABLE_INVALID_CALLS, PythonSchemaValidationBridge, ResolvedAgentProfile,
    ToolPayloadCapturePolicy, compile_act_schema, hold_test_turn_registration,
    hold_test_turn_settlement, install_schema, release_test_turn_registration,
    release_test_turn_settlement, reset_test_launch, resolve_agent_profile, set_test_launch,
    turn_gate_states,
};
use troupe_diagnostics_core::kinds::{CounterKind, InstantKind};

const VALIDATED_VALUE_SENTINEL: &str = "validated-value-must-not-survive";
const RAW_VALIDATION_SENTINEL: &str = "raw-validation-payload-must-not-survive";
const SCRIPT_SENTINEL: &str = "script-must-not-survive";
const TOOL_PAYLOAD_SENTINEL: &str = "tool-payload-must-not-survive";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-diagnostic-result-{}-{sequence}-{label}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create diagnostic result test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
enum ResultFailureMode {
    None,
    ErrorThenPanic,
}

struct RecordingDestination {
    observations: Mutex<Vec<AgentDiagnosticObservation>>,
    result_observations: AtomicUsize,
    failure_mode: ResultFailureMode,
}

impl Default for RecordingDestination {
    fn default() -> Self {
        Self {
            observations: Mutex::new(Vec::new()),
            result_observations: AtomicUsize::new(0),
            failure_mode: ResultFailureMode::None,
        }
    }
}

impl RecordingDestination {
    fn failing_result_observer() -> Self {
        Self {
            failure_mode: ResultFailureMode::ErrorThenPanic,
            ..Self::default()
        }
    }

    fn result_candidates(&self) -> Vec<AgentResultCandidate> {
        lock(&self.observations)
            .iter()
            .filter_map(|observation| {
                observation
                    .candidate()?
                    .as_any()
                    .downcast_ref::<AgentResultCandidate>()
                    .cloned()
            })
            .collect()
    }

    fn submitted_turns(&self) -> Vec<Arc<AgentTurnDiagnosticMetadata>> {
        lock(&self.observations)
            .iter()
            .filter_map(|observation| match observation {
                AgentDiagnosticObservation::TurnSubmitted(metadata) => Some(Arc::clone(metadata)),
                _ => None,
            })
            .collect()
    }
}

impl AgentDiagnosticDestination for RecordingDestination {
    fn try_observe(
        &self,
        observation: AgentDiagnosticObservation,
    ) -> Result<(), AgentDiagnosticErrorCode> {
        let is_result = observation
            .candidate()
            .is_some_and(|candidate| candidate.as_any().is::<AgentResultCandidate>());
        lock(&self.observations).push(observation);
        if !is_result {
            return Ok(());
        }
        let index = self.result_observations.fetch_add(1, Ordering::Relaxed);
        match (self.failure_mode, index) {
            (ResultFailureMode::ErrorThenPanic, 0) => {
                Err(AgentDiagnosticErrorCode::new("test_destination_failed"))
            }
            (ResultFailureMode::ErrorThenPanic, 1) => {
                panic!("intentional diagnostic destination panic")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Default)]
struct RecordingFailureOwner {
    failures: Mutex<Vec<AgentDiagnosticObserverFailure>>,
}

impl RecordingFailureOwner {
    fn failures(&self) -> Vec<AgentDiagnosticObserverFailure> {
        lock(&self.failures).clone()
    }
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

struct PythonFixtures {
    profile_type: Py<PyAny>,
    python_executable: PathBuf,
    native_schema: Arc<CompiledActSchema>,
    callback_schema: Arc<CompiledActSchema>,
}

impl PythonFixtures {
    fn new() -> Self {
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

            let modules = py
                .import("sys")
                .expect("import sys")
                .getattr("modules")
                .expect("resolve sys.modules")
                .cast_into::<PyDict>()
                .expect("sys.modules is a dict");
            modules
                .set_item("troupe", &troupe)
                .expect("install troupe test module");
            let act_schema = troupe.getattr("act_schema").expect("resolve act schema");
            modules
                .set_item("troupe.act_schema", &act_schema)
                .expect("install troupe.act_schema test module");

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
            let native = PyDict::new(py);
            native
                .set_item(
                    "decision",
                    string_value("decision", Some(vec!["approve", "reject"])),
                )
                .expect("set decision field");
            native
                .set_item("validated_value", string_value("validated value", None))
                .expect("set validated value field");
            native
                .set_item("script_value", string_value("script value", None))
                .expect("set script value field");
            native
                .set_item("tool_value", string_value("tool value", None))
                .expect("set tool value field");
            let native_schema =
                Arc::new(compile_act_schema(native.as_any()).expect("compile native schema"));

            let locals = PyDict::new(py);
            locals
                .set_item("act_schema", &act_schema)
                .expect("set custom schema module");
            let source = CString::new(
                r#"class FaultValue(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='faulting integer', json_kind='int64')
    def render_prompt(self):
        return 'must be an integer'
    def validate(self, value):
        del value
        raise LookupError('private callback detail must not reach diagnostics')

callback_schema = {'value': FaultValue()}
"#,
            )
            .expect("custom schema source contains no NUL");
            py.run(source.as_c_str(), Some(&locals), Some(&locals))
                .expect("define callback schema");
            let callback_schema = Arc::new(
                compile_act_schema(
                    &locals
                        .get_item("callback_schema")
                        .expect("read callback schema")
                        .expect("callback schema exists"),
                )
                .expect("compile callback schema"),
            );
            let python_executable = std::env::var_os("PYO3_PYTHON")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("python3"));
            Self {
                profile_type: profile_type.unbind(),
                python_executable,
                native_schema,
                callback_schema,
            }
        })
    }

    fn profile(&self, workspace: &Path) -> ResolvedAgentProfile {
        Python::attach(|py| {
            let profile = self
                .profile_type
                .bind(py)
                .call0()
                .expect("create AgentProfile test value");
            profile.setattr("agent", "codex").expect("set agent");
            profile.setattr("model", "test-model").expect("set model");
            profile.setattr("effort", "max").expect("set effort");
            profile
                .setattr(
                    "workspace",
                    workspace.to_str().expect("workspace path is UTF-8"),
                )
                .expect("set workspace");
            resolve_agent_profile(&profile).expect("resolve test profile")
        })
    }
}

struct PythonValidationLoop {
    bridge: Arc<PythonSchemaValidationBridge>,
    event_loop: Py<PyAny>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PythonValidationLoop {
    fn start() -> Self {
        type Ready = Result<(Arc<PythonSchemaValidationBridge>, Py<PyAny>), String>;
        let (sender, receiver) = mpsc::sync_channel::<Ready>(1);
        let thread = thread::spawn(move || {
            Python::attach(|py| {
                let setup = (|| -> PyResult<_> {
                    let asyncio = py.import("asyncio")?;
                    let event_loop = asyncio.call_method0("new_event_loop")?;
                    asyncio.call_method1("set_event_loop", (&event_loop,))?;
                    let events = py.import("asyncio.events")?;
                    events.call_method1("_set_running_loop", (&event_loop,))?;
                    let bridge = PythonSchemaValidationBridge::new(py)?;
                    events.call_method1("_set_running_loop", (py.None(),))?;
                    Ok((bridge, event_loop))
                })();
                let (bridge, event_loop) = match setup {
                    Ok(values) => values,
                    Err(error) => {
                        sender
                            .send(Err(error.to_string()))
                            .expect("report Python loop setup failure");
                        return;
                    }
                };
                sender
                    .send(Ok((Arc::clone(&bridge), event_loop.clone().unbind())))
                    .expect("publish Python validation loop");
                event_loop
                    .call_method0("run_forever")
                    .expect("run Python validation loop");
            });
        });
        let (bridge, event_loop) = receiver
            .recv_timeout(TEST_TIMEOUT)
            .expect("Python validation loop starts")
            .unwrap_or_else(|error| panic!("Python validation loop failed: {error}"));
        Self {
            bridge,
            event_loop,
            thread: Some(thread),
        }
    }

    fn bridge(&self) -> Arc<PythonSchemaValidationBridge> {
        Arc::clone(&self.bridge)
    }

    fn shutdown(mut self) {
        Python::attach(|py| {
            let event_loop = self.event_loop.bind(py);
            let stop = event_loop.getattr("stop").expect("resolve loop.stop");
            event_loop
                .call_method1("call_soon_threadsafe", (stop,))
                .expect("stop Python validation loop");
        });
        self.thread
            .take()
            .expect("Python validation loop thread exists")
            .join()
            .expect("Python validation loop exits cleanly");
    }
}

#[derive(Clone, Copy)]
enum HeldGate {
    None,
    Registration,
    Settlement,
}

struct SessionHarness {
    _directory: TestDirectory,
    events: PathBuf,
    supervisor: AgentSupervisor,
    permit: Option<AgentCastPermit>,
    slot: Arc<troupe_agent_runtime::AgentSessionSlot>,
    diagnostic_context: AgentSessionDiagnosticContext,
}

impl SessionHarness {
    fn start(
        fixtures: &PythonFixtures,
        label: &str,
        scenario: &str,
        results: Option<&[Value]>,
        held_gate: HeldGate,
        session_observer: AgentDiagnosticObserver,
    ) -> Self {
        reset_test_launch();
        let directory = TestDirectory::new(label);
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).expect("create mock workspace");
        let events = directory.path().join("events.jsonl");
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .expect("resolve repository root");
        let mut args = vec![
            repository
                .join("tests/support/mock_acp_agent.py")
                .into_os_string(),
            OsString::from("--events"),
            events.as_os_str().to_os_string(),
            OsString::from("--scenario"),
            OsString::from(scenario),
        ];
        if let Some(results) = results {
            args.push(OsString::from("--results-json"));
            args.push(OsString::from(
                serde_json::to_string(results).expect("serialize mock results"),
            ));
        }
        set_test_launch(fixtures.python_executable.clone(), args, None, None, None)
            .expect("configure mock agent launch");
        match held_gate {
            HeldGate::None => {}
            HeldGate::Registration => {
                hold_test_turn_registration().expect("hold turn registration")
            }
            HeldGate::Settlement => hold_test_turn_settlement().expect("hold turn settlement"),
        }

        let profile = Arc::new(fixtures.profile(&workspace));
        let supervisor = AgentSupervisor::new();
        supervisor
            .install_diagnostic_observer(session_observer)
            .expect("install Production diagnostic observer before opening");
        let permit = supervisor.begin_cast().expect("begin test cast");
        let launch = supervisor.resolve(&profile).expect("resolve mock launch");
        let diagnostic_context = AgentSessionDiagnosticContext::new(
            format!("actor-{label}"),
            format!("session-{label}"),
        );
        let slot = supervisor.start_with_diagnostic_context(
            &permit,
            profile,
            launch,
            diagnostic_context.clone(),
        );
        Self {
            _directory: directory,
            events,
            supervisor,
            permit: Some(permit),
            slot,
            diagnostic_context,
        }
    }

    fn spawn_turn(
        &self,
        act_id: &str,
        schema: Arc<CompiledActSchema>,
        validation_bridge: Option<Arc<PythonSchemaValidationBridge>>,
        standalone_observer: AgentDiagnosticObserver,
    ) -> (
        Arc<AgentTurnControl>,
        JoinHandle<Result<AgentTurnOutcome, AgentActError>>,
    ) {
        let admission = self
            .slot
            .try_claim_admission()
            .expect("the focused Act is admitted");
        let control = AgentTurnControl::new(Arc::clone(&self.slot));
        assert!(control.install_admission(admission));
        let identity = AgentTurnDiagnosticIdentity::new(
            self.diagnostic_context.clone(),
            act_id.to_owned(),
            format!("turn-{act_id}"),
        );
        let context = control.new_diagnostic_context(
            identity,
            Some(standalone_observer),
            ToolPayloadCapturePolicy::default(),
        );
        control
            .install_diagnostic_context(context)
            .expect("attach turn diagnostics to the real session");
        let slot = Arc::clone(&self.slot);
        let spawned_control = Arc::clone(&control);
        let prompt = format!("{SCRIPT_SENTINEL}: {act_id}");
        let task = tokio::spawn(async move {
            slot.run_turn(prompt, schema, validation_bridge, spawned_control)
                .await
        });
        (control, task)
    }

    fn event_texts(&self) -> Vec<String> {
        if !self.events.exists() {
            return Vec::new();
        }
        std::fs::read_to_string(&self.events)
            .expect("read mock agent events")
            .lines()
            .filter_map(|line| {
                serde_json::from_str::<Value>(line)
                    .expect("mock event is JSON")
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    async fn shutdown(mut self) {
        drop(self.permit.take());
        let outcome = timeout(TEST_TIMEOUT, self.supervisor.shutdown_and_wait()).await;
        reset_test_launch();
        outcome.expect("real session cleanup completes");
    }
}

fn submitted_value(decision: &str) -> Value {
    json!({
        "decision": decision,
        "validated_value": VALIDATED_VALUE_SENTINEL,
        "script_value": SCRIPT_SENTINEL,
        "tool_value": TOOL_PAYLOAD_SENTINEL
    })
}

fn kinds(candidates: &[AgentResultCandidate]) -> Vec<&'static str> {
    candidates
        .iter()
        .map(AgentDiagnosticCandidate::kind)
        .collect()
}

fn assert_real_session_handoff(
    destination: &RecordingDestination,
    standalone: &RecordingDestination,
    act_id: &str,
) -> Vec<AgentResultCandidate> {
    assert!(
        standalone.result_candidates().is_empty(),
        "the standalone Act observer must be replaced by the Production observer"
    );
    let candidates = destination.result_candidates();
    assert!(
        !candidates.is_empty(),
        "the real result path emits candidates"
    );
    let turns = destination.submitted_turns();
    assert_eq!(turns.len(), 1, "the real session path submits one turn");
    let turn = &turns[0];
    assert_eq!(turn.identity().act_id(), act_id);
    assert_eq!(turn.identity().turn_id(), format!("turn-{act_id}"));
    for candidate in &candidates {
        let metadata = candidate.metadata();
        assert_eq!(metadata.identity(), turn.identity());
        assert_eq!(metadata.session_generation(), turn.session_generation());
        assert_eq!(metadata.operation_id(), turn.operation_id());
        assert_eq!(metadata.turn_index(), turn.turn_index());
    }
    candidates
}

fn assert_issue(metadata: &AgentResultMetadata, expected_act_id: &str) {
    assert_eq!(metadata.identity().act_id(), expected_act_id);
    assert_eq!(metadata.turn_index(), 1);
}

async fn await_gate(gate: &str, state: &str) {
    timeout(TEST_TIMEOUT, async {
        loop {
            let reached = Python::attach(|py| {
                turn_gate_states(py)
                    .expect("read turn gate states")
                    .bind(py)
                    .get_item(gate)
                    .expect("read held turn gate")
                    .get_item(state)
                    .expect("read held turn gate state")
                    .extract::<bool>()
                    .expect("turn gate state is bool")
            });
            if reached {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("turn gate {gate}.{state} was not reached"));
}

async fn await_outcome(
    control: &AgentTurnControl,
    task: JoinHandle<Result<AgentTurnOutcome, AgentActError>>,
) -> Result<AgentTurnOutcome, AgentActError> {
    let outcome = timeout(TEST_TIMEOUT, task)
        .await
        .expect("the real agent turn completes")
        .expect("the real agent turn task does not panic");
    control.finish_caller();
    outcome
}

fn recording_pair() -> (
    Arc<RecordingDestination>,
    Arc<RecordingFailureOwner>,
    Arc<RecordingDestination>,
    Arc<RecordingFailureOwner>,
) {
    (
        Arc::new(RecordingDestination::default()),
        Arc::new(RecordingFailureOwner::default()),
        Arc::new(RecordingDestination::default()),
        Arc::new(RecordingFailureOwner::default()),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_result_path_emits_bounded_deterministic_diagnostics() {
    let fixtures = PythonFixtures::new();

    let destination = Arc::new(RecordingDestination::failing_result_observer());
    let failure_owner = Arc::new(RecordingFailureOwner::default());
    let standalone = Arc::new(RecordingDestination::default());
    let standalone_owner = Arc::new(RecordingFailureOwner::default());
    let harness = SessionHarness::start(
        &fixtures,
        "accepted",
        "act_submit_results",
        Some(&[submitted_value("approve")]),
        HeldGate::None,
        observer(&destination, &failure_owner),
    );
    let (control, task) = harness.spawn_turn(
        "act-accepted",
        Arc::clone(&fixtures.native_schema),
        None,
        observer(&standalone, &standalone_owner),
    );
    let accepted_outcome = await_outcome(&control, task).await;
    assert!(
        matches!(accepted_outcome, Ok(AgentTurnOutcome::Success(_))),
        "unexpected accepted outcome: {accepted_outcome:?}"
    );
    let candidates = assert_real_session_handoff(&destination, &standalone, "act-accepted");
    assert_eq!(kinds(&candidates), ["result.submitted", "result.accepted"]);
    let failures = failure_owner.failures();
    assert_eq!(failures.len(), 2);
    assert_eq!(
        failures[0].observation_kind(),
        AgentDiagnosticObservationKind::Candidate("result.submitted")
    );
    assert_eq!(failures[0].error_code().as_str(), "test_destination_failed");
    assert_eq!(
        failures[1].observation_kind(),
        AgentDiagnosticObservationKind::Candidate("result.accepted")
    );
    assert_eq!(failures[1].error_code().as_str(), "observer_panicked");
    assert!(standalone_owner.failures().is_empty());
    harness.shutdown().await;

    let (destination, failure_owner, standalone, standalone_owner) = recording_pair();
    let harness = SessionHarness::start(
        &fixtures,
        "missing",
        "act_no_result",
        None,
        HeldGate::None,
        observer(&destination, &failure_owner),
    );
    let (control, task) = harness.spawn_turn(
        "act-missing",
        Arc::clone(&fixtures.native_schema),
        None,
        observer(&standalone, &standalone_owner),
    );
    assert!(matches!(
        await_outcome(&control, task).await,
        Ok(AgentTurnOutcome::MissingResult)
    ));
    let candidates = assert_real_session_handoff(&destination, &standalone, "act-missing");
    assert_eq!(kinds(&candidates), ["result.missing"]);
    let missing = candidates[0].transition().expect("missing is a transition");
    assert_eq!(missing.instant_kind(), InstantKind::ResultMissing);
    assert_eq!(missing.error_code(), Some("missing_result"));
    assert!(missing.issue().is_none());
    assert!(failure_owner.failures().is_empty());
    assert!(standalone_owner.failures().is_empty());
    harness.shutdown().await;

    let (destination, failure_owner, standalone, standalone_owner) = recording_pair();
    let harness = SessionHarness::start(
        &fixtures,
        "repair",
        "act_submit_results",
        Some(&[
            submitted_value(RAW_VALIDATION_SENTINEL),
            submitted_value("approve"),
        ]),
        HeldGate::None,
        observer(&destination, &failure_owner),
    );
    let (control, task) = harness.spawn_turn(
        "act-repair",
        Arc::clone(&fixtures.native_schema),
        None,
        observer(&standalone, &standalone_owner),
    );
    assert!(matches!(
        await_outcome(&control, task).await,
        Ok(AgentTurnOutcome::Success(_))
    ));
    let candidates = assert_real_session_handoff(&destination, &standalone, "act-repair");
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
    assert_issue(rejected.metadata(), "act-repair");
    let diagnostic_debug = format!("{candidates:?}");
    let raw_tool_text = harness
        .event_texts()
        .into_iter()
        .find(|text| text.contains("/decision"))
        .expect("mock agent records raw validation response text");
    for excluded in [
        VALIDATED_VALUE_SENTINEL,
        RAW_VALIDATION_SENTINEL,
        SCRIPT_SENTINEL,
        TOOL_PAYLOAD_SENTINEL,
        raw_tool_text.as_str(),
    ] {
        assert!(
            !diagnostic_debug.contains(excluded),
            "candidate retained excluded content: {excluded}"
        );
    }
    assert!(failure_owner.failures().is_empty());
    assert!(standalone_owner.failures().is_empty());
    harness.shutdown().await;

    let (destination, failure_owner, standalone, standalone_owner) = recording_pair();
    let harness = SessionHarness::start(
        &fixtures,
        "one-terminal",
        "act_submit_results",
        Some(&[submitted_value(RAW_VALIDATION_SENTINEL)]),
        HeldGate::None,
        observer(&destination, &failure_owner),
    );
    let (control, task) = harness.spawn_turn(
        "act-one-terminal",
        Arc::clone(&fixtures.native_schema),
        None,
        observer(&standalone, &standalone_owner),
    );
    let outcome = await_outcome(&control, task).await;
    assert!(matches!(
        outcome,
        Ok(AgentTurnOutcome::ResultRejected {
            invalid_calls: 1,
            ..
        })
    ));
    let candidates = assert_real_session_handoff(&destination, &standalone, "act-one-terminal");
    assert_eq!(
        kinds(&candidates),
        [
            "result.submitted",
            "result.rejected",
            "result.validation_rejections",
            "result.repair_requested",
        ]
    );
    assert!(failure_owner.failures().is_empty());
    assert!(standalone_owner.failures().is_empty());
    harness.shutdown().await;

    let terminal_rejections = usize::from(MAX_REPAIRABLE_INVALID_CALLS) + 1;
    let terminal_values = (0..terminal_rejections)
        .map(|_| submitted_value(RAW_VALIDATION_SENTINEL))
        .collect::<Vec<_>>();
    let (destination, failure_owner, standalone, standalone_owner) = recording_pair();
    let harness = SessionHarness::start(
        &fixtures,
        "n-terminal",
        "act_submit_results",
        Some(&terminal_values),
        HeldGate::Settlement,
        observer(&destination, &failure_owner),
    );
    let (control, task) = harness.spawn_turn(
        "act-n-terminal",
        Arc::clone(&fixtures.native_schema),
        None,
        observer(&standalone, &standalone_owner),
    );
    let outcome = timeout(TEST_TIMEOUT, task)
        .await
        .expect("terminal rejection releases the caller")
        .expect("terminal rejection task does not panic");
    assert!(matches!(
        outcome,
        Ok(AgentTurnOutcome::ResultRejected {
            invalid_calls,
            ..
        }) if usize::from(invalid_calls) == terminal_rejections
    ));
    assert_eq!(control.request_cancel(), AgentTurnCancelDecision::Rejected);
    control.finish_caller();
    await_gate("settlement", "arrived").await;
    release_test_turn_settlement().expect("release terminal result settlement");
    await_gate("settlement", "completed").await;
    let candidates = assert_real_session_handoff(&destination, &standalone, "act-n-terminal");
    let counters = candidates
        .iter()
        .filter_map(AgentResultCandidate::validation_rejections)
        .map(|counter| counter.value())
        .collect::<Vec<_>>();
    assert_eq!(
        counters,
        (1..=u64::try_from(terminal_rejections).expect("bounded rejection count"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        candidates
            .iter()
            .filter_map(AgentResultCandidate::transition)
            .filter(|candidate| candidate.instant_kind() == InstantKind::ResultSubmitted)
            .count(),
        terminal_rejections
    );
    assert_eq!(
        candidates
            .iter()
            .filter_map(AgentResultCandidate::transition)
            .filter(|candidate| candidate.instant_kind() == InstantKind::ResultRejected)
            .count(),
        terminal_rejections
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
                "every rejection is immediately followed by its cumulative counter"
            );
        }
    }
    assert!(failure_owner.failures().is_empty());
    assert!(standalone_owner.failures().is_empty());
    harness.shutdown().await;

    let validation_loop = PythonValidationLoop::start();
    let (destination, failure_owner, standalone, standalone_owner) = recording_pair();
    let harness = SessionHarness::start(
        &fixtures,
        "callback-failed",
        "act_submit_results",
        Some(&[json!({"value": 2})]),
        HeldGate::None,
        observer(&destination, &failure_owner),
    );
    let (control, task) = harness.spawn_turn(
        "act-callback-failed",
        Arc::clone(&fixtures.callback_schema),
        Some(validation_loop.bridge()),
        observer(&standalone, &standalone_owner),
    );
    assert!(matches!(
        await_outcome(&control, task).await,
        Ok(AgentTurnOutcome::SchemaCallbackFailed(_))
    ));
    let candidates = assert_real_session_handoff(&destination, &standalone, "act-callback-failed");
    assert_eq!(kinds(&candidates), ["result.submitted"]);
    assert!(!format!("{candidates:?}").contains("private callback detail"));
    assert!(failure_owner.failures().is_empty());
    assert!(standalone_owner.failures().is_empty());
    harness.shutdown().await;
    validation_loop.shutdown();

    let (destination, failure_owner, standalone, standalone_owner) = recording_pair();
    let harness = SessionHarness::start(
        &fixtures,
        "cancel-zero",
        "act_no_result",
        None,
        HeldGate::Registration,
        observer(&destination, &failure_owner),
    );
    let (control, task) = harness.spawn_turn(
        "act-cancel-zero",
        Arc::clone(&fixtures.native_schema),
        None,
        observer(&standalone, &standalone_owner),
    );
    await_gate("registration", "arrived").await;
    assert_eq!(control.request_cancel(), AgentTurnCancelDecision::Accepted);
    assert_eq!(control.request_cancel(), AgentTurnCancelDecision::Accepted);
    assert!(matches!(
        await_outcome(&control, task).await,
        Err(AgentActError::CallerCancelled)
    ));
    release_test_turn_registration().expect("release cancelled registration");
    assert!(destination.result_candidates().is_empty());
    assert!(standalone.result_candidates().is_empty());
    assert!(failure_owner.failures().is_empty());
    assert!(standalone_owner.failures().is_empty());
    harness.shutdown().await;

    for (label, decision, expected_kinds) in [
        (
            "cancel-one",
            RAW_VALIDATION_SENTINEL,
            vec![
                "result.submitted",
                "result.rejected",
                "result.validation_rejections",
                "result.repair_requested",
            ],
        ),
        (
            "cancel-accepted",
            "approve",
            vec!["result.submitted", "result.accepted"],
        ),
    ] {
        let (destination, failure_owner, standalone, standalone_owner) = recording_pair();
        let harness = SessionHarness::start(
            &fixtures,
            label,
            "act_submit_results",
            Some(&[submitted_value(decision)]),
            HeldGate::Settlement,
            observer(&destination, &failure_owner),
        );
        let act_id = format!("act-{label}");
        let (control, task) = harness.spawn_turn(
            &act_id,
            Arc::clone(&fixtures.native_schema),
            None,
            observer(&standalone, &standalone_owner),
        );
        await_gate("settlement", "arrived").await;
        assert_eq!(control.request_cancel(), AgentTurnCancelDecision::Accepted);
        assert!(matches!(
            await_outcome(&control, task).await,
            Err(AgentActError::CallerCancelled)
        ));
        release_test_turn_settlement().expect("release cancelled settlement");
        await_gate("settlement", "completed").await;
        let candidates = assert_real_session_handoff(&destination, &standalone, &act_id);
        assert_eq!(kinds(&candidates), expected_kinds);
        assert!(failure_owner.failures().is_empty());
        assert!(standalone_owner.failures().is_empty());
        harness.shutdown().await;
    }

    let source = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/diagnostics/result.rs"),
    )
    .expect("read result normalizer source");
    for forbidden in [
        "ResultDiagnosticTestDriver",
        "ResultDiagnosticSettlementForTest",
        "endpoint_for_test",
        "authorization_for_test",
        "arm_for_test",
        "cancel_for_test",
        "settle_for_test",
    ] {
        assert!(
            !source.contains(forbidden),
            "ordinary library surface retained a test-only seam: {forbidden}"
        );
    }
}
