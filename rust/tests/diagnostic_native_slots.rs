use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pyo3::prelude::*;

mod orchestration {
    pub(crate) mod mailbox {
        pub(crate) struct CueOperation;
    }

    pub(crate) mod scene_context {
        pub(crate) struct CuedScope;
        pub(crate) struct RunBinding;
        pub(crate) struct SceneScope;
    }
}

#[allow(dead_code)]
#[path = "../src/diagnostic_runtime/cue_producer.rs"]
mod cue_producer_contract;
#[allow(dead_code)]
#[path = "../src/diagnostic_runtime/hooks.rs"]
mod diagnostic_hooks;
#[path = "../src/diagnostic_python/fragment_test_support.rs"]
mod fragment_test_support;

use diagnostic_hooks::{
    DiagnosticActBinding, DiagnosticAdmissionCapability, DiagnosticAdmissionProfile,
    DiagnosticAdmissionSlot,
};
use troupe_agent_runtime::AgentTurnControl;

struct FakeCapability(DiagnosticAdmissionProfile);

impl DiagnosticAdmissionCapability for FakeCapability {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn profile(&self) -> DiagnosticAdmissionProfile {
        self.0
    }

    fn admit_act(
        &self,
        _py: Python<'_>,
        _run: &orchestration::scene_context::RunBinding,
        _cued: &Arc<orchestration::scene_context::CuedScope>,
        _control: &Arc<AgentTurnControl>,
        _binding: DiagnosticActBinding,
    ) -> PyResult<()> {
        Ok(())
    }
}

const DIAGNOSTIC_PYTHON_MODULES: &[&str] = &[
    "custom",
    "events",
    "fragment_test_support",
    "install",
    "sink",
];

const DIAGNOSTIC_SINK_MODULES: &[&str] = &[
    "budget",
    "callback",
    "dispatcher",
    "queue",
    "seal",
    "shutdown",
    "summary",
    "thread",
];

const DIAGNOSTIC_RUNTIME_MODULES: &[&str] = &[
    "act_producer",
    "activation",
    "actor_producer",
    "bootstrap",
    "cue_producer",
    "custom_act_binding",
    "custom_binding",
    "effect_producer",
    "hooks",
    "load_producer",
    "observation_bridge",
    "runtime_producer",
    "scene_drain_producer",
    "scene_producer",
    "shutdown",
    "sink_binding",
    "sink_projection",
    "sink_settlement",
    "supervisor",
    "usage_finalization",
];

const DIAGNOSTIC_CLI_MODULES: &[&str] = &[
    "archive_target",
    "args",
    "cleanup_apply",
    "cleanup_policy",
    "dispatch",
    "dump",
    "events_finite",
    "events_follow",
    "http_client",
    "resolver",
    "runs",
    "serve",
    "snapshot",
    "status",
    "target",
    "values",
];

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    fs::read_to_string(crate_root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn assert_ordered(source: &str, context: &str, needles: &[&str]) {
    let mut cursor = 0;
    for needle in needles {
        let offset = source[cursor..]
            .find(needle)
            .unwrap_or_else(|| panic!("{context} is missing ordered marker {needle}"));
        cursor += offset + needle.len();
    }
}

fn module_declarations(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let declaration = line
                .strip_prefix("pub(crate) mod ")
                .or_else(|| line.strip_prefix("mod "))?;
            declaration.strip_suffix(';').map(ToOwned::to_owned)
        })
        .collect()
}

fn rust_source_stems(directory: &Path) -> BTreeSet<String> {
    fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.expect("read source entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .filter_map(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .filter(|stem| stem != "mod")
        .collect()
}

fn assert_module_group(relative: &str, expected: &[&str]) {
    let directory = crate_root().join(relative);
    let expected = expected
        .iter()
        .map(|module| (*module).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(rust_source_stems(&directory), expected);
    assert_eq!(
        module_declarations(
            &fs::read_to_string(directory.join("mod.rs")).expect("read module root")
        ),
        expected
    );
}

#[test]
fn native_slot_inventory_and_parent_declarations_are_exact() {
    assert_module_group("src/diagnostic_python", DIAGNOSTIC_PYTHON_MODULES);
    assert_module_group("src/diagnostic_sink", DIAGNOSTIC_SINK_MODULES);
    assert_module_group("src/diagnostic_runtime", DIAGNOSTIC_RUNTIME_MODULES);
    assert_module_group("src/application/diagnostic_cli", DIAGNOSTIC_CLI_MODULES);

    let lib = read("src/lib.rs");
    for declaration in [
        "mod diagnostic_python;",
        "mod diagnostic_runtime;",
        "mod diagnostic_sink;",
    ] {
        assert!(lib.contains(declaration), "missing {declaration}");
    }
    assert!(
        read("src/application/mod.rs").contains("pub(crate) mod diagnostic_cli;"),
        "diagnostic CLI must remain private at the application root"
    );
}

#[test]
fn native_test_feature_and_private_installer_are_wired_once() {
    let manifest = read("Cargo.toml");
    assert!(manifest.contains("diagnostics-test-support = []"));

    let lib = read("src/lib.rs");
    assert_eq!(
        lib.matches("crate::diagnostic_python::install(module)?;")
            .count(),
        1
    );
    assert!(
        !lib.contains("pymodule_export]\n    use crate::diagnostic_python"),
        "fragment support must not become a Python product symbol"
    );
}

#[test]
fn fresh_fragment_installer_never_reuses_or_publishes_a_module() {
    pyo3::Python::initialize();
    Python::attach(|py| {
        let source = CString::new("VALUE = 1").unwrap();
        let first = fragment_test_support::install_fresh_fragment(
            py,
            source.as_c_str(),
            c"diagnostic-events.py",
            c"_troupe_diagnostic_events_test",
        )
        .unwrap();
        let second = fragment_test_support::install_fresh_fragment(
            py,
            source.as_c_str(),
            c"diagnostic-events.py",
            c"_troupe_diagnostic_events_test",
        )
        .unwrap();

        assert!(!first.is(&second));
        first.setattr("VALUE", 2).unwrap();
        assert_eq!(first.getattr("VALUE").unwrap().extract::<i32>().unwrap(), 2);
        assert_eq!(
            second.getattr("VALUE").unwrap().extract::<i32>().unwrap(),
            1
        );

        let modules = py.import("sys").unwrap().getattr("modules").unwrap();
        assert!(!modules.contains("_troupe_diagnostic_events_test").unwrap());
    });
}

#[test]
fn shared_roots_are_connected_to_typed_noop_hooks() {
    for (relative, references) in [
        (
            "src/act_call.rs",
            &[
                "diagnostic_runtime::act_producer",
                "diagnostic_runtime::sink_binding",
            ][..],
        ),
        (
            "src/orchestration/mod.rs",
            &["diagnostic_runtime::hooks"][..],
        ),
        (
            "src/orchestration/actor.rs",
            &[
                "diagnostic_runtime::actor_producer",
                "preflight_diagnostic_sink",
            ][..],
        ),
        (
            "src/orchestration/actor_handle.rs",
            &["diagnostic_runtime::actor_producer"][..],
        ),
        (
            "src/orchestration/actor_registry.rs",
            &["diagnostic_runtime::actor_producer"][..],
        ),
        (
            "src/orchestration/cue.rs",
            &["diagnostic_runtime::cue_producer"][..],
        ),
        (
            "src/orchestration/cue_future.rs",
            &["diagnostic_runtime::cue_producer"][..],
        ),
        (
            "src/orchestration/effect.rs",
            &["diagnostic_runtime::effect_producer"][..],
        ),
        (
            "src/orchestration/mailbox.rs",
            &["diagnostic_runtime::cue_producer"][..],
        ),
        (
            "src/orchestration/production.rs",
            &["diagnostic_runtime::runtime_producer"][..],
        ),
        (
            "src/orchestration/python_task.rs",
            &["diagnostic_runtime::scene_producer"][..],
        ),
        (
            "src/orchestration/runtime.rs",
            &["diagnostic_runtime::runtime_producer"][..],
        ),
        (
            "src/orchestration/scene_context.rs",
            &[
                "diagnostic_runtime::scene_drain_producer",
                "diagnostic_runtime::scene_producer",
            ][..],
        ),
    ] {
        let source = read(relative);
        for reference in references {
            assert!(
                source.contains(reference),
                "{relative} is missing {reference}"
            );
        }
    }
}

#[test]
fn hook_slots_freeze_typed_identity_scope_outcome_and_error_inputs() {
    let activation = read("src/diagnostic_runtime/activation.rs");
    for input in [
        "&RuntimeCore",
        "&ProductionState",
        "&Arc<RunBinding>",
        "PyResult<()>",
    ] {
        assert!(activation.contains(input), "activation is missing {input}");
    }

    let act = read("src/diagnostic_runtime/act_producer.rs");
    for input in [
        "&RunBinding",
        "&Arc<CuedScope>",
        "&Arc<AgentTurnControl>",
        "ActCallerExit",
        "Option<&PyErr>",
    ] {
        assert!(act.contains(input), "Act producer is missing {input}");
    }

    let actor = read("src/diagnostic_runtime/actor_producer.rs");
    for input in [
        "&ProductionState",
        "&Arc<ActorIdentity>",
        "&Bound<'_, PyType>",
        "Result<&Py<ActorHandle>, &PyErr>",
        "ActorHandleIdentity",
        "fn cleared(_actor: &Actor, _production: Option<&Py<PyAny>>)",
    ] {
        assert!(actor.contains(input), "Actor producer is missing {input}");
    }

    let cue = read("src/diagnostic_runtime/cue_producer.rs");
    for input in [
        "&CueOperation",
        "&RunBinding",
        "&SceneScope",
        "CueMailboxHook",
        "CueCallerOutcome",
        "CueTerminalOutcome",
        "CueLineageSnapshot",
        "fn lineage_snapshot(cued: &Arc<CuedScope>)",
        "effect_producer::cue_terminal",
        "effect_producer::caller_finished",
        "impl FnOnce() -> Option<PyErr>",
    ] {
        assert!(cue.contains(input), "Cue producer is missing {input}");
    }

    let effect = read("src/diagnostic_runtime/effect_producer.rs");
    for input in [
        "&EffectConstruction",
        "&Effect",
        "Result<&Bound<'_, Effect>, &PyErr>",
        "Option<&Py<PyString>>",
        "&Arc<CuedScope>",
        "CueTerminalOutcome",
        "CueCallerOutcome",
        "SchemaU64",
        "Returned",
    ] {
        assert!(effect.contains(input), "Effect producer is missing {input}");
    }

    let scene = read("src/diagnostic_runtime/scene_producer.rs");
    for input in [
        "&RunBinding",
        "&SceneScope",
        "&TaskLineage",
        "Option<&PyErr>",
    ] {
        assert!(scene.contains(input), "Scene producer is missing {input}");
    }

    let drain = read("src/diagnostic_runtime/scene_drain_producer.rs");
    for input in ["&SceneScope", "SceneDriverExit", "Option<&PyErr>"] {
        assert!(
            drain.contains(input),
            "Scene drain producer is missing {input}"
        );
    }

    let runtime = read("src/diagnostic_runtime/runtime_producer.rs");
    for input in [
        "&RuntimeCore",
        "&ProductionState",
        "&RunBinding",
        "Option<&PyErr>",
    ] {
        assert!(
            runtime.contains(input),
            "Runtime producer is missing {input}"
        );
    }
}

#[test]
fn noop_cue_terminal_does_not_evaluate_error_supplier() {
    use std::cell::Cell;

    let operation = orchestration::mailbox::CueOperation;
    let evaluated = Cell::new(false);
    cue_producer_contract::terminal(
        &operation,
        cue_producer_contract::CueTerminalOutcome::Failed,
        || {
            evaluated.set(true);
            None
        },
    );

    assert!(!evaluated.get());
}

#[test]
fn hooks_bracket_real_transitions_and_overridden_lifecycle_calls() {
    let runtime = read("src/orchestration/runtime.rs");
    assert_ordered(
        &runtime,
        "Run activation",
        &[
            "production_state.bind(&binding)?;",
            "diagnostic_runtime::activation::bind_run(",
            "runtime_producer::run_started(&self.core, &binding);",
            "tokio::get_runtime().spawn",
        ],
    );
    assert_ordered(
        &runtime,
        "Production start",
        &[
            "RuntimeHook::ProductionStartEntered",
            "await_hook(",
            "RuntimeTaskPhase::Start",
            "RuntimeHook::ProductionStartReturned",
        ],
    );
    assert_ordered(
        &runtime,
        "Scene execution",
        &[
            "RuntimeHook::SceneEntered",
            "create_scene_task(&locals, &production, Arc::clone(&binding))",
            "RuntimeHook::SceneReturned",
        ],
    );
    assert_ordered(
        &runtime,
        "Production stop",
        &[
            "RuntimeHook::ProductionStopEntered",
            "await_hook(",
            "RuntimeTaskPhase::Stop",
            "RuntimeHook::ProductionStopReturned",
        ],
    );
    assert_ordered(
        &runtime,
        "Shutdown transition",
        &[
            "self.shutdown.cancel();",
            "self.shutdown_observed.swap(true, Ordering::AcqRel)",
            "RuntimeHook::ShutdownRequested",
        ],
    );

    let production = read("src/orchestration/production.rs");
    assert_ordered(
        &production,
        "Production activation",
        &[
            "let state = Arc::new(ProductionState::new());",
            "activation::production_created(args.py(), &state)?;",
            "RuntimeHook::ProductionCreated",
        ],
    );
    assert_ordered(
        &production,
        "Actor cast",
        &[
            "actor_producer::cast_started(",
            "let result = (||",
            "reservation.commit(&capability);",
            "actor_producer::cast_finished(",
        ],
    );

    let cue_call = read("src/orchestration/cue_future.rs");
    assert_ordered(
        &cue_call,
        "Cue admission preparation",
        &[
            "validate_lineage_for_scene(py, &scene)?",
            ".begin_admission()",
            "cue_producer::admission_started(&binding, &scene);",
            "prepared.commit(operation.clone())?;",
        ],
    );

    let scene_context = read("src/orchestration/scene_context.rs");
    assert_ordered(
        &scene_context,
        "Cue committed admission",
        &[
            "state.operations.push(operation.clone());",
            "cue_producer::observe(&operation, CueHook::Admitted);",
            "self.scope.cancel_operations(operations);",
            "operation.enqueue()?;",
        ],
    );

    let mailbox = read("src/orchestration/mailbox.rs");
    let terminal = &mailbox[mailbox
        .find("fn transition_from_result(")
        .expect("Cue terminal transition")..];
    assert_ordered(
        terminal,
        "Cue terminal outcome",
        &[
            "let diagnostic_outcome = match &outcome",
            "state.phase = OperationPhase::Terminal(outcome);",
            "cue_producer::terminal(self, action.diagnostic_outcome, || self.terminal_error());",
        ],
    );

    let effect = read("src/orchestration/effect.rs");
    assert_ordered(
        &effect,
        "Effect construction",
        &[
            "effect_producer::construction_started(&construction);",
            "let result = (||",
            "effect_producer::construction_finished(&construction, Ok(effect));",
            "effect_producer::construction_finished(&construction, Err(error));",
        ],
    );

    let task = read("src/orchestration/python_task.rs");
    assert_ordered(
        &task,
        "Scene task outcome",
        &[
            "let result = future.await;",
            "scene_producer::task_finished(&self.scene, result.as_ref().err());",
            "result.map(|_| ())",
        ],
    );

    let scene = read("src/orchestration/scene_context.rs");
    assert_ordered(
        &scene,
        "Scene close transition",
        &[
            "state.phase = ScenePhase::Closed;",
            "CloseAction::Closed",
            "self.observe_closed();",
        ],
    );
    assert_ordered(
        &scene,
        "Scope driver exact-once exit",
        &[
            "self.owner_closed.swap(true, Ordering::AcqRel)",
            "scene_drain_producer::driver_exited(scene, exit, error);",
            "scene.close();",
        ],
    );
}

#[test]
fn act_admission_handoff_is_ordered_and_run_binding_owned() {
    let hooks = read("src/diagnostic_runtime/hooks.rs");
    for contract in [
        "DiagnosticAdmissionCapability",
        "DiagnosticAdmissionProfile",
        "DiagnosticAdmissionSlot",
        "DiagnosticActBinding",
        "DiagnosticCaptureConfig",
    ] {
        assert!(hooks.contains(contract), "missing {contract}");
    }

    let binding = read("src/orchestration/scene_context.rs");
    assert!(binding.contains("diagnostic_admission: DiagnosticAdmissionSlot"));
    assert!(binding.contains("fn diagnostic_admission("));

    let act_call = read("src/act_call.rs");
    assert!(act_call.contains("diagnostics: DiagnosticActBinding"));
    let admission = act_call
        .find("control.install_admission")
        .expect("agent admission install");
    let diagnostics = act_call[admission..]
        .find("diagnostic_runtime::sink_binding")
        .map(|offset| admission + offset)
        .expect("diagnostic admission handoff");
    let prompt = act_call[diagnostics..]
        .find(".run_turn(")
        .map(|offset| diagnostics + offset)
        .expect("prompt submission");
    assert!(admission < diagnostics && diagnostics < prompt);

    let sink_binding = read("src/diagnostic_runtime/sink_binding.rs");
    assert_ordered(
        &sink_binding,
        "Production capability handoff",
        &[
            "run.diagnostic_admission().capability()",
            "capability.admit_act(py, run, cued, control, binding)",
            "act_producer::admitted(run, cued, control)",
        ],
    );
}

#[test]
fn admission_slot_is_empty_by_default_and_profile_immutable() {
    let slot = DiagnosticAdmissionSlot::new();
    assert!(slot.capability().is_none());
    assert_eq!(slot.profile(), None);

    slot.install(Arc::new(FakeCapability(
        DiagnosticAdmissionProfile::ProductionDurable,
    )))
    .unwrap();
    assert_eq!(
        slot.profile(),
        Some(DiagnosticAdmissionProfile::ProductionDurable)
    );
    assert!(
        slot.capability()
            .expect("installed capability")
            .as_any()
            .is::<FakeCapability>()
    );

    let error = slot
        .install(Arc::new(FakeCapability(
            DiagnosticAdmissionProfile::SinkOnlyVolatile,
        )))
        .unwrap_err();
    assert_eq!(
        error.installed,
        DiagnosticAdmissionProfile::ProductionDurable
    );
    assert_eq!(
        error.requested,
        DiagnosticAdmissionProfile::SinkOnlyVolatile
    );
}
