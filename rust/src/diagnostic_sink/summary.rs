use troupe_diagnostics_core::event::DiagnosticEventKind;
use troupe_diagnostics_core::id::{CanonicalUuid, RunLocalId};

use super::callback::CallbackFailure;
use super::dispatcher::DeliveryProgress;
use super::queue::DropDelta;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActOutcome {
    Completed,
    Cancelled,
    Failed,
}

impl ActOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SinkCloseReason {
    ActFinished,
    CallbackFailed,
    DeliveryOverflow,
    RuntimeShutdown,
}

impl SinkCloseReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ActFinished => "act_finished",
            Self::CallbackFailed => "callback_failed",
            Self::DeliveryOverflow => "delivery_overflow",
            Self::RuntimeShutdown => "runtime_shutdown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SinkDropCount {
    event_kind: DiagnosticEventKind,
    events: usize,
    encoded_bytes: usize,
}

impl SinkDropCount {
    const fn from_delta(delta: DropDelta) -> Self {
        Self {
            event_kind: delta.event_kind(),
            events: delta.events(),
            encoded_bytes: delta.encoded_bytes(),
        }
    }

    pub(crate) const fn event_kind(self) -> DiagnosticEventKind {
        self.event_kind
    }

    pub(crate) const fn events(self) -> usize {
        self.events
    }

    pub(crate) const fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SinkDeliverySummary {
    run_id: CanonicalUuid,
    act_id: RunLocalId,
    act_outcome: Option<ActOutcome>,
    close_reason: SinkCloseReason,
    complete: bool,
    delivered_events: usize,
    first_delivered_sequence: Option<u64>,
    last_delivered_sequence: Option<u64>,
    dropped_events: usize,
    dropped_bytes: usize,
    dropped_by_kind: Vec<SinkDropCount>,
    source_gaps: usize,
    truncated_payloads: usize,
    callback_failure: Option<CallbackFailure>,
    callback_abandoned: bool,
}

impl SinkDeliverySummary {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn from_delivery_facts(
        run_id: CanonicalUuid,
        act_id: RunLocalId,
        act_outcome: Option<ActOutcome>,
        nominal_close_reason: SinkCloseReason,
        delivery: DeliveryProgress,
        drops: Vec<DropDelta>,
        source_gaps: usize,
        truncated_payloads: usize,
        callback_failure: Option<CallbackFailure>,
        callback_abandoned: bool,
        unexpected_dispatcher_failure: bool,
    ) -> Self {
        let close_reason = if callback_failure.is_some() {
            SinkCloseReason::CallbackFailed
        } else {
            nominal_close_reason
        };
        let callback_abandoned = callback_abandoned && callback_failure.is_none();
        debug_assert!(
            !callback_abandoned || close_reason == SinkCloseReason::RuntimeShutdown,
            "an abandoned callback must close for Runtime shutdown"
        );
        let dropped_by_kind = drops
            .into_iter()
            .map(SinkDropCount::from_delta)
            .collect::<Vec<_>>();
        let dropped_events = dropped_by_kind
            .iter()
            .fold(0usize, |total, count| total.saturating_add(count.events()));
        let dropped_bytes = dropped_by_kind.iter().fold(0usize, |total, count| {
            total.saturating_add(count.encoded_bytes())
        });
        let complete = close_reason == SinkCloseReason::ActFinished
            && dropped_events == 0
            && source_gaps == 0
            && truncated_payloads == 0
            && callback_failure.is_none()
            && !callback_abandoned
            && !unexpected_dispatcher_failure;

        Self {
            run_id,
            act_id,
            act_outcome,
            close_reason,
            complete,
            delivered_events: delivery.delivered_events(),
            first_delivered_sequence: delivery.first_delivered_sequence(),
            last_delivered_sequence: delivery.last_delivered_sequence(),
            dropped_events,
            dropped_bytes,
            dropped_by_kind,
            source_gaps,
            truncated_payloads,
            callback_failure,
            callback_abandoned,
        }
    }

    pub(crate) const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub(crate) fn act_id(&self) -> &RunLocalId {
        &self.act_id
    }

    pub(crate) const fn act_outcome(&self) -> Option<ActOutcome> {
        self.act_outcome
    }

    pub(crate) const fn close_reason(&self) -> SinkCloseReason {
        self.close_reason
    }

    pub(crate) const fn complete(&self) -> bool {
        self.complete
    }

    pub(crate) const fn delivered_events(&self) -> usize {
        self.delivered_events
    }

    pub(crate) const fn first_delivered_sequence(&self) -> Option<u64> {
        self.first_delivered_sequence
    }

    pub(crate) const fn last_delivered_sequence(&self) -> Option<u64> {
        self.last_delivered_sequence
    }

    pub(crate) const fn dropped_events(&self) -> usize {
        self.dropped_events
    }

    pub(crate) const fn dropped_bytes(&self) -> usize {
        self.dropped_bytes
    }

    pub(crate) fn dropped_by_kind(&self) -> &[SinkDropCount] {
        &self.dropped_by_kind
    }

    pub(crate) const fn source_gaps(&self) -> usize {
        self.source_gaps
    }

    pub(crate) const fn truncated_payloads(&self) -> usize {
        self.truncated_payloads
    }

    pub(crate) fn callback_failure(&self) -> Option<&CallbackFailure> {
        self.callback_failure.as_ref()
    }

    pub(crate) const fn callback_abandoned(&self) -> bool {
        self.callback_abandoned
    }
}
