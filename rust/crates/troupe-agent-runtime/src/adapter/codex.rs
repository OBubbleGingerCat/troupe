use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::{
    ErrorCode, PermissionOption, PermissionOptionKind, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest,
};
use serde_json::Value;

use super::{
    AcpAgentAdapter, RemotePromptErrorSettlement, SupervisorResponseSettlement, object_field,
    reject_unknown, request_meta_field, select_unique, select_unique_id_and_kind,
};
use crate::launch::{AgentLaunchSpec, launch_spec};
use crate::profile::AgentKind;

pub(super) struct CodexAcpAdapter;

pub(super) static CODEX_ADAPTER: CodexAcpAdapter = CodexAcpAdapter;

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

    fn classify_supervisor_response(
        &self,
        _response: &PromptResponse,
    ) -> SupervisorResponseSettlement {
        SupervisorResponseSettlement::Authoritative
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
