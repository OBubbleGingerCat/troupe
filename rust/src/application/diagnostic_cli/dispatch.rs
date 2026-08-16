use std::{fmt, future::Future};

use tokio_util::sync::CancellationToken;

use super::{
    args::DiagnosticCommand, cleanup_apply, cleanup_policy, dump, events_finite, events_follow,
    runs, serve, snapshot, status,
};

const COMMAND_FAILURE_PREFIX: &str = "troupe: diagnostic command failed: ";

pub(crate) trait DiagnosticOutput {
    type Error: fmt::Display;

    fn write_stdout(&mut self, text: &str) -> Result<(), Self::Error>;
    fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error>;
}

impl<Output> events_follow::FollowOutput for Output
where
    Output: DiagnosticOutput,
{
    type Error = Output::Error;

    fn write_stdout_record(&mut self, record: &str) -> Result<(), Self::Error> {
        self.write_stdout(record)
    }

    fn write_stderr_line(&mut self, line: &str) -> Result<(), Self::Error> {
        if line.ends_with('\n') {
            self.write_stderr(line)
        } else {
            self.write_stderr(&format!("{line}\n"))
        }
    }
}

impl<Output> serve::ServeOutput for Output
where
    Output: DiagnosticOutput,
{
    type Error = Output::Error;

    fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error> {
        DiagnosticOutput::write_stderr(self, text)
    }
}

impl<Output> dump::DumpOutput for Output
where
    Output: DiagnosticOutput,
{
    type Error = Output::Error;

    fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error> {
        DiagnosticOutput::write_stderr(self, text)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticTermination {
    Success,
    Interrupted,
}

impl DiagnosticTermination {
    pub(crate) const fn exit_code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Interrupted => 130,
        }
    }

    fn from_command_exit_code(exit_code: u8) -> Self {
        match exit_code {
            0 => Self::Success,
            130 => Self::Interrupted,
            _ => unreachable!("command termination must be success or interruption"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticDispatchError {
    code: &'static str,
    detail: String,
}

impl DiagnosticDispatchError {
    fn command(code: &'static str, error: impl fmt::Display) -> Self {
        Self {
            code,
            detail: error.to_string(),
        }
    }

    fn output(error: impl fmt::Display) -> Self {
        Self::command("diagnostic_cli.output", error)
    }

    fn unsatisfied(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    #[cfg(test)]
    const fn code(&self) -> &'static str {
        self.code
    }

    pub(crate) fn line(&self) -> String {
        format!("{COMMAND_FAILURE_PREFIX}{self}\n")
    }
}

impl fmt::Display for DiagnosticDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for DiagnosticDispatchError {}

async fn interruptible<Output, Error>(
    cancellation: &CancellationToken,
    future: impl Future<Output = Result<Output, Error>>,
) -> Result<Option<Output>, Error> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Ok(None),
        result = future => result.map(Some),
    }
}

fn write_stdout<Output>(output: &mut Output, text: &str) -> Result<(), DiagnosticDispatchError>
where
    Output: DiagnosticOutput,
{
    output
        .write_stdout(text)
        .map_err(DiagnosticDispatchError::output)
}

pub(crate) async fn execute<Output>(
    command: DiagnosticCommand,
    output: &mut Output,
    cancellation: CancellationToken,
) -> Result<DiagnosticTermination, DiagnosticDispatchError>
where
    Output: DiagnosticOutput,
{
    match command {
        DiagnosticCommand::Runs(arguments) => {
            let Some(document) = interruptible(&cancellation, runs::execute(arguments))
                .await
                .map_err(|error| DiagnosticDispatchError::command(error.code().as_str(), error))?
            else {
                return Ok(DiagnosticTermination::Interrupted);
            };
            write_stdout(output, &document)?;
            Ok(DiagnosticTermination::Success)
        }
        DiagnosticCommand::Status(arguments) => {
            let Some(document) = interruptible(&cancellation, status::execute(arguments))
                .await
                .map_err(|error| DiagnosticDispatchError::command(error.code().as_str(), error))?
            else {
                return Ok(DiagnosticTermination::Interrupted);
            };
            write_stdout(output, &document)?;
            Ok(DiagnosticTermination::Success)
        }
        DiagnosticCommand::Snapshot(arguments) => {
            let Some(document) = interruptible(&cancellation, snapshot::execute(arguments))
                .await
                .map_err(|error| DiagnosticDispatchError::command(error.code().as_str(), error))?
            else {
                return Ok(DiagnosticTermination::Interrupted);
            };
            write_stdout(output, &document)?;
            Ok(DiagnosticTermination::Success)
        }
        DiagnosticCommand::Events(arguments) if arguments.follow => {
            events_follow::execute(arguments, output, cancellation)
                .await
                .map(|termination| {
                    DiagnosticTermination::from_command_exit_code(termination.exit_code())
                })
                .map_err(|error| DiagnosticDispatchError::command(error.code().as_str(), error))
        }
        DiagnosticCommand::Events(arguments) => {
            let Some(document) = interruptible(&cancellation, events_finite::execute(arguments))
                .await
                .map_err(|error| DiagnosticDispatchError::command(error.code().as_str(), error))?
            else {
                return Ok(DiagnosticTermination::Interrupted);
            };
            write_stdout(output, &document)?;
            Ok(DiagnosticTermination::Success)
        }
        DiagnosticCommand::Dump(arguments) => dump::execute(arguments, output, cancellation)
            .await
            .map(|termination| {
                DiagnosticTermination::from_command_exit_code(termination.exit_code())
            })
            .map_err(|error| DiagnosticDispatchError::command(error.code().as_str(), error)),
        DiagnosticCommand::Serve(arguments) => serve::execute(arguments, output, cancellation)
            .await
            .map(|termination| {
                DiagnosticTermination::from_command_exit_code(termination.exit_code())
            })
            .map_err(|error| DiagnosticDispatchError::command(error.code().as_str(), error)),
        DiagnosticCommand::Cleanup(arguments) => {
            let (production, policy, apply, format) = arguments.into_parts();
            if apply {
                // Deletion owns a durable multi-step state machine and must settle before SIGINT
                // can return control to the caller.
                let report = cleanup_apply::apply(production, policy)
                    .await
                    .map_err(|error| {
                        DiagnosticDispatchError::command(error.code().as_str(), error)
                    })?;
                write_stdout(output, &report.render(format))?;
                if cancellation.is_cancelled() {
                    return Ok(DiagnosticTermination::Interrupted);
                }
                if report.satisfied() {
                    Ok(DiagnosticTermination::Success)
                } else {
                    let detail = report.operation_failure().map_or(
                        "cleanup apply did not satisfy the selected policy",
                        |failure| failure.as_str(),
                    );
                    Err(DiagnosticDispatchError::unsatisfied(
                        "diagnostic_cleanup_apply.unsatisfied",
                        detail,
                    ))
                }
            } else {
                let Some(preview) =
                    interruptible(&cancellation, cleanup_policy::preview(production, policy))
                        .await
                        .map_err(|error| {
                            DiagnosticDispatchError::command(error.code().as_str(), error)
                        })?
                else {
                    return Ok(DiagnosticTermination::Interrupted);
                };
                write_stdout(output, &preview.render(format))?;
                if preview.satisfied() {
                    Ok(DiagnosticTermination::Success)
                } else {
                    let detail = preview.operation_failure().map_or(
                        "cleanup preview cannot satisfy the selected policy",
                        |failure| failure.as_str(),
                    );
                    Err(DiagnosticDispatchError::unsatisfied(
                        "diagnostic_cleanup.unsatisfied",
                        detail,
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, ffi::OsString};

    use clap::error::ErrorKind;
    use pyo3::prelude::*;
    use pyo3::types::PyList;

    use super::*;
    use crate::application::{
        diagnostic_cli::TroupeInvocation,
        invocation::{PRODUCTION_HELP, parse_encoded_arguments},
    };

    const TOP_HELP: &str = include_str!("../../../../tests/fixtures/diagnostics/cli/help.txt");
    const DIAGNOSTIC_HELP: &str =
        include_str!("../../../../tests/fixtures/diagnostics/cli/help-diagnostic.txt");
    const RUN_HELP: &str = include_str!("../../../../tests/fixtures/diagnostics/cli/help-run.txt");

    #[derive(Default)]
    struct MemoryOutput {
        stdout: String,
        stderr: String,
    }

    impl DiagnosticOutput for MemoryOutput {
        type Error = Infallible;

        fn write_stdout(&mut self, text: &str) -> Result<(), Self::Error> {
            self.stdout.push_str(text);
            Ok(())
        }

        fn write_stderr(&mut self, text: &str) -> Result<(), Self::Error> {
            self.stderr.push_str(text);
            Ok(())
        }
    }

    fn parse(arguments: &[&str]) -> Result<TroupeInvocation, clap::Error> {
        parse_encoded_arguments(arguments.iter().map(OsString::from))
    }

    fn help(arguments: &[&str]) -> String {
        let error = parse(arguments).expect_err("help exits before dispatch");
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert_eq!(error.exit_code(), 0);
        error.to_string()
    }

    #[test]
    fn frozen_help_version_and_no_run_subcommand_surface() {
        assert_eq!(help(&["troupe", "--help"]), TOP_HELP);
        assert_eq!(help(&["troupe", "diagnostic", "--help"]), DIAGNOSTIC_HELP);
        assert_eq!(format!("{PRODUCTION_HELP}\n"), RUN_HELP);

        let version = parse(&["troupe", "--version"]).unwrap_err();
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);
        assert_eq!(
            version.to_string(),
            format!("troupe {}\n", env!("CARGO_PKG_VERSION"))
        );
        let run = parse(&["troupe", "run", "--help"]).unwrap_err();
        assert_eq!(run.exit_code(), 2);
    }

    #[tokio::test]
    async fn command_error_is_operation_one_material_and_never_loads_production() {
        let TroupeInvocation::Diagnostic(command) = parse(&[
            "troupe",
            "diagnostic",
            "runs",
            "--production",
            "/troupe-d07-definitely-missing",
        ])
        .unwrap() else {
            panic!("expected diagnostic command");
        };
        let mut output = MemoryOutput::default();
        let error = execute(command, &mut output, CancellationToken::new())
            .await
            .unwrap_err();

        assert_eq!(error.code(), "diagnostic_runs.invalid_production_root");
        assert!(error.line().starts_with(COMMAND_FAILURE_PREFIX));
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn top_level_diagnostic_error_is_exit_one_without_loading_production() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| -> PyResult<()> {
            let sys = py.import("sys")?;
            let io = py.import("io")?;
            let original_argv = sys.getattr("argv")?.unbind();
            let original_stdout = sys.getattr("stdout")?.unbind();
            let original_stderr = sys.getattr("stderr")?.unbind();
            let stdout = io.call_method0("StringIO")?;
            let stderr = io.call_method0("StringIO")?;
            let missing = format!("/troupe-d07-definitely-missing-{}", std::process::id());
            let argv = PyList::new(
                py,
                [
                    "troupe",
                    "diagnostic",
                    "runs",
                    "--production",
                    missing.as_str(),
                ],
            )?;

            let result = (|| -> PyResult<(i32, String, String)> {
                sys.setattr("argv", &argv)?;
                sys.setattr("stdout", &stdout)?;
                sys.setattr("stderr", &stderr)?;
                let exit_code = crate::application::cli::main(py)?;
                let stdout = stdout.call_method0("getvalue")?.extract()?;
                let stderr = stderr.call_method0("getvalue")?.extract()?;
                Ok((exit_code, stdout, stderr))
            })();

            sys.setattr("argv", original_argv.bind(py))?;
            sys.setattr("stdout", original_stdout.bind(py))?;
            sys.setattr("stderr", original_stderr.bind(py))?;

            let (exit_code, stdout, stderr) = result?;
            assert_eq!(exit_code, 1);
            assert!(stdout.is_empty());
            assert!(stderr.starts_with(COMMAND_FAILURE_PREFIX));
            assert!(stderr.contains("diagnostic_runs.invalid_production_root"));
            assert!(!stderr.contains("failed to load production"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn streaming_adapters_keep_facts_on_stdout_and_controls_on_stderr() {
        let mut output = MemoryOutput::default();
        events_follow::FollowOutput::write_stdout_record(&mut output, "{\"event\":1}\n").unwrap();
        events_follow::FollowOutput::write_stderr_line(&mut output, "reconnecting").unwrap();
        serve::ServeOutput::write_stderr(&mut output, "ready\n").unwrap();
        dump::DumpOutput::write_stderr(&mut output, "published\n").unwrap();

        assert_eq!(output.stdout, "{\"event\":1}\n");
        assert_eq!(output.stderr, "reconnecting\nready\npublished\n");
        assert_eq!(DiagnosticTermination::Success.exit_code(), 0);
        assert_eq!(DiagnosticTermination::Interrupted.exit_code(), 130);
    }
}
