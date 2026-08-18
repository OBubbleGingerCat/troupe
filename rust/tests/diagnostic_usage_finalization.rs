use std::{fs, path::PathBuf};

use troupe_agent_runtime::{AgentDiagnosticErrorCode, diagnostics::usage::AgentTurnUsage};
use troupe_diagnostics_core::{
    kinds::{UsageAvailability, UsageSource, UsageUnavailableReason},
    scalar::{SchemaU64, TokenCount},
};

#[allow(dead_code)]
#[path = "../src/diagnostic_runtime/usage_finalization.rs"]
mod usage_finalization_source;

use usage_finalization_source::machine::{
    FinalUsage, FinalizationEffects, FinalizationMachine, MachineDrive, SettlementBoundary,
    SlotSnapshot,
};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn token(value: &str) -> TokenCount {
    TokenCount::parse(value).expect("canonical arbitrary-precision token count")
}

fn reported(values: [&str; 6]) -> FinalUsage {
    let [total, input, output, thought, cached_read, cached_write] = values;
    FinalUsage::from_candidate(
        &AgentTurnUsage::new(
            UsageAvailability::Available,
            Some(UsageSource::AcpPromptResponseUsage),
            None,
            Some(token(total)),
            Some(token(input)),
            Some(token(output)),
            Some(token(thought)),
            Some(token(cached_read)),
            Some(token(cached_write)),
        )
        .expect("valid authoritative usage"),
    )
}

fn unavailable(reason: UsageUnavailableReason) -> FinalUsage {
    FinalUsage::from_candidate(&AgentTurnUsage::unavailable(reason))
}

fn snapshot(prompt_submitted: bool, settlement: Option<SettlementBoundary>) -> SlotSnapshot {
    SlotSnapshot::new(prompt_submitted, settlement)
}

#[derive(Default)]
struct RecordingEffects {
    admission_attempts: usize,
    admissions: Vec<FinalUsage>,
    acknowledgment_attempts: usize,
    acknowledgments: Vec<SchemaU64>,
    admission_error: Option<AgentDiagnosticErrorCode>,
    acknowledgment_error: Option<AgentDiagnosticErrorCode>,
}

impl RecordingEffects {
    fn failing_admission(error_code: AgentDiagnosticErrorCode) -> Self {
        Self {
            admission_error: Some(error_code),
            ..Self::default()
        }
    }
}

impl FinalizationEffects for RecordingEffects {
    type Ack = SchemaU64;

    fn admit(&mut self, usage: FinalUsage) -> Result<SchemaU64, AgentDiagnosticErrorCode> {
        self.admission_attempts += 1;
        if let Some(error_code) = self.admission_error {
            return Err(error_code);
        }
        let sequence = SchemaU64::new(
            u64::try_from(self.admission_attempts).expect("bounded test admission count") + 40,
        );
        self.admissions.push(usage);
        Ok(sequence)
    }

    fn acknowledge(
        &mut self,
        sequence: SchemaU64,
    ) -> Result<Option<Self::Ack>, AgentDiagnosticErrorCode> {
        self.acknowledgment_attempts += 1;
        if let Some(error_code) = self.acknowledgment_error {
            return Err(error_code);
        }
        self.acknowledgments.push(sequence);
        Ok(Some(sequence))
    }
}

fn assert_finalized(outcome: MachineDrive<SchemaU64>, sequence: u64) {
    assert_eq!(outcome, MachineDrive::Finalized(SchemaU64::new(sequence)));
}

fn assert_reason(usage: &FinalUsage, reason: UsageUnavailableReason) {
    assert_eq!(usage.availability(), UsageAvailability::Unavailable);
    assert_eq!(usage.source(), None);
    assert_eq!(usage.unavailable_reason(), Some(reason));
    assert_eq!(usage.provider_total_tokens(), None);
    assert_eq!(usage.input_tokens(), None);
    assert_eq!(usage.output_tokens(), None);
    assert_eq!(usage.thought_tokens(), None);
    assert_eq!(usage.cached_read_tokens(), None);
    assert_eq!(usage.cached_write_tokens(), None);
}

mod diagnostic_runtime {
    pub mod usage_finalization {
        use super::super::*;

        #[test]
        fn pre_submission_terminal_admits_and_acknowledges_exactly_once() {
            let mut machine = FinalizationMachine::default();
            let mut effects = RecordingEffects::default();
            let not_submitted = snapshot(false, Some(SettlementBoundary::NotSubmitted));

            assert_finalized(machine.drive(not_submitted, &mut effects), 41);
            assert_eq!(effects.admission_attempts, 1);
            assert_eq!(effects.admissions.len(), 1);
            assert_reason(
                &effects.admissions[0],
                UsageUnavailableReason::PromptNotSubmitted,
            );
            assert_eq!(effects.acknowledgments, [SchemaU64::new(41)]);

            assert_eq!(
                machine.drive(not_submitted, &mut effects),
                MachineDrive::LateIgnored
            );
            assert!(
                !machine
                    .observe_candidate(reported(["9", "4", "5", "1", "2", "3"]))
                    .unwrap()
            );
            assert_eq!(effects.admission_attempts, 1);
            assert_eq!(effects.acknowledgment_attempts, 1);
        }

        #[test]
        fn submitted_unknown_settlement_finalizes_immediately_and_ignores_late_inputs() {
            let mut machine = FinalizationMachine::default();
            let mut effects = RecordingEffects::default();
            let unknown = snapshot(true, Some(SettlementBoundary::Unknown));

            assert_finalized(machine.drive(unknown, &mut effects), 41);
            assert_eq!(effects.admission_attempts, 1);
            assert_eq!(effects.admissions.len(), 1);
            assert_reason(
                &effects.admissions[0],
                UsageUnavailableReason::TurnSettlementUnknown,
            );
            assert_eq!(effects.acknowledgment_attempts, 1);
            assert_eq!(effects.acknowledgments, [SchemaU64::new(41)]);

            assert!(
                !machine
                    .observe_candidate(reported(["52", "30", "22", "3", "4", "5"]))
                    .unwrap()
            );
            machine.observe_session_terminal();
            assert_eq!(
                machine.drive(unknown, &mut effects),
                MachineDrive::LateIgnored
            );
            assert_eq!(
                machine.drive(
                    snapshot(true, Some(SettlementBoundary::Authoritative)),
                    &mut effects,
                ),
                MachineDrive::LateIgnored
            );

            assert_eq!(effects.admission_attempts, 1);
            assert_eq!(effects.admissions.len(), 1);
            assert_eq!(effects.acknowledgment_attempts, 1);
            assert_eq!(effects.acknowledgments, [SchemaU64::new(41)]);
        }

        #[test]
        fn authoritative_candidate_before_or_after_settlement_converges_once() {
            let usage = reported(["100", "60", "40", "7", "8", "9"]);
            let submitted = snapshot(true, None);
            let authoritative = snapshot(true, Some(SettlementBoundary::Authoritative));

            let mut candidate_first = FinalizationMachine::default();
            let mut candidate_first_effects = RecordingEffects::default();
            assert!(candidate_first.observe_candidate(usage.clone()).unwrap());
            assert_eq!(
                candidate_first.drive(submitted, &mut candidate_first_effects),
                MachineDrive::Pending
            );
            assert_finalized(
                candidate_first.drive(authoritative, &mut candidate_first_effects),
                41,
            );

            let mut settlement_first = FinalizationMachine::default();
            let mut settlement_first_effects = RecordingEffects::default();
            assert_eq!(
                settlement_first.drive(authoritative, &mut settlement_first_effects),
                MachineDrive::Pending
            );
            assert!(settlement_first.observe_candidate(usage.clone()).unwrap());
            assert_finalized(
                settlement_first.drive(authoritative, &mut settlement_first_effects),
                41,
            );

            assert_eq!(candidate_first_effects.admissions.len(), 1);
            assert_eq!(candidate_first_effects.admissions[0], usage);
            assert_eq!(settlement_first_effects.admissions.len(), 1);
            assert_eq!(settlement_first_effects.admissions[0], usage);
            assert_eq!(candidate_first_effects.acknowledgment_attempts, 1);
            assert_eq!(settlement_first_effects.acknowledgment_attempts, 1);
        }

        #[test]
        fn session_terminal_and_authoritative_settlement_are_first_wins() {
            let usage = reported(["70", "40", "30", "2", "3", "4"]);
            let submitted = snapshot(true, None);
            let authoritative = snapshot(true, Some(SettlementBoundary::Authoritative));

            let mut session_first = FinalizationMachine::default();
            let mut session_first_effects = RecordingEffects::default();
            assert!(session_first.observe_candidate(usage.clone()).unwrap());
            session_first.observe_session_terminal();
            assert_eq!(
                session_first.drive(submitted, &mut session_first_effects),
                MachineDrive::Admitted
            );
            assert_eq!(session_first_effects.admissions.len(), 1);
            assert_reason(
                &session_first_effects.admissions[0],
                UsageUnavailableReason::TurnSettlementUnknown,
            );
            assert_eq!(session_first_effects.acknowledgment_attempts, 0);
            assert_finalized(
                session_first.drive(authoritative, &mut session_first_effects),
                41,
            );
            assert_eq!(session_first_effects.admissions.len(), 1);

            let mut authoritative_first = FinalizationMachine::default();
            let mut authoritative_first_effects = RecordingEffects::default();
            assert!(
                authoritative_first
                    .observe_candidate(usage.clone())
                    .unwrap()
            );
            assert_finalized(
                authoritative_first.drive(authoritative, &mut authoritative_first_effects),
                41,
            );
            authoritative_first.observe_session_terminal();
            assert_eq!(
                authoritative_first.drive(submitted, &mut authoritative_first_effects),
                MachineDrive::LateIgnored
            );
            assert_eq!(authoritative_first_effects.admissions, [usage]);
            assert_eq!(authoritative_first_effects.acknowledgment_attempts, 1);
        }

        #[test]
        fn duplicate_and_late_callbacks_cannot_overwrite_the_first_candidate() {
            let first = reported(["11", "7", "4", "1", "2", "3"]);
            let replacement = reported(["999", "500", "499", "8", "9", "10"]);
            let authoritative = snapshot(true, Some(SettlementBoundary::Authoritative));
            let mut machine = FinalizationMachine::default();
            let mut effects = RecordingEffects::default();

            assert!(machine.observe_candidate(first.clone()).unwrap());
            let duplicate = machine.observe_candidate(replacement.clone()).unwrap_err();
            assert_eq!(
                duplicate.as_str(),
                "usage_finalization_candidate_duplicated"
            );
            assert_finalized(machine.drive(authoritative, &mut effects), 41);
            assert_eq!(effects.admissions, [first]);

            for _ in 0..3 {
                machine.observe_session_terminal();
                assert!(!machine.observe_candidate(replacement.clone()).unwrap());
                assert_eq!(
                    machine.drive(authoritative, &mut effects),
                    MachineDrive::LateIgnored
                );
            }
            assert_eq!(effects.admission_attempts, 1);
            assert_eq!(effects.acknowledgment_attempts, 1);
        }

        #[test]
        fn admission_failure_never_acks_and_is_sticky() {
            const FAILURE: AgentDiagnosticErrorCode =
                AgentDiagnosticErrorCode::new("test_admission_failed");
            let mut machine = FinalizationMachine::default();
            let mut effects = RecordingEffects::failing_admission(FAILURE);
            let not_submitted = snapshot(false, Some(SettlementBoundary::NotSubmitted));

            assert_eq!(
                machine.drive(not_submitted, &mut effects),
                MachineDrive::Failed {
                    error_code: FAILURE,
                    notify: true,
                }
            );
            assert_eq!(effects.admission_attempts, 1);
            assert_eq!(effects.acknowledgment_attempts, 0);
            assert!(effects.admissions.is_empty());
            assert!(effects.acknowledgments.is_empty());

            machine.observe_session_terminal();
            assert!(
                !machine
                    .observe_candidate(reported(["8", "4", "4", "1", "2", "3"]))
                    .unwrap()
            );
            assert_eq!(
                machine.drive(not_submitted, &mut effects),
                MachineDrive::Failed {
                    error_code: FAILURE,
                    notify: false,
                }
            );
            assert_eq!(effects.admission_attempts, 1);
            assert_eq!(effects.acknowledgment_attempts, 0);
        }

        #[test]
        fn authoritative_projection_preserves_six_arbitrary_precision_fields() {
            let values = [
                "184467440737095516160000000000000000000000000001",
                "184467440737095516160000000000000000000000000002",
                "184467440737095516160000000000000000000000000003",
                "184467440737095516160000000000000000000000000004",
                "184467440737095516160000000000000000000000000005",
                "184467440737095516160000000000000000000000000006",
            ];
            let mut machine = FinalizationMachine::default();
            let mut effects = RecordingEffects::default();
            assert!(machine.observe_candidate(reported(values)).unwrap());
            assert_finalized(
                machine.drive(
                    snapshot(true, Some(SettlementBoundary::Authoritative)),
                    &mut effects,
                ),
                41,
            );

            let usage = &effects.admissions[0];
            assert_eq!(usage.availability(), UsageAvailability::Available);
            assert_eq!(usage.source(), Some(UsageSource::AcpPromptResponseUsage));
            assert_eq!(usage.unavailable_reason(), None);
            for (actual, expected) in [
                (usage.provider_total_tokens(), values[0]),
                (usage.input_tokens(), values[1]),
                (usage.output_tokens(), values[2]),
                (usage.thought_tokens(), values[3]),
                (usage.cached_read_tokens(), values[4]),
                (usage.cached_write_tokens(), values[5]),
            ] {
                assert_eq!(actual.map(TokenCount::as_str), Some(expected));
            }
            assert_eq!(effects.acknowledgments, [SchemaU64::new(41)]);
        }

        #[test]
        fn authoritative_unavailable_reasons_are_not_rewritten() {
            for reason in [
                UsageUnavailableReason::SourceUnsupported,
                UsageUnavailableReason::UsageNotReported,
            ] {
                let mut machine = FinalizationMachine::default();
                let mut effects = RecordingEffects::default();
                assert!(
                    machine
                        .observe_candidate(unavailable(reason))
                        .expect("authoritative unavailable candidate")
                );
                assert_finalized(
                    machine.drive(
                        snapshot(true, Some(SettlementBoundary::Authoritative)),
                        &mut effects,
                    ),
                    41,
                );
                assert_reason(&effects.admissions[0], reason);
            }
        }

        #[test]
        fn runtime_router_executes_the_tested_machine_and_is_the_unique_event_owner() {
            let source = fs::read_to_string(
                crate_root().join("src/diagnostic_runtime/usage_finalization.rs"),
            )
            .expect("read usage finalization source");
            assert!(source.contains("entry.machine.drive(snapshot, &mut effects)"));

            let runtime = crate_root().join("src/diagnostic_runtime");
            let owners: Vec<_> = fs::read_dir(runtime)
                .expect("read runtime sources")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("rs"))
                .filter(|path| {
                    fs::read_to_string(path)
                        .expect("read runtime module")
                        .contains("ActTokenUsageFinalized::new(")
                })
                .collect();
            assert_eq!(owners.len(), 1);
            assert!(owners[0].ends_with("usage_finalization.rs"));
        }
    }
}
