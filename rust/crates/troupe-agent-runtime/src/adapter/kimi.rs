use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::{
    ErrorCode, PermissionOptionKind, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest,
};

use super::{
    AcpAgentAdapter, RemotePromptErrorSettlement, SupervisorResponseSettlement, reject_unknown,
    select_unique_id_and_kind,
};
use crate::launch::{AgentLaunchSpec, launch_spec};
use crate::profile::AgentKind;

pub(super) struct KimiAcpAdapter;

pub(super) static KIMI_ADAPTER: KimiAcpAdapter = KimiAcpAdapter;

fn has_exact_options(
    request: &RequestPermissionRequest,
    expected: &[(&str, PermissionOptionKind)],
) -> bool {
    request.options.len() == expected.len()
        && expected.iter().all(|(option_id, kind)| {
            request
                .options
                .iter()
                .filter(|option| option.option_id.0.as_ref() == *option_id && option.kind == *kind)
                .count()
                == 1
        })
}

fn indexed_options_match(
    request: &RequestPermissionRequest,
    prefix: &str,
    minimum: usize,
    fixed: &[(&str, PermissionOptionKind)],
) -> bool {
    if request.options.len() < minimum + fixed.len()
        || !fixed.iter().all(|(option_id, kind)| {
            request
                .options
                .iter()
                .filter(|option| option.option_id.0.as_ref() == *option_id && option.kind == *kind)
                .count()
                == 1
        })
    {
        return false;
    }

    let mut indexes = Vec::new();
    for option in &request.options {
        let option_id = option.option_id.0.as_ref();
        if fixed
            .iter()
            .any(|(fixed_id, kind)| option_id == *fixed_id && option.kind == *kind)
        {
            continue;
        }
        let Some(index) = option_id
            .strip_prefix(prefix)
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return false;
        };
        if option.kind != PermissionOptionKind::AllowOnce {
            return false;
        }
        indexes.push(index);
    }
    indexes.sort_unstable();
    indexes.len() >= minimum && indexes.iter().copied().eq(0..indexes.len())
}

impl KimiAcpAdapter {
    fn resolve_kimi_permission(
        &self,
        request: &RequestPermissionRequest,
    ) -> RequestPermissionOutcome {
        if has_exact_options(
            request,
            &[
                ("approve_once", PermissionOptionKind::AllowOnce),
                ("approve_always", PermissionOptionKind::AllowAlways),
                ("reject", PermissionOptionKind::RejectOnce),
            ],
        ) {
            return select_unique_id_and_kind(
                request,
                "approve_once",
                PermissionOptionKind::AllowOnce,
            )
            .expect("the Kimi one-shot approval option was just verified");
        }

        if indexed_options_match(
            request,
            "q0_opt_",
            0,
            &[("q0_skip", PermissionOptionKind::RejectOnce)],
        ) {
            return select_unique_id_and_kind(request, "q0_skip", PermissionOptionKind::RejectOnce)
                .expect("the Kimi question skip option was just verified");
        }

        if has_exact_options(
            request,
            &[
                ("plan_approve", PermissionOptionKind::AllowOnce),
                ("plan_revise", PermissionOptionKind::RejectOnce),
                ("plan_reject_and_exit", PermissionOptionKind::RejectOnce),
            ],
        ) {
            return select_unique_id_and_kind(
                request,
                "plan_approve",
                PermissionOptionKind::AllowOnce,
            )
            .expect("the Kimi plan approval option was just verified");
        }

        if indexed_options_match(
            request,
            "plan_opt_",
            2,
            &[
                ("plan_revise", PermissionOptionKind::RejectOnce),
                ("plan_reject_and_exit", PermissionOptionKind::RejectOnce),
            ],
        ) {
            return select_unique_id_and_kind(
                request,
                "plan_opt_0",
                PermissionOptionKind::AllowOnce,
            )
            .expect("the first Kimi plan implementation option was just verified");
        }

        reject_unknown(request)
    }
}

impl AcpAgentAdapter for KimiAcpAdapter {
    fn launch_spec(&self) -> &'static AgentLaunchSpec {
        launch_spec(AgentKind::Kimi)
    }

    fn resolve_permission(&self, request: &RequestPermissionRequest) -> RequestPermissionOutcome {
        self.resolve_kimi_permission(request)
    }

    fn classify_remote_prompt_error(&self, error: &Error) -> RemotePromptErrorSettlement {
        if error.code == ErrorCode::AuthRequired {
            RemotePromptErrorSettlement::AuthenticationLost
        } else {
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
