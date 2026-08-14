use agent_client_protocol::schema::v1::SessionUpdate;

use super::session::AgentDiagnosticUpdateContext;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ToolPayloadCapturePolicy {
    capture_input: bool,
    capture_output: bool,
}

impl ToolPayloadCapturePolicy {
    pub const fn new(capture_input: bool, capture_output: bool) -> Self {
        Self {
            capture_input,
            capture_output,
        }
    }

    pub const fn capture_input(self) -> bool {
        self.capture_input
    }

    pub const fn capture_output(self) -> bool {
        self.capture_output
    }
}

#[inline]
pub(crate) fn observe_update(_context: &AgentDiagnosticUpdateContext<'_>, _update: &SessionUpdate) {
}
