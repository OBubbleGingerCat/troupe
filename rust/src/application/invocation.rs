use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use clap::Parser;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyBytes, PyBytesMethods, PyList, PyListMethods, PyString};

#[derive(Parser)]
#[command(name = "troupe")]
struct TroupeArgs {
    #[arg(long, value_name = "PACKAGE_DIR")]
    production: PathBuf,
}

pub(crate) enum InvocationError {
    Python(PyErr),
    Clap(clap::Error),
}

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

    let troupe_end = separator.unwrap_or(tokens.len());
    let production_args = match separator {
        Some(index) => argv.get_slice(index + 1, argv.len()),
        None => PyList::empty(py),
    };

    let os = py.import("os")?;
    let mut clap_args = Vec::with_capacity(troupe_end + 1);
    clap_args.push(OsString::from("troupe"));
    for token in &tokens[..troupe_end] {
        let encoded = os
            .call_method1("fsencode", (token,))?
            .cast_into::<PyBytes>()?;
        clap_args.push(OsString::from_vec(encoded.as_bytes().to_vec()));
    }

    Ok((clap_args, production_args))
}

pub(crate) fn parse_arguments<'py>(
    py: Python<'py>,
    argv: &Bound<'py, PyList>,
) -> Result<(Bound<'py, PyString>, Bound<'py, PyList>), InvocationError> {
    let (clap_args, production_args) =
        prepare_invocation(py, argv).map_err(InvocationError::Python)?;
    let parsed = TroupeArgs::try_parse_from(clap_args).map_err(InvocationError::Clap)?;
    let os = py.import("os").map_err(InvocationError::Python)?;
    let path_bytes = PyBytes::new(py, parsed.production.as_os_str().as_bytes());
    let path = os
        .call_method1("fsdecode", (path_bytes,))
        .and_then(|value| value.cast_into::<PyString>().map_err(Into::into))
        .map_err(InvocationError::Python)?;

    Ok((path, production_args))
}

#[pyfunction(name = "_parse_invocation")]
pub fn parse_invocation<'py>(
    py: Python<'py>,
    argv: &Bound<'py, PyList>,
) -> PyResult<(Bound<'py, PyString>, Bound<'py, PyList>)> {
    parse_arguments(py, argv).map_err(|error| match error {
        InvocationError::Python(error) => error,
        InvocationError::Clap(error) => PyValueError::new_err(error.to_string()),
    })
}
