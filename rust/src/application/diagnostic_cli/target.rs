#![allow(dead_code)]

use std::path::PathBuf;

use clap::Args;

use super::values::{DiagnosticBaseUrl, RunId};

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct TargetArgs {
    #[arg(
        long,
        value_name = "PROD",
        required_unless_present_any = ["url", "archive"],
        conflicts_with_all = ["url", "archive"]
    )]
    production: Option<PathBuf>,

    #[arg(
        long,
        value_name = "RUN_ID",
        requires = "production",
        conflicts_with_all = ["url", "archive"]
    )]
    run: Option<RunId>,

    #[arg(long, value_name = "BASE_URL", conflicts_with = "archive")]
    url: Option<DiagnosticBaseUrl>,

    #[arg(long, value_name = "RUN_DIRECTORY")]
    archive: Option<PathBuf>,
}

impl TargetArgs {
    pub(crate) fn into_target(self) -> DiagnosticTarget {
        match (self.production, self.url, self.archive) {
            (Some(production), None, None) => DiagnosticTarget::Production {
                production,
                run: self.run,
            },
            (None, Some(url), None) => DiagnosticTarget::Url(url),
            (None, None, Some(archive)) => DiagnosticTarget::Archive(archive),
            _ => unreachable!("clap enforces exactly one diagnostic target"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticTarget {
    Production {
        production: PathBuf,
        run: Option<RunId>,
    },
    Url(DiagnosticBaseUrl),
    Archive(PathBuf),
}

#[derive(Clone, Debug, Args, Eq, PartialEq)]
pub(crate) struct ServeTargetArgs {
    #[arg(long, value_name = "PROD", conflicts_with = "archive")]
    production: Option<PathBuf>,

    #[arg(
        long,
        value_name = "RUN_ID",
        requires = "production",
        required_unless_present = "archive",
        conflicts_with = "archive"
    )]
    run: Option<RunId>,

    #[arg(long, value_name = "RUN_DIRECTORY")]
    archive: Option<PathBuf>,
}

impl ServeTargetArgs {
    pub(crate) fn into_target(self) -> ServeTarget {
        match (self.production, self.run, self.archive) {
            (Some(production), Some(run), None) => ServeTarget::Production { production, run },
            (None, None, Some(archive)) => ServeTarget::Archive(archive),
            _ => unreachable!("clap enforces an explicit inactive serve target"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ServeTarget {
    Production { production: PathBuf, run: RunId },
    Archive(PathBuf),
}
