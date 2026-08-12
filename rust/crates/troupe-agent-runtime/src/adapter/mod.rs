mod claude;
mod codex;
mod kimi;

use agent_client_protocol::Error;
#[cfg(feature = "agent-test-support")]
use agent_client_protocol::schema::v1::StopReason;
use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SelectedPermissionOutcome,
};
use serde_json::Value;

use self::claude::CLAUDE_ADAPTER;
use self::codex::CODEX_ADAPTER;
use self::kimi::KIMI_ADAPTER;
use crate::launch::AgentLaunchSpec;
use crate::profile::AgentKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemotePromptErrorSettlement {
    AuthoritativeRequestFailure,
    AuthenticationLost,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SupervisorResponseSettlement {
    Authoritative,
    Uncertain,
}

pub(crate) trait AcpAgentAdapter: Sync {
    fn launch_spec(&self) -> &'static AgentLaunchSpec;

    fn accepts_post_ready_mode(
        &self,
        expected: &str,
        observed: &str,
        turn_is_active: bool,
    ) -> bool {
        let _ = turn_is_active;
        observed == expected
    }

    fn resolve_permission(&self, request: &RequestPermissionRequest) -> RequestPermissionOutcome;

    fn classify_remote_prompt_error(&self, error: &Error) -> RemotePromptErrorSettlement;

    fn classify_supervisor_response(
        &self,
        response: &PromptResponse,
    ) -> SupervisorResponseSettlement;
}

pub(crate) fn agent_adapter(agent: AgentKind) -> &'static dyn AcpAgentAdapter {
    match agent {
        AgentKind::Codex => &CODEX_ADAPTER,
        AgentKind::Claude => &CLAUDE_ADAPTER,
        AgentKind::Kimi => &KIMI_ADAPTER,
    }
}

fn selected(option: &PermissionOption) -> RequestPermissionOutcome {
    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
}

fn select_unique(
    request: &RequestPermissionRequest,
    mut matches: impl FnMut(&PermissionOption) -> bool,
) -> Option<RequestPermissionOutcome> {
    let mut matching = request.options.iter().filter(|option| matches(option));
    let option = matching.next()?;
    matching.next().is_none().then(|| selected(option))
}

fn select_unique_id_and_kind(
    request: &RequestPermissionRequest,
    option_id: &str,
    kind: PermissionOptionKind,
) -> Option<RequestPermissionOutcome> {
    select_unique(request, |option| {
        option.option_id.0.as_ref() == option_id && option.kind == kind
    })
}

fn reject_unknown(request: &RequestPermissionRequest) -> RequestPermissionOutcome {
    select_unique(request, |option| {
        option.kind == PermissionOptionKind::RejectOnce
    })
    .unwrap_or(RequestPermissionOutcome::Cancelled)
}

fn object_field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object()?.get(key)
}

fn request_meta_field<'a>(request: &'a RequestPermissionRequest, key: &str) -> Option<&'a Value> {
    request.meta.as_ref()?.get(key)
}

#[cfg(feature = "agent-test-support")]
fn parse_agent_for_test(agent: &str) -> pyo3::PyResult<AgentKind> {
    match agent {
        "codex" => Ok(AgentKind::Codex),
        "claude" => Ok(AgentKind::Claude),
        "kimi" => Ok(AgentKind::Kimi),
        _ => Err(pyo3::exceptions::PyValueError::new_err(
            "unknown agent adapter",
        )),
    }
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_adapter_permission_for_test")]
pub fn permission_for_test(agent: &str, request_json: &str) -> pyo3::PyResult<String> {
    let request: RequestPermissionRequest =
        serde_json::from_str(request_json).map_err(|error| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid permission request: {error}"))
        })?;
    Ok(
        match agent_adapter(parse_agent_for_test(agent)?).resolve_permission(&request) {
            RequestPermissionOutcome::Selected(selected) => {
                format!("selected:{}", selected.option_id.0)
            }
            RequestPermissionOutcome::Cancelled => "cancelled".to_owned(),
            _ => "cancelled".to_owned(),
        },
    )
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_adapter_settlement_for_test")]
pub fn settlement_for_test(
    agent: &str,
    code: i32,
    data_json: &str,
) -> pyo3::PyResult<&'static str> {
    let data: Value = serde_json::from_str(data_json).map_err(|error| {
        pyo3::exceptions::PyValueError::new_err(format!("invalid error data: {error}"))
    })?;
    let error = Error::new(code, "test prompt error").data((data != Value::Null).then_some(data));
    Ok(
        match agent_adapter(parse_agent_for_test(agent)?).classify_remote_prompt_error(&error) {
            RemotePromptErrorSettlement::AuthoritativeRequestFailure => {
                "authoritative_request_failure"
            }
            RemotePromptErrorSettlement::AuthenticationLost => "authentication_lost",
            RemotePromptErrorSettlement::Uncertain => "uncertain",
        },
    )
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_adapter_supervisor_response_for_test")]
pub fn supervisor_response_for_test(
    agent: &str,
    stop_reason: &str,
) -> pyo3::PyResult<&'static str> {
    let stop_reason: StopReason = serde_json::from_value(Value::String(stop_reason.to_owned()))
        .map_err(|error| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid stop reason: {error}"))
        })?;
    let response = PromptResponse::new(stop_reason);
    Ok(
        match agent_adapter(parse_agent_for_test(agent)?).classify_supervisor_response(&response) {
            SupervisorResponseSettlement::Authoritative => "authoritative",
            SupervisorResponseSettlement::Uncertain => "uncertain",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_plan_review_uses_the_canonical_implement_option() {
        let request: RequestPermissionRequest = serde_json::from_value(json!({
            "sessionId": "session",
            "toolCall": {
                "toolCallId": "plan-review:item",
                "kind": "switch_mode",
                "status": "pending"
            },
            "options": [
                {"optionId": "revise_plan", "name": "revise", "kind": "reject_once"},
                {"optionId": "implement_plan", "name": "implement", "kind": "allow_once"}
            ],
            "_meta": {"codex": {"kind": "plan_review", "planItemId": "item"}}
        }))
        .expect("the pinned Codex permission shape must deserialize");

        assert!(matches!(
            CODEX_ADAPTER.resolve_permission(&request),
            RequestPermissionOutcome::Selected(selected)
                if selected.option_id.0.as_ref() == "implement_plan"
        ));
    }

    #[test]
    fn claude_accepts_plan_mode_only_during_an_active_turn() {
        assert!(CLAUDE_ADAPTER.accepts_post_ready_mode("default", "default", false));
        assert!(CLAUDE_ADAPTER.accepts_post_ready_mode("default", "plan", true));
        assert!(!CLAUDE_ADAPTER.accepts_post_ready_mode("default", "plan", false));
        assert!(!CLAUDE_ADAPTER.accepts_post_ready_mode("default", "acceptEdits", true));
        assert!(!CODEX_ADAPTER.accepts_post_ready_mode("agent", "plan", true));
    }
}
