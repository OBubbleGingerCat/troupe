use agent_client_protocol::schema::v1::SessionUpdate;

use super::session::AgentDiagnosticUpdateContext;

#[inline]
pub(crate) fn observe_update(_context: &AgentDiagnosticUpdateContext<'_>, _update: &SessionUpdate) {
}
