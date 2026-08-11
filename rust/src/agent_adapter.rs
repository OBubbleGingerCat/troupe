use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::{
    ErrorCode, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, SelectedPermissionOutcome,
};
use serde_json::Value;

use crate::agent_launch::{AgentLaunchSpec, launch_spec};
use crate::agent_profile::AgentKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemotePromptErrorSettlement {
    AuthoritativeRequestFailure,
    AuthenticationLost,
    Uncertain,
}

pub(crate) trait AcpAgentAdapter: Sync {
    fn launch_spec(&self) -> &'static AgentLaunchSpec;

    fn resolve_permission(&self, request: &RequestPermissionRequest) -> RequestPermissionOutcome;

    fn classify_remote_prompt_error(&self, error: &Error) -> RemotePromptErrorSettlement;
}

struct CodexAcpAdapter;
struct ClaudeAcpAdapter;
struct KimiAcpAdapter;

static CODEX_ADAPTER: CodexAcpAdapter = CodexAcpAdapter;
static CLAUDE_ADAPTER: ClaudeAcpAdapter = ClaudeAcpAdapter;
static KIMI_ADAPTER: KimiAcpAdapter = KimiAcpAdapter;

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

fn codex_request_meta(
    request: &RequestPermissionRequest,
) -> Option<&serde_json::Map<String, Value>> {
    request_meta_field(request, "codex")?.as_object()
}

fn codex_option_decision(option: &PermissionOption) -> Option<&str> {
    option
        .meta
        .as_ref()?
        .get("codex")?
        .as_object()?
        .get("decision")?
        .as_str()
}

impl CodexAcpAdapter {
    fn resolve_codex_permission(
        &self,
        request: &RequestPermissionRequest,
    ) -> RequestPermissionOutcome {
        let codex_meta = codex_request_meta(request);

        if codex_meta
            .and_then(|meta| meta.get("kind"))
            .and_then(Value::as_str)
            == Some("plan_review")
        {
            return select_unique_id_and_kind(
                request,
                "implement_plan",
                PermissionOptionKind::AllowOnce,
            )
            .unwrap_or(RequestPermissionOutcome::Cancelled);
        }

        if request_meta_field(request, "is_mcp_tool_approval").and_then(Value::as_bool)
            == Some(true)
        {
            return select_unique_id_and_kind(
                request,
                "allow_once",
                PermissionOptionKind::AllowOnce,
            )
            .unwrap_or(RequestPermissionOutcome::Cancelled);
        }

        if codex_meta.is_some_and(|meta| meta.get("params").is_some_and(Value::is_object)) {
            if let Some(outcome) = select_unique(request, |option| {
                option.option_id.0.as_ref() == "allow_permissions_turn"
                    && option.kind == PermissionOptionKind::AllowOnce
                    && codex_option_decision(option) == Some("allowPermissionsForTurn")
            }) {
                return outcome;
            }
            if let Some(outcome) = select_unique(request, |option| {
                option.option_id.0.as_ref() == "allow_once"
                    && option.kind == PermissionOptionKind::AllowOnce
                    && codex_option_decision(option) == Some("accept")
            }) {
                return outcome;
            }
            return reject_unknown(request);
        }

        let provider_question =
            select_unique_id_and_kind(request, "accept", PermissionOptionKind::AllowOnce).is_some()
                && select_unique_id_and_kind(request, "decline", PermissionOptionKind::RejectOnce)
                    .is_some();
        if provider_question {
            return select_unique_id_and_kind(request, "decline", PermissionOptionKind::RejectOnce)
                .expect("the provider question decline option was just verified");
        }

        reject_unknown(request)
    }
}

impl AcpAgentAdapter for CodexAcpAdapter {
    fn launch_spec(&self) -> &'static AgentLaunchSpec {
        launch_spec(AgentKind::Codex)
    }

    fn resolve_permission(&self, request: &RequestPermissionRequest) -> RequestPermissionOutcome {
        self.resolve_codex_permission(request)
    }

    fn classify_remote_prompt_error(&self, error: &Error) -> RemotePromptErrorSettlement {
        if error.code == ErrorCode::AuthRequired || codex_error_is_authentication_loss(error) {
            return RemotePromptErrorSettlement::AuthenticationLost;
        }
        if error.code == ErrorCode::InternalError
            && error
                .data
                .as_ref()
                .and_then(|data| object_field(data, "codexErrorInfo"))
                .and_then(Value::as_str)
                == Some("usageLimitExceeded")
        {
            return RemotePromptErrorSettlement::AuthoritativeRequestFailure;
        }
        RemotePromptErrorSettlement::Uncertain
    }
}

fn codex_error_is_authentication_loss(error: &Error) -> bool {
    if error.code != ErrorCode::InternalError {
        return false;
    }
    let Some(info) = error
        .data
        .as_ref()
        .and_then(|data| object_field(data, "codexErrorInfo"))
    else {
        return false;
    };
    if info.as_str() == Some("unauthorized") {
        return true;
    }
    [
        "httpConnectionFailed",
        "responseStreamConnectionFailed",
        "responseStreamDisconnected",
        "responseTooManyFailedAttempts",
    ]
    .into_iter()
    .any(|key| {
        object_field(info, key)
            .and_then(|failure| object_field(failure, "httpStatusCode"))
            .and_then(Value::as_i64)
            == Some(401)
    })
}

fn classify_unimplemented_provider_error(error: &Error) -> RemotePromptErrorSettlement {
    if error.code == ErrorCode::AuthRequired {
        RemotePromptErrorSettlement::AuthenticationLost
    } else {
        RemotePromptErrorSettlement::Uncertain
    }
}

macro_rules! impl_pending_adapter {
    ($adapter:ty, $agent:expr) => {
        impl AcpAgentAdapter for $adapter {
            fn launch_spec(&self) -> &'static AgentLaunchSpec {
                launch_spec($agent)
            }

            fn resolve_permission(
                &self,
                request: &RequestPermissionRequest,
            ) -> RequestPermissionOutcome {
                reject_unknown(request)
            }

            fn classify_remote_prompt_error(&self, error: &Error) -> RemotePromptErrorSettlement {
                classify_unimplemented_provider_error(error)
            }
        }
    };
}

impl_pending_adapter!(ClaudeAcpAdapter, AgentKind::Claude);
impl_pending_adapter!(KimiAcpAdapter, AgentKind::Kimi);

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
pub(crate) fn permission_for_test(agent: &str, request_json: &str) -> pyo3::PyResult<String> {
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
pub(crate) fn settlement_for_test(
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
}
