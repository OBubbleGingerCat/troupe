use uuid::Uuid;

use super::session::TurnDiagnosticContext;
use crate::schema::ValidationIssue;

#[inline]
pub(crate) fn observe_submitted(
    _context: Option<&TurnDiagnosticContext>,
    _session_generation: u64,
    _operation_id: Uuid,
    _turn_index: u64,
) {
}

#[inline]
pub(crate) fn observe_validation_rejected(
    _context: Option<&TurnDiagnosticContext>,
    _session_generation: u64,
    _operation_id: Uuid,
    _turn_index: u64,
    _invalid_calls: u8,
    _issues: &[ValidationIssue],
    _truncated: bool,
) {
}

#[inline]
pub(crate) fn observe_repair_requested(
    _context: Option<&TurnDiagnosticContext>,
    _session_generation: u64,
    _operation_id: Uuid,
    _turn_index: u64,
    _invalid_calls: u8,
) {
}

#[inline]
pub(crate) fn observe_accepted(
    _context: Option<&TurnDiagnosticContext>,
    _session_generation: u64,
    _operation_id: Uuid,
    _turn_index: u64,
) {
}

#[inline]
pub(crate) fn observe_missing(
    _context: Option<&TurnDiagnosticContext>,
    _session_generation: u64,
    _operation_id: Uuid,
    _turn_index: u64,
) {
}
