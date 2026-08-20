use std::{ffi::OsString, fs, path::PathBuf, str::FromStr, time::Duration};

use clap::{CommandFactory, Parser, error::ErrorKind};

#[path = "../src/application/diagnostic_cli/args.rs"]
mod args;
#[path = "../src/application/diagnostic_cli/target.rs"]
mod target;
#[path = "../src/application/diagnostic_cli/values.rs"]
mod values;

use args::{
    CleanupPolicy, DiagnosticCommand, DocumentFormat, EventStart, EventsFormat, TroupeArgs,
    TroupeInvocation,
};
use target::{DiagnosticTarget, ServeTarget};
use values::{
    ArchiveAge, BindHost, ByteSize, CanonicalU64, Count, DiagnosticBaseUrl, Port, RunId,
    RuntimeDuration,
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

fn parse(arguments: &[&str]) -> TroupeInvocation {
    TroupeArgs::try_parse_from(arguments)
        .unwrap_or_else(|error| panic!("parse {arguments:?}: {error}"))
        .into_invocation()
}

fn usage_error(arguments: &[&str]) -> clap::Error {
    let error = match TroupeArgs::try_parse_from(arguments) {
        Ok(_) => panic!("accepted invalid arguments {arguments:?}"),
        Err(error) => error,
    };
    assert_eq!(error.exit_code(), 2, "{arguments:?}: {error}");
    error
}

#[test]
fn frozen_command_surface_and_formats_parse_to_typed_commands() {
    TroupeArgs::command().debug_assert();

    match parse(&["troupe", "diagnostic", "runs", "--production", "prod"]) {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Runs(arguments)) => {
            assert_eq!(arguments.production, PathBuf::from("prod"));
            assert_eq!(arguments.format, DocumentFormat::Human);
        }
        invocation => panic!("unexpected invocation: {invocation:?}"),
    }

    match parse(&[
        "troupe",
        "diagnostic",
        "status",
        "--production",
        "prod",
        "--run",
        RUN_ID,
        "--format",
        "json",
    ]) {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Status(arguments)) => {
            assert_eq!(arguments.format, DocumentFormat::Json);
            let (target, format) = arguments.into_parts();
            assert_eq!(format, DocumentFormat::Json);
            assert_eq!(
                target,
                DiagnosticTarget::Production {
                    production: PathBuf::from("prod"),
                    run: Some(RunId::from_str(RUN_ID).unwrap()),
                }
            );
        }
        invocation => panic!("unexpected invocation: {invocation:?}"),
    }

    match parse(&[
        "troupe",
        "diagnostic",
        "snapshot",
        "--url",
        "https://diagnostics.example/base",
    ]) {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Snapshot(arguments)) => {
            assert_eq!(arguments.format, DocumentFormat::Human);
            let (target, format) = arguments.into_parts();
            assert_eq!(format, DocumentFormat::Human);
            let DiagnosticTarget::Url(url) = target else {
                panic!("expected URL target");
            };
            assert_eq!(url.as_str(), "https://diagnostics.example/base");
        }
        invocation => panic!("unexpected invocation: {invocation:?}"),
    }

    match parse(&[
        "troupe",
        "diagnostic",
        "events",
        "--archive",
        "copied-run",
        "--after",
        "18446744073709551615",
        "--format",
        "jsonl",
    ]) {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Events(arguments)) => {
            assert_eq!(arguments.format, EventsFormat::Jsonl);
            assert!(!arguments.follow);
            let (target, start, follow, format) = arguments.into_parts();
            assert_eq!(
                target,
                DiagnosticTarget::Archive(PathBuf::from("copied-run"))
            );
            assert_eq!(start, EventStart::After(CanonicalU64::new(u64::MAX)));
            assert!(!follow);
            assert_eq!(format, EventsFormat::Jsonl);
        }
        invocation => panic!("unexpected invocation: {invocation:?}"),
    }

    match parse(&[
        "troupe",
        "diagnostic",
        "dump",
        "--url",
        "http://127.0.0.1:43120/",
        "--output",
        "run.pftrace",
        "--through",
        "0",
        "--force",
    ]) {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Dump(arguments)) => {
            assert_eq!(arguments.output, PathBuf::from("run.pftrace"));
            assert_eq!(arguments.through.unwrap().get(), 0);
            assert!(arguments.force);
            let (target, output, through, force) = arguments.into_parts();
            assert!(matches!(target, DiagnosticTarget::Url(_)));
            assert_eq!(output, PathBuf::from("run.pftrace"));
            assert_eq!(through.unwrap().get(), 0);
            assert!(force);
        }
        invocation => panic!("unexpected invocation: {invocation:?}"),
    }
}

#[test]
fn events_default_tail_and_explicit_zero_are_distinct_and_closed() {
    for (extra, expected) in [
        (&[][..], EventStart::Tail(Count::new(100))),
        (&["--tail", "000"][..], EventStart::Tail(Count::new(0))),
        (
            &["--after", "0"][..],
            EventStart::After(CanonicalU64::new(0)),
        ),
    ] {
        let mut arguments = vec!["troupe", "diagnostic", "events", "--url", "http://host/"];
        arguments.extend_from_slice(extra);
        let TroupeInvocation::Diagnostic(DiagnosticCommand::Events(arguments)) = parse(&arguments)
        else {
            panic!("expected events command");
        };
        assert_eq!(arguments.into_parts().1, expected);
    }

    usage_error(&[
        "troupe",
        "diagnostic",
        "events",
        "--url",
        "http://host/",
        "--tail",
        "1",
        "--after",
        "2",
    ]);
    usage_error(&[
        "troupe",
        "diagnostic",
        "events",
        "--archive",
        "run",
        "--follow",
    ]);
    usage_error(&[
        "troupe",
        "diagnostic",
        "events",
        "--url",
        "http://host/",
        "--format",
        "json",
    ]);
}

#[test]
fn query_targets_require_exactly_one_selector_and_run_requires_production() {
    for arguments in [
        vec!["troupe", "diagnostic", "status"],
        vec![
            "troupe",
            "diagnostic",
            "status",
            "--production",
            "prod",
            "--url",
            "http://host/",
        ],
        vec![
            "troupe",
            "diagnostic",
            "status",
            "--url",
            "http://host/",
            "--archive",
            "run",
        ],
        vec!["troupe", "diagnostic", "status", "--run", RUN_ID],
    ] {
        usage_error(&arguments);
    }

    let TroupeInvocation::Diagnostic(DiagnosticCommand::Status(arguments)) = parse(&[
        "troupe",
        "diagnostic",
        "status",
        "--production",
        "missing-production-root",
    ]) else {
        panic!("expected status command");
    };
    assert_eq!(
        arguments.into_parts().0,
        DiagnosticTarget::Production {
            production: PathBuf::from("missing-production-root"),
            run: None,
        }
    );
}

#[test]
fn serve_requires_an_explicit_inactive_target_and_valid_port() {
    match parse(&[
        "troupe",
        "diagnostic",
        "serve",
        "--production",
        "prod",
        "--run",
        RUN_ID,
        "--port",
        "65535",
        "--open",
    ]) {
        TroupeInvocation::Diagnostic(DiagnosticCommand::Serve(arguments)) => {
            assert_eq!(arguments.port.get(), 65535);
            assert!(arguments.open);
            let (target, port, open) = arguments.into_parts();
            assert_eq!(port.get(), 65535);
            assert!(open);
            assert_eq!(
                target,
                ServeTarget::Production {
                    production: PathBuf::from("prod"),
                    run: RunId::from_str(RUN_ID).unwrap(),
                }
            );
        }
        invocation => panic!("unexpected invocation: {invocation:?}"),
    }

    let TroupeInvocation::Diagnostic(DiagnosticCommand::Serve(arguments)) =
        parse(&["troupe", "diagnostic", "serve", "--archive", "copied-run"])
    else {
        panic!("expected serve command");
    };
    assert_eq!(arguments.port.get(), 0);
    let (target, port, open) = arguments.into_parts();
    assert_eq!(port.get(), 0);
    assert!(!open);
    assert_eq!(target, ServeTarget::Archive(PathBuf::from("copied-run")));

    for invalid in [
        vec!["troupe", "diagnostic", "serve"],
        vec!["troupe", "diagnostic", "serve", "--production", "prod"],
        vec!["troupe", "diagnostic", "serve", "--url", "http://host/"],
        vec![
            "troupe",
            "diagnostic",
            "serve",
            "--archive",
            "run",
            "--run",
            RUN_ID,
        ],
        vec![
            "troupe",
            "diagnostic",
            "serve",
            "--archive",
            "run",
            "--port",
            "65536",
        ],
    ] {
        usage_error(&invalid);
    }
}

#[test]
fn cleanup_requires_production_and_exactly_one_policy() {
    let cases = [
        (
            vec![
                "troupe",
                "diagnostic",
                "cleanup",
                "--production",
                "prod",
                "--run",
                RUN_ID,
            ],
            CleanupPolicy::Run(RunId::from_str(RUN_ID).unwrap()),
        ),
        (
            vec![
                "troupe",
                "diagnostic",
                "cleanup",
                "--production",
                "prod",
                "--older-than",
                "2w",
            ],
            CleanupPolicy::OlderThan(ArchiveAge::from_str("2w").unwrap()),
        ),
        (
            vec![
                "troupe",
                "diagnostic",
                "cleanup",
                "--production",
                "prod",
                "--keep-runs",
                "000",
            ],
            CleanupPolicy::KeepRuns(Count::new(0)),
        ),
        (
            vec![
                "troupe",
                "diagnostic",
                "cleanup",
                "--production",
                "prod",
                "--max-total-bytes",
                "10GiB",
                "--apply",
                "--format",
                "json",
            ],
            CleanupPolicy::MaxTotalBytes(ByteSize::from_str("10GiB").unwrap()),
        ),
    ];
    for (arguments, expected) in cases {
        let TroupeInvocation::Diagnostic(DiagnosticCommand::Cleanup(arguments)) = parse(&arguments)
        else {
            panic!("expected cleanup command");
        };
        assert_eq!(arguments.production, PathBuf::from("prod"));
        assert_eq!(arguments.policy(), expected);
        assert_eq!(arguments.into_parts().1, expected);
    }

    for invalid in [
        vec!["troupe", "diagnostic", "cleanup", "--production", "prod"],
        vec![
            "troupe",
            "diagnostic",
            "cleanup",
            "--production",
            "prod",
            "--run",
            RUN_ID,
            "--keep-runs",
            "1",
        ],
        vec!["troupe", "diagnostic", "cleanup", "--run", RUN_ID],
        vec![
            "troupe",
            "diagnostic",
            "cleanup",
            "--archive",
            "run",
            "--keep-runs",
            "1",
        ],
    ] {
        usage_error(&invalid);
    }
}

#[test]
fn canonical_value_grammars_cover_equal_and_one_over_boundaries() {
    assert_eq!(RunId::from_str(RUN_ID).unwrap().get().to_string(), RUN_ID);
    for invalid in [
        "12345678-1234-4234-9234-123456789ABC",
        "{12345678-1234-4234-9234-123456789abc}",
        "12345678123442349234123456789abc",
        "not-a-uuid",
    ] {
        assert!(RunId::from_str(invalid).is_err(), "accepted {invalid}");
    }

    assert_eq!(CanonicalU64::from_str("0").unwrap().get(), 0);
    assert_eq!(
        CanonicalU64::from_str("18446744073709551615")
            .unwrap()
            .get(),
        u64::MAX
    );
    for invalid in ["", "00", "01", "+1", "-1", "18446744073709551616"] {
        assert!(
            CanonicalU64::from_str(invalid).is_err(),
            "accepted {invalid}"
        );
    }

    assert_eq!(Count::from_str("00").unwrap().get(), 0);
    assert_eq!(Count::from_str("00042").unwrap().get(), 42);
    for invalid in ["", "+1", "-1", "18446744073709551616"] {
        assert!(Count::from_str(invalid).is_err(), "accepted {invalid}");
    }

    assert_eq!(Port::from_str("0").unwrap().get(), 0);
    assert_eq!(Port::from_str("00001").unwrap().get(), 1);
    assert_eq!(Port::from_str("65535").unwrap().get(), 65535);
    for invalid in ["-1", "65536"] {
        assert!(Port::from_str(invalid).is_err(), "accepted port {invalid}");
    }

    for (value, bytes) in [
        ("1", 1),
        ("1KiB", 1_u64 << 10),
        ("2MiB", 2_u64 << 20),
        ("3GiB", 3_u64 << 30),
        ("4TiB", 4_u64 << 40),
    ] {
        assert_eq!(ByteSize::from_str(value).unwrap().bytes(), bytes);
    }
    assert!(ByteSize::from_str("16777215TiB").is_ok());
    for invalid in ["0", "0KiB", "01KiB", "1KB", "1kiB", "1.5GiB", "16777216TiB"] {
        assert!(ByteSize::from_str(invalid).is_err(), "accepted {invalid}");
    }

    for (value, duration) in [
        ("1ms", Duration::from_millis(1)),
        ("0001s", Duration::from_secs(1)),
        ("2s", Duration::from_secs(2)),
        ("3m", Duration::from_secs(180)),
        ("4h", Duration::from_secs(14_400)),
    ] {
        assert_eq!(RuntimeDuration::from_str(value).unwrap().get(), duration);
    }
    for invalid in ["0ms", "1", "1d", "1S", "1.5s", "18446744073709551615h"] {
        assert!(
            RuntimeDuration::from_str(invalid).is_err(),
            "accepted {invalid}"
        );
    }

    for (value, duration) in [
        ("1h", Duration::from_secs(3_600)),
        ("0001d", Duration::from_secs(86_400)),
        ("2d", Duration::from_secs(172_800)),
        ("3w", Duration::from_secs(1_814_400)),
    ] {
        assert_eq!(ArchiveAge::from_str(value).unwrap().get(), duration);
    }
    for invalid in ["0h", "1ms", "1m", "1W"] {
        assert!(ArchiveAge::from_str(invalid).is_err(), "accepted {invalid}");
    }
}

#[test]
fn url_and_bind_host_validation_reuse_runtime_identity_rules() {
    assert_eq!(
        DiagnosticBaseUrl::from_str("http://diagnostics.example")
            .unwrap()
            .as_str(),
        "http://diagnostics.example/"
    );
    assert_eq!(
        DiagnosticBaseUrl::from_str("https://diagnostics.example/base")
            .unwrap()
            .into_inner()
            .as_str(),
        "https://diagnostics.example/base"
    );
    for invalid in [
        "ftp://diagnostics.example/",
        "http://user@diagnostics.example/",
        "http://diagnostics.example/path?query=1",
        "http://diagnostics.example/path#fragment",
        "//diagnostics.example/path",
        "http:///missing-host",
        "http://diagnostics.example/a/../b",
    ] {
        assert!(
            DiagnosticBaseUrl::from_str(invalid).is_err(),
            "accepted {invalid}"
        );
    }

    assert_eq!(BindHost::from_str("0.0.0.0").unwrap().as_str(), "0.0.0.0");
    assert_eq!(BindHost::from_str("[::1]").unwrap().as_str(), "::1");
    assert_eq!(
        BindHost::from_str("troupe-host.local").unwrap().as_str(),
        "troupe-host.local"
    );
    for invalid in ["", "host name", "http://host", "user@host", "host/path"] {
        assert!(BindHost::from_str(invalid).is_err(), "accepted {invalid}");
    }
}

#[test]
fn runtime_flags_are_closed_and_only_parsed_before_the_separator() {
    let invocation = parse(&[
        "troupe",
        "--production",
        "prod",
        "--diagnostic-bind-host",
        "127.0.0.1",
        "--diagnostic-port",
        "43120",
        "--diagnostic-advertise-url",
        "https://host.example/troupe/",
        "--diagnostic-max-run-bytes",
        "10GiB",
        "--diagnostic-writer-stall-timeout",
        "250ms",
        "--diagnostic-shutdown-timeout",
        "2m",
        "--",
        "--diagnostic-port",
        "7",
        "production-command",
    ]);
    let TroupeInvocation::Production(arguments) = invocation else {
        panic!("expected Production invocation");
    };
    assert_eq!(arguments.production, PathBuf::from("prod"));
    assert_eq!(arguments.diagnostics.bind_host.as_str(), "127.0.0.1");
    assert_eq!(arguments.diagnostics.port.get(), 43120);
    assert_eq!(
        arguments
            .diagnostics
            .advertise_url
            .as_ref()
            .unwrap()
            .as_str(),
        "https://host.example/troupe/"
    );
    assert_eq!(
        arguments.diagnostics.max_run_bytes.unwrap().bytes(),
        10_u64 << 30
    );
    assert_eq!(
        arguments.diagnostics.writer_stall_timeout.get(),
        Duration::from_millis(250)
    );
    assert_eq!(
        arguments.diagnostics.shutdown_timeout.get(),
        Duration::from_secs(120)
    );
    assert_eq!(
        arguments.production_args,
        [
            OsString::from("--diagnostic-port"),
            OsString::from("7"),
            OsString::from("production-command"),
        ]
    );

    let TroupeInvocation::Production(defaults) = parse(&["troupe", "--production", "prod"]) else {
        panic!("expected Production invocation");
    };
    assert_eq!(defaults.diagnostics.bind_host.as_str(), "0.0.0.0");
    assert_eq!(defaults.diagnostics.port.get(), 0);
    assert!(defaults.diagnostics.advertise_url.is_none());
    assert!(defaults.diagnostics.max_run_bytes.is_none());
    assert_eq!(
        defaults.diagnostics.writer_stall_timeout.get(),
        Duration::from_secs(10)
    );
    assert_eq!(
        defaults.diagnostics.shutdown_timeout.get(),
        Duration::from_secs(30)
    );

    for forbidden in [
        "--diagnostic-disable",
        "--diagnostic-root",
        "--diagnostic-auth",
        "--diagnostic-queue-size",
        "--diagnostic-batch-size",
        "--diagnostic-retention",
    ] {
        usage_error(&["troupe", "--production", "prod", forbidden]);
    }
}

#[test]
fn help_is_zero_and_all_usage_failures_are_code_two() {
    let help = TroupeArgs::try_parse_from(["troupe", "diagnostic", "--help"]).unwrap_err();
    assert_eq!(help.kind(), ErrorKind::DisplayHelp);
    assert_eq!(help.exit_code(), 0);
    let rendered = help.to_string();
    for command in [
        "runs", "status", "snapshot", "events", "dump", "serve", "cleanup",
    ] {
        assert!(rendered.contains(command), "help omitted {command}");
    }

    for invalid in [
        vec!["troupe"],
        vec!["troupe", "diagnostic"],
        vec!["troupe", "diagnostic", "unknown"],
        vec!["troupe", "diagnostic", "status", "--url", "ftp://host/"],
        vec![
            "troupe",
            "diagnostic",
            "status",
            "--production",
            "prod",
            "--run",
            "12345678-1234-4234-9234-123456789ABC",
        ],
        vec!["troupe", "diagnostic", "dump", "--archive", "run"],
    ] {
        usage_error(&invalid);
    }
}

#[test]
fn diagnostic_parse_is_pure_and_never_loads_a_production() {
    let root = std::env::temp_dir().join(format!("troupe-d00-no-loader-{}", std::process::id()));
    let package = root.join("explosive_production");
    let marker = root.join("loader-ran");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("__init__.py"),
        format!(
            "from pathlib import Path\nPath({:?}).write_text('loaded')\nraise RuntimeError('must not load')\n",
            marker
        ),
    )
    .unwrap();

    let package_text = package.to_str().unwrap();
    assert!(matches!(
        parse(&["troupe", "diagnostic", "runs", "--production", package_text,]),
        TroupeInvocation::Diagnostic(DiagnosticCommand::Runs(_))
    ));
    assert!(matches!(
        parse(&[
            "troupe",
            "diagnostic",
            "status",
            "--production",
            package_text,
        ]),
        TroupeInvocation::Diagnostic(DiagnosticCommand::Status(_))
    ));
    assert!(
        !marker.exists(),
        "diagnostic parsing executed Production code"
    );

    let missing_archive = root.join("missing-archive");
    let missing_text = missing_archive.to_str().unwrap();
    assert!(matches!(
        parse(&["troupe", "diagnostic", "status", "--archive", missing_text,]),
        TroupeInvocation::Diagnostic(DiagnosticCommand::Status(_))
    ));
    assert!(
        !missing_archive.exists(),
        "parsing resolved or created an archive"
    );
    fs::remove_dir_all(root).unwrap();
}
