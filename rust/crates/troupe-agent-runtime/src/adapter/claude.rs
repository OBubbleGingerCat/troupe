use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::{
    ErrorCode, PermissionOptionKind, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, StopReason,
};
use serde_json::Value;

use super::{
    AcpAgentAdapter, RemotePromptErrorSettlement, SupervisorResponseSettlement, object_field,
    reject_unknown, select_unique_id_and_kind,
};
use crate::launch::{AgentLaunchSpec, launch_spec};
use crate::profile::AgentKind;

pub(super) struct ClaudeAcpAdapter;

pub(super) static CLAUDE_ADAPTER: ClaudeAcpAdapter = ClaudeAcpAdapter;

impl ClaudeAcpAdapter {
    fn resolve_claude_permission(
        &self,
        request: &RequestPermissionRequest,
    ) -> RequestPermissionOutcome {
        let exit_plan_mode =
            select_unique_id_and_kind(request, "default", PermissionOptionKind::AllowOnce)
                .is_some()
                && select_unique_id_and_kind(request, "plan", PermissionOptionKind::RejectOnce)
                    .is_some();
        if exit_plan_mode {
            return select_unique_id_and_kind(request, "default", PermissionOptionKind::AllowOnce)
                .expect("the Claude ExitPlanMode default option was just verified");
        }

        let ordinary_tool_permission =
            select_unique_id_and_kind(request, "allow", PermissionOptionKind::AllowOnce).is_some()
                && select_unique_id_and_kind(request, "reject", PermissionOptionKind::RejectOnce)
                    .is_some()
                && select_unique_id_and_kind(
                    request,
                    "allow_always",
                    PermissionOptionKind::AllowAlways,
                )
                .is_some();
        if ordinary_tool_permission {
            return select_unique_id_and_kind(request, "allow", PermissionOptionKind::AllowOnce)
                .expect("the Claude allow-once option was just verified");
        }

        reject_unknown(request)
    }

    fn classify_claude_prompt_error(error: &Error) -> RemotePromptErrorSettlement {
        if error.code == ErrorCode::AuthRequired {
            return RemotePromptErrorSettlement::AuthenticationLost;
        }
        if error.code != ErrorCode::InternalError {
            return RemotePromptErrorSettlement::Uncertain;
        }
        let Some(error_kind) = error
            .data
            .as_ref()
            .and_then(|data| object_field(data, "errorKind"))
            .and_then(Value::as_str)
        else {
            return RemotePromptErrorSettlement::Uncertain;
        };
        match error_kind {
            "authentication_failed" | "oauth_org_not_allowed" => {
                RemotePromptErrorSettlement::AuthenticationLost
            }
            "billing_error" | "rate_limit" | "overloaded" | "invalid_request"
            | "model_not_found" | "server_error" | "unknown" | "max_output_tokens"
            | "no_result" => RemotePromptErrorSettlement::AuthoritativeRequestFailure,
            _ => RemotePromptErrorSettlement::Uncertain,
        }
    }
}

impl AcpAgentAdapter for ClaudeAcpAdapter {
    fn launch_spec(&self) -> &'static AgentLaunchSpec {
        launch_spec(AgentKind::Claude)
    }

    fn resolve_permission(&self, request: &RequestPermissionRequest) -> RequestPermissionOutcome {
        self.resolve_claude_permission(request)
    }

    fn accepts_post_ready_mode(
        &self,
        expected: &str,
        observed: &str,
        turn_is_active: bool,
    ) -> bool {
        observed == expected || (turn_is_active && expected == "default" && observed == "plan")
    }

    fn classify_remote_prompt_error(&self, error: &Error) -> RemotePromptErrorSettlement {
        Self::classify_claude_prompt_error(error)
    }

    fn classify_supervisor_response(
        &self,
        response: &PromptResponse,
    ) -> SupervisorResponseSettlement {
        if response.stop_reason == StopReason::Cancelled {
            SupervisorResponseSettlement::Uncertain
        } else {
            SupervisorResponseSettlement::Authoritative
        }
    }
}
