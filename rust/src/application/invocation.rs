use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
};

use clap::{CommandFactory, FromArgMatches};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyBytes, PyBytesMethods, PyList, PyListMethods, PyString};

use crate::application::diagnostic_cli::{
    DiagnosticCommand, RuntimeDiagnosticArgs, TroupeInvocation, args::TroupeArgs,
};

pub(crate) const PRODUCTION_HELP: &str = "Run a Production:\n  troupe --production <PACKAGE_DIR> [RUNTIME_OPTIONS] [-- <PRODUCTION_ARGS>...]\n\nProduction arguments begin only after the first exact `--`; no `troupe run` subcommand is required.";

pub(crate) enum InvocationError {
    Python(PyErr),
    Clap(clap::Error),
}

pub(crate) enum ParsedInvocation<'py> {
    Production {
        path: Bound<'py, PyString>,
        diagnostics: RuntimeDiagnosticArgs,
        production_args: Bound<'py, PyList>,
    },
    Diagnostic(DiagnosticCommand),
}

pub(crate) type ProductionInvocation<'py> = (Bound<'py, PyString>, Bound<'py, PyList>);

fn prepare_invocation<'py>(
    py: Python<'py>,
    argv: &Bound<'py, PyList>,
) -> PyResult<(Vec<OsString>, Bound<'py, PyList>)> {
    let mut tokens = Vec::with_capacity(argv.len());
    for item in argv.iter() {
        tokens.push(item.cast_into::<PyString>()?);
    }

    let mut separator = None;
    for (index, token) in tokens.iter().enumerate() {
        if token.eq("--") {
            separator = Some(index);
            break;
        }
    }

    let production_args = match separator {
        Some(index) => argv.get_slice(index + 1, argv.len()),
        None => PyList::empty(py),
    };

    let os = py.import("os")?;
    let mut clap_args = Vec::with_capacity(tokens.len() + 1);
    clap_args.push(OsString::from("troupe"));
    for token in &tokens {
        let encoded = os
            .call_method1("fsencode", (token,))?
            .cast_into::<PyBytes>()?;
        clap_args.push(OsString::from_vec(encoded.as_bytes().to_vec()));
    }

    Ok((clap_args, production_args))
}

pub(crate) fn troupe_command() -> clap::Command {
    TroupeArgs::command()
        .version(env!("CARGO_PKG_VERSION"))
        .about("Run a Production or inspect its diagnostics")
        .mut_args(|argument| {
            let help = match argument.get_long() {
                Some("production") => Some("Python package directory containing the Production"),
                Some("diagnostic-bind-host") => Some("Diagnostic server bind host"),
                Some("diagnostic-port") => {
                    Some("Diagnostic server bind port; zero selects an available port")
                }
                Some("diagnostic-advertise-url") => {
                    Some("Externally advertised diagnostic base URL")
                }
                Some("diagnostic-max-run-bytes") => {
                    Some("Maximum retained bytes for one diagnostic Run")
                }
                Some("diagnostic-writer-stall-timeout") => {
                    Some("Maximum diagnostic writer progress stall")
                }
                Some("diagnostic-shutdown-timeout") => {
                    Some("Maximum diagnostic shutdown drain time")
                }
                _ if argument.get_id().as_str() == "production_args" => {
                    Some("Arguments passed unchanged to the Production after `--`")
                }
                _ => None,
            };
            match help {
                Some(help) => argument.help(help),
                None => argument,
            }
        })
        .mut_subcommand("diagnostic", |command| {
            command
                .about("Inspect active and archived Production diagnostics")
                .mut_subcommand("runs", |command| {
                    command.about("List active, stale, and archived Runs")
                })
                .mut_subcommand("status", |command| {
                    command.about("Show diagnostic and Production status")
                })
                .mut_subcommand("snapshot", |command| {
                    command.about("Read the current diagnostic snapshot")
                })
                .mut_subcommand("events", |command| {
                    command.about("Read or follow canonical diagnostic events")
                })
                .mut_subcommand("dump", |command| {
                    command.about("Export a captured prefix as a Perfetto trace")
                })
                .mut_subcommand("serve", |command| {
                    command.about("Serve an inactive archive on loopback")
                })
                .mut_subcommand("cleanup", |command| {
                    command.about("Preview or apply archive retention cleanup")
                })
        })
        .after_help(PRODUCTION_HELP)
}

pub(crate) fn parse_encoded_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<TroupeInvocation, clap::Error> {
    let matches = troupe_command().try_get_matches_from(arguments)?;
    TroupeArgs::from_arg_matches(&matches).map(TroupeArgs::into_invocation)
}

pub(crate) fn parse_arguments<'py>(
    py: Python<'py>,
    argv: &Bound<'py, PyList>,
) -> Result<ParsedInvocation<'py>, InvocationError> {
    let (clap_args, production_args) =
        prepare_invocation(py, argv).map_err(InvocationError::Python)?;
    match parse_encoded_arguments(clap_args).map_err(InvocationError::Clap)? {
        TroupeInvocation::Production(parsed) => {
            let os = py.import("os").map_err(InvocationError::Python)?;
            let path_bytes = PyBytes::new(py, parsed.production.as_os_str().as_bytes());
            let path = os
                .call_method1("fsdecode", (path_bytes,))
                .and_then(|value| value.cast_into::<PyString>().map_err(Into::into))
                .map_err(InvocationError::Python)?;
            Ok(ParsedInvocation::Production {
                path,
                diagnostics: parsed.diagnostics,
                production_args,
            })
        }
        TroupeInvocation::Diagnostic(command) => Ok(ParsedInvocation::Diagnostic(command)),
    }
}

#[pyfunction(name = "_parse_invocation")]
pub fn parse_invocation<'py>(
    py: Python<'py>,
    argv: &Bound<'py, PyList>,
) -> PyResult<ProductionInvocation<'py>> {
    parse_arguments(py, argv)
        .map_err(|error| match error {
            InvocationError::Python(error) => error,
            InvocationError::Clap(error) => PyValueError::new_err(error.to_string()),
        })
        .and_then(|invocation| match invocation {
            ParsedInvocation::Production {
                path,
                production_args,
                ..
            } => Ok((path, production_args)),
            ParsedInvocation::Diagnostic(_) => Err(PyValueError::new_err(
                "diagnostic command is not a Production invocation",
            )),
        })
}
