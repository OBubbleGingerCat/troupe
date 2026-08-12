use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "agent-test-support")]
use std::collections::VecDeque;
#[cfg(feature = "agent-test-support")]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(feature = "agent-test-support")]
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
#[cfg(feature = "agent-test-support")]
use tokio::sync::Notify;

use crate::error::AgentStartupFailure;
use crate::profile::AgentKind;

pub(super) mod fd_registry;
pub(super) mod process;

const ACP_CLIENT_SDK_VERSION: &str = "2.0.0";

pub(crate) enum LaunchRunner {
    Npx {
        package: &'static str,
        exact_version: &'static str,
        fixed_args: &'static [&'static str],
    },
    Command {
        program: &'static str,
        fixed_args: &'static [&'static str],
        exact_version: &'static str,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AcpWireProtocolVersion {
    StableV1,
}

impl AcpWireProtocolVersion {
    const fn as_str(self) -> &'static str {
        match self {
            Self::StableV1 => "stable-v1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum McpWireProtocolVersion {
    V2025_06_18,
    V2025_11_25,
}

impl McpWireProtocolVersion {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::V2025_06_18 => "2025-06-18",
            Self::V2025_11_25 => "2025-11-25",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum McpTransportProfileId {
    V1,
}

impl McpTransportProfileId {
    const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "McpTransportProfileV1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaunchEnvironmentPolicy {
    InheritParent,
}

impl LaunchEnvironmentPolicy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InheritParent => "inherit_parent",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModeApplicationV1 {
    SessionConfigOption {
        config_id: &'static str,
        value: &'static str,
    },
    #[allow(dead_code)]
    LegacySessionMode { mode_id: &'static str },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResolvedModeApplication {
    SessionConfigOption { config_id: String, value: String },
    LegacySessionMode { mode_id: String },
}

impl From<ModeApplicationV1> for ResolvedModeApplication {
    fn from(application: ModeApplicationV1) -> Self {
        match application {
            ModeApplicationV1::SessionConfigOption { config_id, value } => {
                Self::SessionConfigOption {
                    config_id: config_id.to_owned(),
                    value: value.to_owned(),
                }
            }
            ModeApplicationV1::LegacySessionMode { mode_id } => Self::LegacySessionMode {
                mode_id: mode_id.to_owned(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigurationOrderV1 {
    ModeModelEffort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpeningRequestPhaseV1 {
    Initialize,
    SessionNew,
    Configure,
}

impl OpeningRequestPhaseV1 {
    #[cfg(feature = "agent-test-support")]
    fn parse(value: &str) -> Option<Self> {
        match value {
            "initialize" => Some(Self::Initialize),
            "session_new" => Some(Self::SessionNew),
            "configure" => Some(Self::Configure),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OpeningTransientErrorV1 {
    pub(crate) phase: OpeningRequestPhaseV1,
    pub(crate) code: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectiveValueValidationV1 {
    ExactAdvertisedSelect,
}

impl EffectiveValueValidationV1 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ExactAdvertisedSelect => "exact_advertised_select",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum McpRegistrationV1 {
    SessionNewHttp,
}

impl McpRegistrationV1 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SessionNewHttp => "session/new.mcpServers.http",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AutonomousRequestProfileId(&'static str);

impl AutonomousRequestProfileId {
    const fn as_str(self) -> &'static str {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SettlementProfileId(&'static str);

impl SettlementProfileId {
    const fn as_str(self) -> &'static str {
        self.0
    }
}

pub(crate) struct AgentLaunchSpec {
    pub(crate) agent: AgentKind,
    pub(crate) acp_wire_protocol: AcpWireProtocolVersion,
    pub(crate) client_sdk_version: &'static str,
    pub(crate) mcp_wire_protocol: McpWireProtocolVersion,
    pub(crate) mcp_transport_profile: McpTransportProfileId,
    pub(crate) runner: LaunchRunner,
    pub(crate) environment_policy: LaunchEnvironmentPolicy,
    pub(crate) fixed_environment: &'static [(&'static str, &'static str)],
    pub(crate) removed_environment: &'static [&'static str],
    pub(crate) initial_mode: &'static str,
    pub(crate) mode_application: ModeApplicationV1,
    pub(crate) model_config_id: &'static str,
    pub(crate) effort_config_id: &'static str,
    pub(crate) effort_option_optional_when_unspecified: bool,
    pub(crate) configuration_order: ConfigurationOrderV1,
    pub(crate) effective_value_validation: EffectiveValueValidationV1,
    pub(crate) mcp_registration: McpRegistrationV1,
    pub(crate) autonomous_request_profile: AutonomousRequestProfileId,
    pub(crate) settlement_profile: SettlementProfileId,
    pub(crate) opening_transient_errors: &'static [OpeningTransientErrorV1],
    pub(crate) authoritative_prompt_error_codes: &'static [i32],
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct NpxPreparationKey {
    agent: AgentKind,
    package: &'static str,
    exact_version: &'static str,
}

impl AgentLaunchSpec {
    pub(crate) const fn npx_preparation_key(&self) -> Option<NpxPreparationKey> {
        match self.runner {
            LaunchRunner::Npx {
                package,
                exact_version,
                ..
            } => Some(NpxPreparationKey {
                agent: self.agent,
                package,
                exact_version,
            }),
            LaunchRunner::Command { .. } => None,
        }
    }

    pub(crate) const fn required_agent_info_version(&self) -> Option<&'static str> {
        match self.runner {
            LaunchRunner::Npx { .. } => None,
            LaunchRunner::Command { exact_version, .. } => Some(exact_version),
        }
    }

    pub(crate) fn supports_step1_opening(&self, agent: AgentKind) -> bool {
        let mode_matches = match self.mode_application {
            ModeApplicationV1::SessionConfigOption { value, .. } => value == self.initial_mode,
            ModeApplicationV1::LegacySessionMode { mode_id } => mode_id == self.initial_mode,
        };
        self.agent == agent
            && self.acp_wire_protocol.as_str() == "stable-v1"
            && self.client_sdk_version == ACP_CLIENT_SDK_VERSION
            && matches!(
                self.mcp_wire_protocol,
                McpWireProtocolVersion::V2025_06_18 | McpWireProtocolVersion::V2025_11_25
            )
            && self.mcp_transport_profile.as_str() == "McpTransportProfileV1"
            && self.environment_policy.as_str() == "inherit_parent"
            && mode_matches
            && matches!(
                self.configuration_order,
                ConfigurationOrderV1::ModeModelEffort
            )
            && self.effective_value_validation.as_str() == "exact_advertised_select"
            && self.mcp_registration.as_str() == "session/new.mcpServers.http"
            && !self.autonomous_request_profile.as_str().is_empty()
            && !self.settlement_profile.as_str().is_empty()
    }
}

const NO_ARGS: &[&str] = &[];
const NO_ENVIRONMENT: &[(&str, &str)] = &[];
const NO_REMOVED_ENVIRONMENT: &[&str] = &[];
const NO_ERROR_CODES: &[i32] = &[];
const NO_TRANSIENT_OPENING_ERRORS: &[OpeningTransientErrorV1] = &[];
const CODEX_ENVIRONMENT: &[(&str, &str)] = &[("INITIAL_AGENT_MODE", "agent")];
const CODEX_REMOVED_ENVIRONMENT: &[&str] = &["CODEX_PATH"];

const CODEX: AgentLaunchSpec = AgentLaunchSpec {
    agent: AgentKind::Codex,
    acp_wire_protocol: AcpWireProtocolVersion::StableV1,
    client_sdk_version: ACP_CLIENT_SDK_VERSION,
    mcp_wire_protocol: McpWireProtocolVersion::V2025_06_18,
    mcp_transport_profile: McpTransportProfileId::V1,
    runner: LaunchRunner::Npx {
        package: "@agentclientprotocol/codex-acp",
        exact_version: "1.1.9",
        fixed_args: NO_ARGS,
    },
    environment_policy: LaunchEnvironmentPolicy::InheritParent,
    fixed_environment: CODEX_ENVIRONMENT,
    removed_environment: CODEX_REMOVED_ENVIRONMENT,
    initial_mode: "agent",
    mode_application: ModeApplicationV1::SessionConfigOption {
        config_id: "mode",
        value: "agent",
    },
    model_config_id: "model",
    effort_config_id: "reasoning_effort",
    effort_option_optional_when_unspecified: false,
    configuration_order: ConfigurationOrderV1::ModeModelEffort,
    effective_value_validation: EffectiveValueValidationV1::ExactAdvertisedSelect,
    mcp_registration: McpRegistrationV1::SessionNewHttp,
    autonomous_request_profile: AutonomousRequestProfileId("codex-acp@1.1.9"),
    settlement_profile: SettlementProfileId("codex-acp@1.1.9"),
    opening_transient_errors: NO_TRANSIENT_OPENING_ERRORS,
    authoritative_prompt_error_codes: NO_ERROR_CODES,
};

const CLAUDE: AgentLaunchSpec = AgentLaunchSpec {
    agent: AgentKind::Claude,
    acp_wire_protocol: AcpWireProtocolVersion::StableV1,
    client_sdk_version: ACP_CLIENT_SDK_VERSION,
    mcp_wire_protocol: McpWireProtocolVersion::V2025_11_25,
    mcp_transport_profile: McpTransportProfileId::V1,
    runner: LaunchRunner::Npx {
        package: "@agentclientprotocol/claude-agent-acp",
        exact_version: "0.64.2",
        fixed_args: NO_ARGS,
    },
    environment_policy: LaunchEnvironmentPolicy::InheritParent,
    fixed_environment: NO_ENVIRONMENT,
    removed_environment: NO_REMOVED_ENVIRONMENT,
    initial_mode: "default",
    mode_application: ModeApplicationV1::SessionConfigOption {
        config_id: "mode",
        value: "default",
    },
    model_config_id: "model",
    effort_config_id: "effort",
    effort_option_optional_when_unspecified: true,
    configuration_order: ConfigurationOrderV1::ModeModelEffort,
    effective_value_validation: EffectiveValueValidationV1::ExactAdvertisedSelect,
    mcp_registration: McpRegistrationV1::SessionNewHttp,
    autonomous_request_profile: AutonomousRequestProfileId("claude-agent-acp@0.64.2"),
    settlement_profile: SettlementProfileId("claude-agent-acp@0.64.2"),
    opening_transient_errors: NO_TRANSIENT_OPENING_ERRORS,
    authoritative_prompt_error_codes: NO_ERROR_CODES,
};

const KIMI_ARGS: &[&str] = &["acp"];
const KIMI: AgentLaunchSpec = AgentLaunchSpec {
    agent: AgentKind::Kimi,
    acp_wire_protocol: AcpWireProtocolVersion::StableV1,
    client_sdk_version: ACP_CLIENT_SDK_VERSION,
    mcp_wire_protocol: McpWireProtocolVersion::V2025_11_25,
    mcp_transport_profile: McpTransportProfileId::V1,
    runner: LaunchRunner::Command {
        program: "kimi",
        fixed_args: KIMI_ARGS,
        exact_version: "0.31.1",
    },
    environment_policy: LaunchEnvironmentPolicy::InheritParent,
    fixed_environment: NO_ENVIRONMENT,
    removed_environment: NO_REMOVED_ENVIRONMENT,
    initial_mode: "default",
    mode_application: ModeApplicationV1::SessionConfigOption {
        config_id: "mode",
        value: "default",
    },
    model_config_id: "model",
    effort_config_id: "thinking",
    effort_option_optional_when_unspecified: true,
    configuration_order: ConfigurationOrderV1::ModeModelEffort,
    effective_value_validation: EffectiveValueValidationV1::ExactAdvertisedSelect,
    mcp_registration: McpRegistrationV1::SessionNewHttp,
    autonomous_request_profile: AutonomousRequestProfileId("kimi-code@0.31.1"),
    settlement_profile: SettlementProfileId("kimi-code@0.31.1"),
    opening_transient_errors: NO_TRANSIENT_OPENING_ERRORS,
    authoritative_prompt_error_codes: NO_ERROR_CODES,
};

pub(crate) const fn launch_spec(agent: AgentKind) -> &'static AgentLaunchSpec {
    match agent {
        AgentKind::Codex => &CODEX,
        AgentKind::Claude => &CLAUDE,
        AgentKind::Kimi => &KIMI,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedAgentCommand {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) environment: Vec<(OsString, OsString)>,
    pub(crate) removed_environment: Vec<OsString>,
    pub(crate) mode_application: ResolvedModeApplication,
    pub(crate) opening_transient_errors: Vec<OpeningTransientErrorV1>,
    pub(crate) authoritative_prompt_error_codes: Arc<[i32]>,
    #[cfg(feature = "agent-test-support")]
    pub(crate) opening_gate: Option<Arc<TestOpeningGate>>,
    #[cfg(feature = "agent-test-support")]
    pub(crate) configuration_ready_gate: Option<Arc<TestOpeningGate>>,
    #[cfg(feature = "agent-test-support")]
    pub(crate) mcp_ready_gate: Option<Arc<TestOpeningGate>>,
    #[cfg(feature = "agent-test-support")]
    pub(crate) opening_backoff: Option<Arc<TestOpeningBackoff>>,
    #[cfg(feature = "agent-test-support")]
    pub(crate) turn_gates: TestTurnGates,
}

#[derive(Clone, Debug)]
pub struct ResolvedLaunch(pub(crate) ResolvedLaunchKind);

#[derive(Clone, Debug)]
pub(crate) enum ResolvedLaunchKind {
    #[cfg_attr(not(feature = "agent-test-support"), allow(dead_code))]
    Inert,
    Process(Box<ResolvedAgentCommand>),
}

fn unavailable() -> AgentStartupFailure {
    AgentStartupFailure::start(
        "launcher_unavailable",
        "preparation",
        "agent launcher is unavailable",
    )
}

fn validate_executable(path: &Path) -> Result<PathBuf, AgentStartupFailure> {
    let path = std::fs::canonicalize(path).map_err(|_| unavailable())?;
    let metadata = std::fs::metadata(&path).map_err(|_| unavailable())?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(unavailable());
    }
    Ok(path)
}

fn resolve_program(program: &Path) -> Result<PathBuf, AgentStartupFailure> {
    if program.components().count() > 1 || program.is_absolute() {
        return validate_executable(program);
    }
    let path = std::env::var_os("PATH").ok_or_else(unavailable)?;
    for directory in std::env::split_paths(&path) {
        if let Ok(resolved) = validate_executable(&directory.join(program)) {
            return Ok(resolved);
        }
    }
    Err(unavailable())
}

#[cfg(not(feature = "agent-test-support"))]
fn production_command(
    spec: &'static AgentLaunchSpec,
) -> Result<ResolvedAgentCommand, AgentStartupFailure> {
    let (program, args) = match spec.runner {
        LaunchRunner::Npx {
            package,
            exact_version,
            fixed_args,
        } => (
            PathBuf::from("npx"),
            [
                OsString::from("--yes"),
                OsString::from(format!("{package}@{exact_version}")),
            ]
            .into_iter()
            .chain(fixed_args.iter().map(OsString::from))
            .collect(),
        ),
        LaunchRunner::Command {
            program,
            fixed_args,
            ..
        } => (
            PathBuf::from(program),
            fixed_args.iter().map(OsString::from).collect(),
        ),
    };
    let environment = match spec.environment_policy {
        LaunchEnvironmentPolicy::InheritParent => spec
            .fixed_environment
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
    };
    Ok(ResolvedAgentCommand {
        program: resolve_program(&program)?,
        args,
        environment,
        removed_environment: spec
            .removed_environment
            .iter()
            .map(OsString::from)
            .collect(),
        mode_application: spec.mode_application.into(),
        opening_transient_errors: spec.opening_transient_errors.to_vec(),
        authoritative_prompt_error_codes: Arc::from(spec.authoritative_prompt_error_codes),
        #[cfg(feature = "agent-test-support")]
        opening_gate: None,
        #[cfg(feature = "agent-test-support")]
        configuration_ready_gate: None,
        #[cfg(feature = "agent-test-support")]
        mcp_ready_gate: None,
        #[cfg(feature = "agent-test-support")]
        opening_backoff: None,
        #[cfg(feature = "agent-test-support")]
        turn_gates: TestTurnGates::default(),
    })
}

#[cfg(feature = "agent-test-support")]
#[derive(Clone, Debug)]
struct TestLaunch {
    program: PathBuf,
    args: Vec<OsString>,
    opening_gate: Option<Arc<TestOpeningGate>>,
    configuration_ready_gate: Option<Arc<TestOpeningGate>>,
    mcp_ready_gate: Option<Arc<TestOpeningGate>>,
    opening_backoff: Option<Arc<TestOpeningBackoff>>,
    turn_gates: TestTurnGates,
    legacy_mode_id: Option<String>,
    transient_opening_errors: Vec<OpeningTransientErrorV1>,
    authoritative_prompt_error_codes: Vec<i32>,
}

#[cfg(feature = "agent-test-support")]
#[derive(Clone, Debug, Default)]
pub(crate) struct TestTurnGates {
    pub(crate) registration: Option<Arc<TestOpeningGate>>,
    pub(crate) intake: Option<Arc<TestOpeningGate>>,
    pub(crate) submission: Option<Arc<TestOpeningGate>>,
    pub(crate) response_flush: Option<Arc<TestOpeningGate>>,
    pub(crate) settlement: Option<Arc<TestOpeningGate>>,
    pub(crate) terminal_delivery: Option<Arc<TestOpeningGate>>,
    pub(crate) outcome: Option<Arc<TestOpeningGate>>,
}

#[cfg(feature = "agent-test-support")]
#[derive(Debug)]
pub(crate) struct TestOpeningGate {
    arrived: AtomicBool,
    released: AtomicBool,
    completed: AtomicBool,
    changed: Notify,
    blocking_lock: Mutex<()>,
    blocking_changed: Condvar,
}

#[cfg(feature = "agent-test-support")]
#[derive(Debug)]
pub(crate) struct TestOpeningBackoff {
    random_words: Mutex<VecDeque<u64>>,
    arrivals: AtomicU64,
    releases: AtomicU64,
    completions: AtomicU64,
    delays_ms: Mutex<Vec<u64>>,
    changed: Notify,
}

#[cfg(feature = "agent-test-support")]
impl TestOpeningBackoff {
    fn new(random_words: Vec<u64>) -> Arc<Self> {
        Arc::new(Self {
            random_words: Mutex::new(random_words.into()),
            arrivals: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            completions: AtomicU64::new(0),
            delays_ms: Mutex::new(Vec::new()),
            changed: Notify::new(),
        })
    }

    pub(crate) fn next_random_word(&self) -> Option<u64> {
        lock(&self.random_words).pop_front()
    }

    pub(crate) async fn wait(
        &self,
        delay_ms: u64,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        lock(&self.delays_ms).push(delay_ms);
        let arrival = self.arrivals.fetch_add(1, Ordering::AcqRel) + 1;
        loop {
            if self.releases.load(Ordering::Acquire) >= arrival {
                self.completions.fetch_add(1, Ordering::AcqRel);
                return true;
            }
            let changed = self.changed.notified();
            if self.releases.load(Ordering::Acquire) >= arrival {
                continue;
            }
            tokio::select! {
                () = changed => {}
                () = cancellation.cancelled() => return false,
            }
        }
    }

    fn release_one(&self) {
        self.releases.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_waiters();
    }
}

#[cfg(feature = "agent-test-support")]
impl TestOpeningGate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            arrived: AtomicBool::new(false),
            released: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            changed: Notify::new(),
            blocking_lock: Mutex::new(()),
            blocking_changed: Condvar::new(),
        })
    }

    pub(crate) async fn wait(&self) {
        self.arrived.store(true, Ordering::Release);
        while !self.released.load(Ordering::Acquire) {
            let changed = self.changed.notified();
            if self.released.load(Ordering::Acquire) {
                break;
            }
            changed.await;
        }
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.changed.notify_waiters();
        self.blocking_changed.notify_all();
    }

    pub(crate) fn wait_blocking(&self) {
        self.arrived.store(true, Ordering::Release);
        let mut guard = lock(&self.blocking_lock);
        while !self.released.load(Ordering::Acquire) {
            guard = self
                .blocking_changed
                .wait(guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(crate) fn mark_completed(&self) {
        self.completed.store(true, Ordering::Release);
    }

    pub(crate) fn states(&self) -> (bool, bool) {
        (
            self.arrived.load(Ordering::Acquire),
            self.completed.load(Ordering::Acquire),
        )
    }
}

#[cfg(feature = "agent-test-support")]
fn test_launch() -> &'static Mutex<Option<TestLaunch>> {
    static TEST_LAUNCH: OnceLock<Mutex<Option<TestLaunch>>> = OnceLock::new();
    TEST_LAUNCH.get_or_init(|| Mutex::new(None))
}

#[cfg(feature = "agent-test-support")]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn resolve_launch(agent: AgentKind) -> Result<ResolvedLaunch, AgentStartupFailure> {
    #[cfg(feature = "agent-test-support")]
    {
        let _ = agent;
        let configured = lock(test_launch()).clone();
        let Some(configured) = configured else {
            return Ok(ResolvedLaunch(ResolvedLaunchKind::Inert));
        };
        let mut authoritative_prompt_error_codes = configured.authoritative_prompt_error_codes;
        authoritative_prompt_error_codes
            .extend_from_slice(launch_spec(agent).authoritative_prompt_error_codes);
        authoritative_prompt_error_codes.sort_unstable();
        authoritative_prompt_error_codes.dedup();
        Ok(ResolvedLaunch(ResolvedLaunchKind::Process(Box::new(
            ResolvedAgentCommand {
                program: resolve_program(&configured.program)?,
                args: configured.args,
                environment: Vec::new(),
                removed_environment: launch_spec(agent)
                    .removed_environment
                    .iter()
                    .map(OsString::from)
                    .collect(),
                mode_application: configured.legacy_mode_id.map_or_else(
                    || launch_spec(agent).mode_application.into(),
                    |mode_id| ResolvedModeApplication::LegacySessionMode { mode_id },
                ),
                opening_transient_errors: configured.transient_opening_errors,
                authoritative_prompt_error_codes: Arc::from(authoritative_prompt_error_codes),
                opening_gate: configured.opening_gate,
                configuration_ready_gate: configured.configuration_ready_gate,
                mcp_ready_gate: configured.mcp_ready_gate,
                opening_backoff: configured.opening_backoff,
                turn_gates: configured.turn_gates,
            },
        ))))
    }

    #[cfg(not(feature = "agent-test-support"))]
    {
        production_command(launch_spec(agent))
            .map(Box::new)
            .map(ResolvedLaunchKind::Process)
            .map(ResolvedLaunch)
    }
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_set_launch")]
#[pyo3(signature = (*, program, args, legacy_mode_id=None, transient_opening_errors=None, authoritative_prompt_error_codes=None))]
pub fn set_test_launch(
    program: PathBuf,
    args: Vec<OsString>,
    legacy_mode_id: Option<String>,
    transient_opening_errors: Option<Vec<(String, i32)>>,
    authoritative_prompt_error_codes: Option<Vec<i32>>,
) -> pyo3::PyResult<()> {
    let transient_opening_errors = transient_opening_errors
        .unwrap_or_default()
        .into_iter()
        .map(|(phase, code)| {
            OpeningRequestPhaseV1::parse(&phase)
                .map(|phase| OpeningTransientErrorV1 { phase, code })
                .ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err(
                        "transient Opening error phase must be initialize, session_new, or configure",
                    )
                })
        })
        .collect::<pyo3::PyResult<Vec<_>>>()?;
    *lock(test_launch()) = Some(TestLaunch {
        program,
        args,
        opening_gate: None,
        configuration_ready_gate: None,
        mcp_ready_gate: None,
        opening_backoff: None,
        turn_gates: TestTurnGates::default(),
        legacy_mode_id,
        transient_opening_errors,
        authoritative_prompt_error_codes: authoritative_prompt_error_codes
            .unwrap_or_else(|| vec![-32603, -32700]),
    });
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_reset_launch")]
pub fn reset_test_launch() {
    *lock(test_launch()) = None;
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_opening_backoff")]
#[pyo3(signature = (*, random_words))]
pub fn hold_test_opening_backoff(random_words: Vec<u64>) -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding Opening backoff",
        )
    })?;
    launch.opening_backoff = Some(TestOpeningBackoff::new(random_words));
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_opening_backoff")]
pub fn release_test_opening_backoff() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let backoff = launch
        .as_ref()
        .and_then(|launch| launch.opening_backoff.as_ref())
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("Opening backoff is not held"))?;
    backoff.release_one();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_opening_backoff_state")]
pub fn opening_backoff_state(py: pyo3::Python<'_>) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
    use pyo3::types::{PyDict, PyDictMethods};

    let launch = lock(test_launch());
    let backoff = launch
        .as_ref()
        .and_then(|launch| launch.opening_backoff.as_ref())
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("Opening backoff is not held"))?;
    let snapshot = PyDict::new(py);
    snapshot.set_item("arrivals", backoff.arrivals.load(Ordering::Acquire))?;
    snapshot.set_item("releases", backoff.releases.load(Ordering::Acquire))?;
    snapshot.set_item("completions", backoff.completions.load(Ordering::Acquire))?;
    snapshot.set_item("delays_ms", lock(&backoff.delays_ms).clone())?;
    snapshot.set_item("random_words_remaining", lock(&backoff.random_words).len())?;
    Ok(snapshot.into_any().unbind())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_opening")]
pub fn hold_test_opening() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err("configure a test launch before holding Opening")
    })?;
    launch.opening_gate = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_opening")]
pub fn release_test_opening() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.opening_gate.as_ref())
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("Opening is not held"))?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_configuration_ready")]
pub fn hold_test_configuration_ready() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding configuration readiness",
        )
    })?;
    launch.configuration_ready_gate = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_configuration_ready")]
pub fn release_test_configuration_ready() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.configuration_ready_gate.as_ref())
        .ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("configuration readiness is not held")
        })?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_mcp_ready")]
pub fn hold_test_mcp_ready() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding MCP readiness",
        )
    })?;
    launch.mcp_ready_gate = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_mcp_ready")]
pub fn release_test_mcp_ready() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.mcp_ready_gate.as_ref())
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("MCP readiness is not held"))?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_turn_registration")]
pub fn hold_test_turn_registration() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding turn registration",
        )
    })?;
    launch.turn_gates.registration = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_turn_registration")]
pub fn release_test_turn_registration() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.turn_gates.registration.as_ref())
        .ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("turn registration is not held")
        })?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_turn_intake")]
pub fn hold_test_turn_intake() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding turn intake",
        )
    })?;
    launch.turn_gates.intake = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_turn_intake")]
pub fn release_test_turn_intake() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.turn_gates.intake.as_ref())
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("turn intake is not held"))?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_turn_submission")]
pub fn hold_test_turn_submission() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding turn submission",
        )
    })?;
    launch.turn_gates.submission = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_turn_submission")]
pub fn release_test_turn_submission() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.turn_gates.submission.as_ref())
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("turn submission is not held"))?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_turn_response_flush")]
pub fn hold_test_turn_response_flush() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding turn response flush",
        )
    })?;
    launch.turn_gates.response_flush = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_turn_response_flush")]
pub fn release_test_turn_response_flush() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.turn_gates.response_flush.as_ref())
        .ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("turn response flush is not held")
        })?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_turn_settlement")]
pub fn hold_test_turn_settlement() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding turn settlement",
        )
    })?;
    launch.turn_gates.settlement = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_turn_settlement")]
pub fn release_test_turn_settlement() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.turn_gates.settlement.as_ref())
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("turn settlement is not held"))?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_turn_terminal_delivery")]
pub fn hold_test_turn_terminal_delivery() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding turn terminal delivery",
        )
    })?;
    launch.turn_gates.terminal_delivery = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_turn_terminal_delivery")]
pub fn release_test_turn_terminal_delivery() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.turn_gates.terminal_delivery.as_ref())
        .ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("turn terminal delivery is not held")
        })?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_hold_turn_outcome")]
pub fn hold_test_turn_outcome() -> pyo3::PyResult<()> {
    let mut launch = lock(test_launch());
    let launch = launch.as_mut().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "configure a test launch before holding turn outcome",
        )
    })?;
    launch.turn_gates.outcome = Some(TestOpeningGate::new());
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_release_turn_outcome")]
pub fn release_test_turn_outcome() -> pyo3::PyResult<()> {
    let launch = lock(test_launch());
    let gate = launch
        .as_ref()
        .and_then(|launch| launch.turn_gates.outcome.as_ref())
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("turn outcome is not held"))?;
    gate.release();
    Ok(())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_turn_gate_states")]
pub fn turn_gate_states(py: pyo3::Python<'_>) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
    use pyo3::types::{PyDict, PyDictMethods};

    let launch = lock(test_launch());
    let snapshot = PyDict::new(py);
    let launch = launch.as_ref().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err("configure a test launch before reading gates")
    })?;
    for (name, gate) in [
        ("registration", launch.turn_gates.registration.as_ref()),
        ("intake", launch.turn_gates.intake.as_ref()),
        ("submission", launch.turn_gates.submission.as_ref()),
        ("response_flush", launch.turn_gates.response_flush.as_ref()),
        ("settlement", launch.turn_gates.settlement.as_ref()),
        (
            "terminal_delivery",
            launch.turn_gates.terminal_delivery.as_ref(),
        ),
        ("outcome", launch.turn_gates.outcome.as_ref()),
    ] {
        if let Some(gate) = gate {
            let (arrived, completed) = gate.states();
            let states = PyDict::new(py);
            states.set_item("arrived", arrived)?;
            states.set_item("completed", completed)?;
            snapshot.set_item(name, states)?;
        }
    }
    Ok(snapshot.into_any().unbind())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_test_readiness_gate_states")]
pub fn readiness_gate_states(py: pyo3::Python<'_>) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
    use pyo3::types::{PyDict, PyDictMethods};

    let launch = lock(test_launch());
    let launch = launch.as_ref().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err("configure a test launch before reading gates")
    })?;
    let snapshot = PyDict::new(py);
    for (name, gate) in [
        ("configuration", launch.configuration_ready_gate.as_ref()),
        ("mcp", launch.mcp_ready_gate.as_ref()),
    ] {
        let gate = gate.ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err(format!("{name} readiness is not held"))
        })?;
        let (arrived, completed) = gate.states();
        let states = PyDict::new(py);
        states.set_item("arrived", arrived)?;
        states.set_item("completed", completed)?;
        snapshot.set_item(name, states)?;
    }
    Ok(snapshot.into_any().unbind())
}

#[cfg(feature = "agent-test-support")]
#[pyo3::pyfunction(name = "_agent_launch_specs_for_test")]
pub fn launch_specs_for_test(py: pyo3::Python<'_>) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
    use pyo3::types::{PyDict, PyDictMethods};

    let snapshot = PyDict::new(py);
    for spec in [&CODEX, &CLAUDE, &KIMI] {
        let value = PyDict::new(py);
        let (program, args, version) = match spec.runner {
            LaunchRunner::Npx {
                package,
                exact_version,
                fixed_args,
            } => (
                "npx",
                ["--yes".to_owned(), format!("{package}@{exact_version}")]
                    .into_iter()
                    .chain(fixed_args.iter().map(|value| (*value).to_owned()))
                    .collect::<Vec<_>>(),
                exact_version,
            ),
            LaunchRunner::Command {
                program,
                fixed_args,
                exact_version,
            } => (
                program,
                fixed_args.iter().map(|value| (*value).to_owned()).collect(),
                exact_version,
            ),
        };
        value.set_item("program", program)?;
        value.set_item("args", args)?;
        value.set_item("version", version)?;
        value.set_item("acp_wire_protocol", spec.acp_wire_protocol.as_str())?;
        value.set_item("client_sdk_version", spec.client_sdk_version)?;
        value.set_item("mcp_wire_protocol", spec.mcp_wire_protocol.as_str())?;
        value.set_item("mcp_transport_profile", spec.mcp_transport_profile.as_str())?;
        value.set_item("environment_policy", spec.environment_policy.as_str())?;
        let fixed_environment = PyDict::new(py);
        for (name, environment_value) in spec.fixed_environment {
            fixed_environment.set_item(name, environment_value)?;
        }
        value.set_item("fixed_environment", fixed_environment)?;
        value.set_item("removed_environment", spec.removed_environment)?;
        value.set_item("initial_mode", spec.initial_mode)?;
        let mode_application = PyDict::new(py);
        match spec.mode_application {
            ModeApplicationV1::SessionConfigOption {
                config_id,
                value: mode_value,
            } => {
                mode_application.set_item("method", "session/set_config_option")?;
                mode_application.set_item("config_id", config_id)?;
                mode_application.set_item("value", mode_value)?;
            }
            ModeApplicationV1::LegacySessionMode { mode_id } => {
                mode_application.set_item("method", "session/set_mode")?;
                mode_application.set_item("mode_id", mode_id)?;
            }
        }
        value.set_item("mode_application", mode_application)?;
        value.set_item("model_config_id", spec.model_config_id)?;
        value.set_item("effort_config_id", spec.effort_config_id)?;
        value.set_item(
            "effort_option_optional_when_unspecified",
            spec.effort_option_optional_when_unspecified,
        )?;
        match spec.configuration_order {
            ConfigurationOrderV1::ModeModelEffort => {
                value.set_item("configuration_order", ["mode", "model", "effort"])?;
            }
        }
        value.set_item(
            "effective_value_validation",
            spec.effective_value_validation.as_str(),
        )?;
        value.set_item("mcp_registration", spec.mcp_registration.as_str())?;
        value.set_item(
            "autonomous_request_profile",
            spec.autonomous_request_profile.as_str(),
        )?;
        value.set_item("settlement_profile", spec.settlement_profile.as_str())?;
        snapshot.set_item(spec.agent.name(), value)?;
    }
    Ok(snapshot.into_any().unbind())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_preparation_keys_partition_npx_adapters_and_exclude_commands() {
        let codex = CODEX
            .npx_preparation_key()
            .expect("Codex uses a pinned npx package");
        let claude = CLAUDE
            .npx_preparation_key()
            .expect("Claude uses a pinned npx package");

        assert_ne!(codex, claude);
        assert_eq!(codex, CODEX.npx_preparation_key().unwrap());
        assert!(KIMI.npx_preparation_key().is_none());
    }
}
