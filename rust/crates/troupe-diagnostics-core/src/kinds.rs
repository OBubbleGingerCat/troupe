use serde::{Deserialize, Serialize};

macro_rules! closed_string_enum {
    (
        $name:ident {
            $($variant:ident => $wire:literal),+ $(,)?
        }
    ) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }
    };
}

macro_rules! closed_string_enum_with_all {
    (
        $name:ident {
            $($variant:ident => $wire:literal),+ $(,)?
        }
    ) => {
        closed_string_enum!($name { $($variant => $wire),+ });

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

closed_string_enum_with_all!(CausalRelation {
    Dispatch => "dispatch",
    Return => "return",
    Handoff => "handoff",
    Retry => "retry",
    FollowsFrom => "follows_from",
});

closed_string_enum_with_all!(SpanKind {
    RunLifecycle => "run.lifecycle",
    ProductionPathResolution => "production.path_resolution",
    ProductionLoad => "production.load",
    ProductionConstruct => "production.construct",
    ProductionStart => "production.start",
    ProductionStop => "production.stop",
    ProductionShutdown => "production.shutdown",
    SceneLifecycle => "scene.lifecycle",
    SceneDrain => "scene.drain",
    SceneCleanup => "scene.cleanup",
    ActorHandleLifetime => "actor.handle_lifetime",
    CueMailboxWait => "cue.mailbox_wait",
    CueExecution => "cue.execution",
    EffectLifecycle => "effect.lifecycle",
    AgentSessionOpening => "agent.session.opening",
    AgentSessionLifecycle => "agent.session.lifecycle",
    AgentSessionClosing => "agent.session.closing",
    ActLifecycle => "act.lifecycle",
    ActCaller => "act.caller",
    AgentTurn => "agent.turn",
    AgentThinking => "agent.thinking",
    ToolCall => "tool.call",
});

closed_string_enum_with_all!(InstantKind {
    ActorCast => "actor.cast",
    CueAdmitted => "cue.admitted",
    CueEnqueued => "cue.enqueued",
    CueDispatched => "cue.dispatched",
    CueCancelRequested => "cue.cancel_requested",
    EffectCreated => "effect.created",
    EffectReturned => "effect.returned",
    EffectConsumed => "effect.consumed",
    AgentSessionReady => "agent.session.ready",
    AgentSessionBroken => "agent.session.broken",
    ActAdmitted => "act.admitted",
    ActWaitingReady => "act.waiting_ready",
    ActPromptSubmitted => "act.prompt_submitted",
    ActCancelRequested => "act.cancel_requested",
    ActSupervisorHandoff => "act.supervisor_handoff",
    AgentTurnActivity => "agent.turn.activity",
    AgentTurnTerminal => "agent.turn.terminal",
    AgentTurnSettled => "agent.turn.settled",
    ToolUpdated => "tool.updated",
    ResultSubmitted => "result.submitted",
    ResultRejected => "result.rejected",
    ResultRepairRequested => "result.repair_requested",
    ResultAccepted => "result.accepted",
    ResultMissing => "result.missing",
    DiagnosticComponentFailed => "diagnostic.component_failed",
});

closed_string_enum_with_all!(CounterKind {
    ActorMailboxDepth => "actor.mailbox_depth",
    CueActive => "cue.active",
    AgentTurnActive => "agent.turn.active",
    ResultValidationRejections => "result.validation_rejections",
    DiagnosticDroppedEvents => "diagnostic.dropped_events",
});

closed_string_enum!(SpanOutcome {
    Completed => "completed",
    Cancelled => "cancelled",
    Failed => "failed",
});

closed_string_enum!(PlanEntryPriority {
    High => "high",
    Medium => "medium",
    Low => "low",
});

closed_string_enum!(PlanEntryStatus {
    Pending => "pending",
    InProgress => "in_progress",
    Completed => "completed",
});

closed_string_enum!(ContextSampleOrigin {
    Provider => "provider",
    CarriedForward => "carried_forward",
});

closed_string_enum!(UsageAvailability {
    Available => "available",
    Partial => "partial",
    Unavailable => "unavailable",
});

closed_string_enum!(UsageSource {
    AcpPromptResponseUsage => "acp.prompt_response.usage",
});

closed_string_enum!(UsageUnavailableReason {
    PromptNotSubmitted => "prompt_not_submitted",
    SourceUnsupported => "source_unsupported",
    UsageNotReported => "usage_not_reported",
    TurnSettlementUnknown => "turn_settlement_unknown",
});

closed_string_enum!(CustomSeverity {
    Debug => "debug",
    Info => "info",
    Warning => "warning",
    Error => "error",
});

closed_string_enum!(ToolKind {
    Read => "read",
    Edit => "edit",
    Delete => "delete",
    Move => "move",
    Search => "search",
    Execute => "execute",
    Think => "think",
    Fetch => "fetch",
    SwitchMode => "switch_mode",
    Other => "other",
});

closed_string_enum!(ToolCallStatus {
    Pending => "pending",
    InProgress => "in_progress",
    Completed => "completed",
    Failed => "failed",
});

closed_string_enum!(DiagnosticComponent {
    Sink => "sink",
});

closed_string_enum!(ComponentFailureStage {
    Enqueue => "enqueue",
    Callback => "callback",
});

closed_string_enum!(ComponentFailureErrorCode {
    DeliveryQueueUnavailable => "delivery_queue_unavailable",
    CallbackRaised => "callback_raised",
    CallbackInvalidReturn => "callback_invalid_return",
});
