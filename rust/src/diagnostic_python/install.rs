use std::ffi::CString;

use pyo3::prelude::*;
use pyo3::types::{PyList, PyModule};

const PUBLIC_NAMES: &[&str] = &[
    "ActTokenMetric",
    "ActTokenUsageFinalized",
    "ActTokenUsageRows",
    "ActorDetail",
    "AffectedElapsedInterval",
    "AgentMessageCompleted",
    "AgentMessageDelta",
    "AgentPlanSnapshot",
    "AgentSessionBrokenDetail",
    "AgentSessionDetail",
    "AgentTurnTerminalDetail",
    "AttributeEqualsFilter",
    "AttributeExistsFilter",
    "CausalLink",
    "CompletedSpanDuration",
    "ContextUsageSampled",
    "CounterRows",
    "CounterSampled",
    "CounterSource",
    "CounterValue",
    "CustomCounterSampled",
    "CustomInstantOccurred",
    "CustomSpanFinished",
    "CustomSpanStarted",
    "DiagnosticAttributeValue",
    "DiagnosticAttributes",
    "DiagnosticCallbackFailure",
    "DiagnosticCapture",
    "DiagnosticComponentFailedDetail",
    "DiagnosticContextError",
    "DiagnosticDimension",
    "DiagnosticDimensions",
    "DiagnosticDropCount",
    "DiagnosticEvent",
    "DiagnosticScalar",
    "DiagnosticScope",
    "DiagnosticSink",
    "DiagnosticSinkStateError",
    "DiagnosticSinkSummary",
    "DiagnosticToolInput",
    "DiagnosticToolLocation",
    "DiagnosticToolOutput",
    "EffectDetail",
    "EmptyDetail",
    "EventRows",
    "FrozenJsonArray",
    "FrozenJsonObject",
    "FrozenJsonValue",
    "GroupBy",
    "InstantCount",
    "InstantDetail",
    "InstantOccurred",
    "InstantRows",
    "InstantSource",
    "MetricQuery",
    "MetricSource",
    "MetricView",
    "ObservationGap",
    "OutcomeFilter",
    "PlanEntry",
    "ProductionConstructDetail",
    "ProductionLoadDetail",
    "ProductionPathResolutionDetail",
    "ResultIssue",
    "ResultTransitionDetail",
    "SeverityFilter",
    "SpanFinished",
    "SpanRows",
    "SpanSource",
    "SpanStartDetail",
    "SpanStarted",
    "TableColumn",
    "TableQuery",
    "TableSource",
    "TableView",
    "TimeSeriesQuery",
    "TimeSeriesView",
    "TimelineQuery",
    "TimelineSource",
    "TimelineView",
    "ToolCallDetail",
    "ViewFilter",
    "ViewScalar",
    "ViewSpec",
    "counter",
    "event",
    "span",
];

fn combined_source() -> CString {
    let fragments = [
        super::events::source(),
        super::sink::source(),
        super::custom::source(),
        super::views::source(),
    ];
    let mut source = Vec::with_capacity(
        fragments
            .iter()
            .map(|fragment| fragment.to_bytes().len() + 1)
            .sum(),
    );
    for fragment in fragments {
        source.extend_from_slice(fragment.to_bytes());
        source.push(b'\n');
    }
    CString::new(source).expect("embedded diagnostic fragments contain no NUL bytes")
}

pub(crate) fn install(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    let source = combined_source();
    let diagnostics = PyModule::from_code(
        py,
        source.as_c_str(),
        c"<troupe.diagnostics>",
        c"troupe.diagnostics",
    )?;
    diagnostics.add("__all__", PyList::new(py, PUBLIC_NAMES)?)?;
    module.add_submodule(&diagnostics)
}

#[cfg(test)]
mod tests {
    use pyo3::prelude::*;
    use pyo3::types::{PyDict, PyModule};

    use super::install;

    #[test]
    fn installs_one_closed_native_diagnostics_module() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let runtime = PyModule::new(py, "troupe._runtime").expect("create test runtime module");
            install(&runtime).expect("install diagnostics module");
            let diagnostics = runtime
                .getattr("diagnostics")
                .expect("runtime must expose diagnostics");
            let modules = py
                .import("sys")
                .and_then(|sys| sys.getattr("modules"))
                .expect("read sys.modules");
            let modules = modules.cast_into::<PyDict>().expect("cast sys.modules");
            let registered = modules
                .get_item("troupe.diagnostics")
                .expect("read registered diagnostics module")
                .expect("diagnostics module must be registered");

            assert!(diagnostics.is(&registered));
            assert_eq!(
                diagnostics
                    .getattr("__name__")
                    .and_then(|name| name.extract::<String>())
                    .unwrap(),
                "troupe.diagnostics"
            );
            py.run(
                cr#"
import inspect as _inspect

assert __all__ == sorted(__all__)
assert set(__all__) == {name for name in globals() if not name.startswith("_")}
assert len(__all__) == 87
for name in __all__:
    value = globals()[name]
    if type(value) is type or type(value).__name__ == "function":
        assert value.__module__ == "troupe.diagnostics", (name, value.__module__)

assert tuple(_inspect.signature(DiagnosticCapture).parameters) == (
    "agent_messages", "plans", "tool_calls", "result_validation",
    "usage", "custom_events", "tool_inputs", "tool_outputs",
)
assert tuple(_inspect.signature(ToolCallDetail).parameters)[-2:] == (
    "captured_input", "captured_output",
)
"#,
                Some(&diagnostics.cast_into::<PyModule>().unwrap().dict()),
                None,
            )
            .expect("validate installed diagnostics surface");
            modules
                .del_item("troupe.diagnostics")
                .expect("remove test diagnostics module");
        });
    }
}
