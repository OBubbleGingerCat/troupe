#![allow(dead_code)]

use std::{ffi::OsString, path::PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};

use super::{
    target::{DiagnosticTarget, ServeTarget, ServeTargetArgs, TargetArgs},
    values::{
        ArchiveAge, BindHost, ByteSize, CanonicalU64, Count, DiagnosticBaseUrl, Port, RunId,
        RuntimeDuration,
    },
};

#[derive(Clone, Debug, Parser, Eq, PartialEq)]
#[command(
    name = "troupe",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true,
    disable_help_subcommand = true
)]
pub(crate) struct TroupeArgs {
    #[arg(long, value_name = "PACKAGE_DIR", required = true)]
    production: Option<PathBuf>,

    #[command(flatten)]
    diagnostics: RuntimeDiagnosticArgs,

    #[arg(
        last = true,
        value_name = "PRODUCTION_ARGS",
        allow_hyphen_values = true
    )]
    production_args: Vec<OsString>,

    #[command(subcommand)]
    command: Option<TroupeCommand>,
}

impl TroupeArgs {
    pub(crate) fn into_invocation(self) -> TroupeInvocation {
        match self.command {
            Some(TroupeCommand::Diagnostic(arguments)) => {
                TroupeInvocation::Diagnostic(arguments.command)
            }
            None => TroupeInvocation::Production(ProductionInvocation {
                production: self
                    .production
                    .expect("clap requires --production without a subcommand"),
                diagnostics: self.diagnostics,
                production_args: self.production_args,
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TroupeInvocation {
    Production(ProductionInvocation),
    Diagnostic(DiagnosticCommand),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProductionInvocation {
    pub(crate) production: PathBuf,
    pub(crate) diagnostics: RuntimeDiagnosticArgs,
    pub(crate) production_args: Vec<OsString>,
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct RuntimeDiagnosticArgs {
    #[arg(
        long = "diagnostic-bind-host",
        value_name = "HOST",
        default_value = "0.0.0.0"
    )]
    pub(crate) bind_host: BindHost,

    #[arg(long = "diagnostic-port", value_name = "PORT", default_value = "0")]
    pub(crate) port: Port,

    #[arg(long = "diagnostic-advertise-url", value_name = "BASE_URL")]
    pub(crate) advertise_url: Option<DiagnosticBaseUrl>,

    #[arg(long = "diagnostic-max-run-bytes", value_name = "SIZE")]
    pub(crate) max_run_bytes: Option<ByteSize>,

    #[arg(
        long = "diagnostic-writer-stall-timeout",
        value_name = "DURATION",
        default_value = "10s"
    )]
    pub(crate) writer_stall_timeout: RuntimeDuration,

    #[arg(
        long = "diagnostic-shutdown-timeout",
        value_name = "DURATION",
        default_value = "30s"
    )]
    pub(crate) shutdown_timeout: RuntimeDuration,
}

#[derive(Clone, Debug, Subcommand, Eq, PartialEq)]
enum TroupeCommand {
    Diagnostic(DiagnosticArgs),
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
struct DiagnosticArgs {
    #[command(subcommand)]
    command: DiagnosticCommand,
}

#[derive(Clone, Debug, Subcommand, Eq, PartialEq)]
pub(crate) enum DiagnosticCommand {
    Runs(RunsArgs),
    Status(StatusArgs),
    Snapshot(SnapshotArgs),
    Events(EventsArgs),
    Dump(DumpArgs),
    Serve(ServeArgs),
    Cleanup(CleanupArgs),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum DocumentFormat {
    #[default]
    Human,
    Json,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum EventsFormat {
    #[default]
    Human,
    Jsonl,
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct RunsArgs {
    #[arg(long, value_name = "PROD")]
    pub(crate) production: PathBuf,

    #[arg(long, value_enum, default_value = "human")]
    pub(crate) format: DocumentFormat,
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct StatusArgs {
    #[command(flatten)]
    target: TargetArgs,

    #[arg(long, value_enum, default_value = "human")]
    pub(crate) format: DocumentFormat,
}

impl StatusArgs {
    pub(crate) fn into_parts(self) -> (DiagnosticTarget, DocumentFormat) {
        (self.target.into_target(), self.format)
    }
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct SnapshotArgs {
    #[command(flatten)]
    target: TargetArgs,

    #[arg(long, value_enum, default_value = "human")]
    pub(crate) format: DocumentFormat,
}

impl SnapshotArgs {
    pub(crate) fn into_parts(self) -> (DiagnosticTarget, DocumentFormat) {
        (self.target.into_target(), self.format)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventStart {
    Tail(Count),
    After(CanonicalU64),
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct EventsArgs {
    #[command(flatten)]
    target: TargetArgs,

    #[arg(long, value_name = "N", conflicts_with = "after")]
    tail: Option<Count>,

    #[arg(long, value_name = "SEQ")]
    after: Option<CanonicalU64>,

    #[arg(long, conflicts_with = "archive")]
    pub(crate) follow: bool,

    #[arg(long, value_enum, default_value = "human")]
    pub(crate) format: EventsFormat,
}

impl EventsArgs {
    pub(crate) fn into_parts(self) -> (DiagnosticTarget, EventStart, bool, EventsFormat) {
        let start = match (self.tail, self.after) {
            (Some(tail), None) => EventStart::Tail(tail),
            (None, Some(after)) => EventStart::After(after),
            (None, None) => EventStart::Tail(Count::new(100)),
            (Some(_), Some(_)) => unreachable!("clap rejects --tail with --after"),
        };
        (self.target.into_target(), start, self.follow, self.format)
    }
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct DumpArgs {
    #[command(flatten)]
    target: TargetArgs,

    #[arg(long, value_name = "FILE")]
    pub(crate) output: PathBuf,

    #[arg(long, value_name = "SEQ")]
    pub(crate) through: Option<CanonicalU64>,

    #[arg(long)]
    pub(crate) force: bool,
}

impl DumpArgs {
    pub(crate) fn into_parts(self) -> (DiagnosticTarget, PathBuf, Option<CanonicalU64>, bool) {
        (
            self.target.into_target(),
            self.output,
            self.through,
            self.force,
        )
    }
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct ServeArgs {
    #[command(flatten)]
    target: ServeTargetArgs,

    #[arg(long, value_name = "PORT", default_value = "0")]
    pub(crate) port: Port,

    #[arg(long)]
    pub(crate) open: bool,
}

impl ServeArgs {
    pub(crate) fn into_parts(self) -> (ServeTarget, Port, bool) {
        (self.target.into_target(), self.port, self.open)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupPolicy {
    Run(RunId),
    OlderThan(ArchiveAge),
    KeepRuns(Count),
    MaxTotalBytes(ByteSize),
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct CleanupArgs {
    #[arg(long, value_name = "PROD")]
    pub(crate) production: PathBuf,

    #[arg(
        long,
        value_name = "RUN_ID",
        required_unless_present_any = ["older_than", "keep_runs", "max_total_bytes"],
        conflicts_with_all = ["older_than", "keep_runs", "max_total_bytes"]
    )]
    run: Option<RunId>,

    #[arg(
        long,
        value_name = "DURATION",
        conflicts_with_all = ["keep_runs", "max_total_bytes"]
    )]
    older_than: Option<ArchiveAge>,

    #[arg(long, value_name = "N", conflicts_with = "max_total_bytes")]
    keep_runs: Option<Count>,

    #[arg(long, value_name = "SIZE")]
    max_total_bytes: Option<ByteSize>,

    #[arg(long)]
    pub(crate) apply: bool,

    #[arg(long, value_enum, default_value = "human")]
    pub(crate) format: DocumentFormat,
}

impl CleanupArgs {
    pub(crate) fn policy(&self) -> CleanupPolicy {
        match (
            self.run,
            self.older_than,
            self.keep_runs,
            self.max_total_bytes,
        ) {
            (Some(run), None, None, None) => CleanupPolicy::Run(run),
            (None, Some(age), None, None) => CleanupPolicy::OlderThan(age),
            (None, None, Some(count), None) => CleanupPolicy::KeepRuns(count),
            (None, None, None, Some(size)) => CleanupPolicy::MaxTotalBytes(size),
            _ => unreachable!("clap enforces exactly one cleanup policy"),
        }
    }

    pub(crate) fn into_parts(self) -> (PathBuf, CleanupPolicy, bool, DocumentFormat) {
        let policy = self.policy();
        (self.production, policy, self.apply, self.format)
    }
}
