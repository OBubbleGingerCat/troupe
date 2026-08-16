use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const SLOT_START: &str = "// checked-usage-finalization-slot:start";
const SLOT_END: &str = "// checked-usage-finalization-slot:end";

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn producer_source() -> String {
    fs::read_to_string(crate_root().join("src/diagnostic_runtime/act_producer.rs"))
        .expect("read Act producer")
}

fn slot_source() -> String {
    let source = producer_source();
    let start = source.find(SLOT_START).expect("slot start marker") + SLOT_START.len();
    let end = source[start..].find(SLOT_END).expect("slot end marker") + start;
    source[start..end].trim().to_owned()
}

struct ScratchDirectory(PathBuf);

impl ScratchDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-diagnostic-usage-slot-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create compile fixture directory");
        Self(path)
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_source(slot: &str, inside_module: &str, main: &str) -> String {
    format!(
        r#"
#![allow(dead_code)]
use std::sync::{{Arc, Mutex, MutexGuard}};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SchemaU64(u64);

impl SchemaU64 {{
    pub(crate) const fn new(value: u64) -> Self {{
        Self(value)
    }}
}}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {{
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}}

mod contract {{
    use super::{{Arc, Mutex, SchemaU64, lock}};

{slot}

{inside_module}
}}

{main}
"#
    )
}

fn rustc(source: &str, directory: &Path, name: &str) -> Output {
    let source_path = directory.join(format!("{name}.rs"));
    let binary_path = directory.join(name);
    fs::write(&source_path, source).expect("write compile fixture");
    Command::new("rustc")
        .args([
            "--edition=2024",
            "--error-format=human",
            "-o",
            binary_path.to_str().expect("UTF-8 binary path"),
        ])
        .arg(&source_path)
        .output()
        .expect("run rustc")
}

fn assert_compile_error(source: &str, expected_code: &str, name: &str) {
    let scratch = ScratchDirectory::new();
    let output = rustc(source, &scratch.0, name);
    assert!(!output.status.success(), "fixture unexpectedly compiled");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_code),
        "expected {expected_code}, stderr:\n{stderr}"
    );
}

#[test]
fn slot_records_only_prompt_and_settlement_state() {
    let slot = slot_source();
    let state_start = slot
        .find("struct UsageFinalizationState {")
        .expect("slot state");
    let state_end = slot[state_start..]
        .find("}\n\n#[derive(Debug)]")
        .expect("slot state end")
        + state_start;
    let state = &slot[state_start..state_end];

    assert!(state.contains("prompt_submitted: bool"));
    assert!(state.contains("settlement: Option<UsageFinalizationSettlement>"));
    assert_eq!(state.matches(':').count(), 2);
    for forbidden in [
        "DiagnosticEvent",
        "AgentDiagnosticCandidate",
        "ActTokenUsageFinalized",
        "token",
        "usage_sequence",
    ] {
        assert!(
            !state.contains(forbidden),
            "slot state contains {forbidden}"
        );
    }
}

#[test]
fn extracted_slot_transitions_and_acknowledgment_run() {
    let source = fixture_source(
        &slot_source(),
        r#"
pub(crate) fn exercise() {
    let (identity, slot) = UsageFinalizationIdentity::new("act-7");
    assert_eq!(slot.act_id(), "act-7");
    assert!(!slot.snapshot().prompt_submitted());
    assert_eq!(slot.snapshot().settlement(), None);

    let slot = match slot.acknowledge(SchemaU64::new(1)) {
        Err(slot) => slot,
        Ok(_) => panic!("an unsettled slot produced an acknowledgment"),
    };
    identity.mark_prompt_submitted().expect("submit prompt");
    assert!(slot.snapshot().prompt_submitted());
    assert!(identity.mark_prompt_submitted().is_err());
    assert!(identity
        .settle(UsageFinalizationSettlement::NotSubmitted)
        .is_err());
    assert_eq!(
        identity.settle(UsageFinalizationSettlement::Authoritative),
        Ok(true)
    );
    assert_eq!(
        identity.settle(UsageFinalizationSettlement::Authoritative),
        Ok(false)
    );
    assert_eq!(
        slot.snapshot().settlement(),
        Some(UsageFinalizationSettlement::Authoritative)
    );

    let ack = slot
        .acknowledge(SchemaU64::new(41))
        .unwrap_or_else(|_| panic!("settled slot must acknowledge"));
    assert_eq!(ack.act_id(), "act-7");
    assert_eq!(ack.usage_sequence(), SchemaU64::new(41));
    assert!(identity.matches(&ack.identity));
}
"#,
        "fn main() { contract::exercise(); }",
    );
    let scratch = ScratchDirectory::new();
    let output = rustc(&source, &scratch.0, "positive");
    assert!(
        output.status.success(),
        "positive fixture failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = Command::new(scratch.0.join("positive"))
        .status()
        .expect("run positive fixture");
    assert!(status.success());
}

#[test]
fn usage_slot_cannot_be_cloned() {
    let source = fixture_source(
        &slot_source(),
        "",
        r#"
fn duplicate(slot: contract::UsageFinalizationSlot) {
    let _copy = slot.clone();
}

fn main() {}
"#,
    );
    assert_compile_error(&source, "error[E0599]", "slot-clone");
}

#[test]
fn usage_acknowledgment_cannot_be_forged() {
    let source = fixture_source(
        &slot_source(),
        "",
        r#"
fn forge() -> contract::UsageFinalizationAck {
    contract::UsageFinalizationAck {
        identity: todo!(),
        usage_sequence: SchemaU64::new(1),
    }
}

fn main() {}
"#,
    );
    assert_compile_error(&source, "error[E0451]", "ack-forge");
}

#[test]
fn lifecycle_finish_requires_the_consumed_slot_ack_sequence() {
    let source = producer_source();
    let start = source
        .find("fn maybe_finish(&self")
        .expect("Act finish gate");
    let end = source[start..]
        .find("fn fail_diagnostic(")
        .expect("Act finish gate end")
        + start;
    let finish = &source[start..end];

    assert!(finish.contains("Some(usage_sequence)"));
    assert!(finish.contains("state.usage_ack_sequence"));
    assert!(finish.contains("follows_from(usage_sequence)"));
    assert!(!finish.contains("ActTokenUsageFinalized"));
}
