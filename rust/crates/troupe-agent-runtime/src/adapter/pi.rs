use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::{
    ErrorCode, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
};
use serde_json::Value;

use super::{
    AcpAgentAdapter, RemotePromptErrorSettlement, SupervisorResponseSettlement, reject_unknown,
};
use crate::launch::{AgentLaunchSpec, launch_spec};
use crate::profile::AgentKind;

pub(super) struct PiAcpAdapter;

pub(super) static PI_ADAPTER: PiAcpAdapter = PiAcpAdapter;

impl AcpAgentAdapter for PiAcpAdapter {
    fn launch_spec(&self) -> &'static AgentLaunchSpec {
        launch_spec(AgentKind::Pi)
    }

    fn resolve_permission(&self, request: &RequestPermissionRequest) -> RequestPermissionOutcome {
        // Pi runs with the Troupe-owned result extension and has no interactive
        // permission surface in this integration.  If a future Pi release does
        // ask, fail closed instead of approving an unknown operation.
        reject_unknown(request)
    }

    fn classify_remote_prompt_error(&self, error: &Error) -> RemotePromptErrorSettlement {
        if error.code == ErrorCode::AuthRequired {
            RemotePromptErrorSettlement::AuthenticationLost
        } else if error.code == ErrorCode::InternalError
            && error
                .data
                .as_ref()
                .and_then(|data| data.as_object())
                .and_then(|data| data.get("piErrorKind"))
                .and_then(Value::as_str)
                == Some("provider")
        {
            // The Troupe shim adds this marker only after Pi has accepted the
            // prompt and reported a provider-level failure (capacity, 5xx,
            // retry exhaustion, etc.).  It is a turn-level provider failure,
            // not a lost result channel.
            RemotePromptErrorSettlement::ProviderFailure
        } else if error
            .data
            .as_ref()
            .and_then(|data| data.as_object())
            .and_then(|data| data.get("piErrorKind"))
            .and_then(Value::as_str)
            == Some("auth")
        {
            RemotePromptErrorSettlement::AuthenticationLost
        } else {
            // Pi's RPC stream can report provider failures after the prompt was
            // accepted; without a stable ACP error taxonomy we must not claim
            // that those failures are authoritative turn settlement.
            RemotePromptErrorSettlement::Uncertain
        }
    }

    fn classify_supervisor_response(
        &self,
        _response: &PromptResponse,
    ) -> SupervisorResponseSettlement {
        SupervisorResponseSettlement::Authoritative
    }
}
