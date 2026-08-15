use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::registry::{
    codec::{RegistryCodecErrorCode, decode_registry_entry, encode_registry_entry},
    model::{
        BindEndpoint, REGISTRY_SCHEMA_VERSION, RegistryEntry, RegistryModelErrorCode,
        SERVER_PROTOCOL_VERSION, SecurityScope, WebBaseUrl,
    },
    process_identity::{
        ObservedProcessIdentity, ProcessIdentity, ProcessIdentityClassification,
        classify_process_identity, current_process_identity, observe_process_identity,
    },
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

fn run_id() -> CanonicalUuid {
    CanonicalUuid::parse(RUN_ID).unwrap()
}

fn process_identity() -> ProcessIdentity {
    ProcessIdentity::new("test", "boot-a:4242").unwrap()
}

fn run_directory() -> PathBuf {
    PathBuf::from(format!("/srv/production/.troupe/diagnostics/runs/{RUN_ID}"))
}

fn entry(bind_host: &str, advertise_url: Option<&str>) -> RegistryEntry {
    RegistryEntry::new(
        run_id(),
        &run_directory(),
        8123,
        process_identity(),
        BindEndpoint::new(bind_host, 43120).unwrap(),
        advertise_url.map(WebBaseUrl::parse).transpose().unwrap(),
        "2026-08-14T09:30:00.123456789Z",
    )
    .unwrap()
}

#[test]
fn locator_round_trips_with_frozen_run_store_process_and_protocol_identity() {
    let expected = entry("127.0.0.1", None);
    let encoded = encode_registry_entry(&expected).unwrap();
    let decoded = decode_registry_entry(Path::new("/registry/entry.json"), &encoded).unwrap();

    assert_eq!(decoded, expected);
    assert_eq!(decoded.registry_schema_version(), REGISTRY_SCHEMA_VERSION);
    assert_eq!(decoded.server_protocol_version(), SERVER_PROTOCOL_VERSION);
    assert_eq!(decoded.run_id(), run_id());
    assert_eq!(decoded.run_directory(), run_directory());
    assert_eq!(decoded.owner_pid(), 8123);
    assert_eq!(decoded.process_identity(), &process_identity());
    assert_eq!(decoded.bind().host(), "127.0.0.1");
    assert_eq!(decoded.bind().port(), 43120);
    assert_eq!(decoded.local_endpoint().as_str(), "http://127.0.0.1:43120/");
    assert_eq!(decoded.advertise_url(), None);
    assert_eq!(decoded.security_scope(), SecurityScope::TrustedNetwork);
    assert_eq!(decoded.started_at(), "2026-08-14T09:30:00.123456789Z");

    let wire: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(wire["registry_schema_version"], REGISTRY_SCHEMA_VERSION);
    assert_eq!(wire["server_protocol_version"], SERVER_PROTOCOL_VERSION);
    assert_eq!(wire["security_scope"], "trusted_network");
    assert_eq!(wire["advertise_url"], Value::Null);
    for forbidden in ["status", "running", "stopped", "failed", "outcome"] {
        assert!(wire.get(forbidden).is_none(), "dynamic field {forbidden}");
    }
}

#[test]
fn strict_codec_rejects_unknown_missing_and_newer_schema_with_original_path() {
    let source_path = Path::new("/registry path/instances/bad locator.json");
    let encoded = encode_registry_entry(&entry("localhost", None)).unwrap();
    let mut wire: Value = serde_json::from_slice(&encoded).unwrap();

    wire.as_object_mut()
        .unwrap()
        .insert("status".into(), json!("running"));
    let error =
        decode_registry_entry(source_path, &serde_json::to_vec(&wire).unwrap()).unwrap_err();
    assert_eq!(error.code(), RegistryCodecErrorCode::InvalidEntry);
    assert_eq!(error.path(), source_path);

    wire.as_object_mut().unwrap().remove("status");
    wire.as_object_mut().unwrap().remove("run_id");
    let error =
        decode_registry_entry(source_path, &serde_json::to_vec(&wire).unwrap()).unwrap_err();
    assert_eq!(error.code(), RegistryCodecErrorCode::InvalidEntry);
    assert_eq!(error.path(), source_path);

    wire.as_object_mut()
        .unwrap()
        .insert("registry_schema_version".into(), json!(2));
    wire.as_object_mut()
        .unwrap()
        .insert("future_required_field".into(), json!(true));
    let error =
        decode_registry_entry(source_path, &serde_json::to_vec(&wire).unwrap()).unwrap_err();
    assert_eq!(error.code(), RegistryCodecErrorCode::NewerSchema);
    assert_eq!(error.observed_schema_version(), Some(2));
    assert_eq!(error.path(), source_path);
}

#[test]
fn wildcard_bind_exports_only_loopback_and_advertise_does_not_change_bind() {
    let ipv4 = entry("0.0.0.0", None);
    assert_eq!(ipv4.bind().host(), "0.0.0.0");
    assert_eq!(ipv4.local_endpoint().as_str(), "http://127.0.0.1:43120/");

    let ipv6 = entry("::", None);
    assert_eq!(ipv6.bind().host(), "::");
    assert_eq!(ipv6.local_endpoint().as_str(), "http://[::1]:43120/");

    let advertised = entry("0.0.0.0", Some("https://diagnostics.example/troupe"));
    assert_eq!(advertised.bind().host(), "0.0.0.0");
    assert_eq!(advertised.bind().port(), 43120);
    assert_eq!(
        advertised.local_endpoint().as_str(),
        "http://127.0.0.1:43120/"
    );
    assert_eq!(
        advertised.advertise_url().unwrap().as_str(),
        "https://diagnostics.example/troupe"
    );
}

#[test]
fn locator_validation_rejects_unsafe_urls_and_store_identity_drift() {
    for invalid in [
        "ftp://diagnostics.example/",
        "http://user@diagnostics.example/",
        "http://diagnostics.example/path?query=1",
        "http://diagnostics.example/path#fragment",
        "//diagnostics.example/path",
        "http:///missing-host",
    ] {
        assert!(WebBaseUrl::parse(invalid).is_err(), "accepted {invalid}");
    }
    assert_eq!(
        WebBaseUrl::parse("http://diagnostics.example")
            .unwrap()
            .as_str(),
        "http://diagnostics.example/"
    );
    assert_eq!(
        BindEndpoint::new("0.0.0.0", 0).unwrap_err().code(),
        RegistryModelErrorCode::InvalidPort
    );

    let wrong_run_directory = PathBuf::from("/srv/production/.troupe/diagnostics/runs/other");
    let error = RegistryEntry::new(
        run_id(),
        &wrong_run_directory,
        1,
        process_identity(),
        BindEndpoint::new("127.0.0.1", 43120).unwrap(),
        None,
        "2026-08-14T09:30:00Z",
    )
    .unwrap_err();
    assert_eq!(error.code(), RegistryModelErrorCode::StoreIdentityMismatch);
}

#[test]
fn process_identity_classification_has_four_closed_conservative_states() {
    let expected = ProcessIdentity::new("test", "boot-a:10").unwrap();
    let reused = ProcessIdentity::new("test", "boot-a:11").unwrap();

    let alive =
        classify_process_identity(&expected, ObservedProcessIdentity::Alive(expected.clone()));
    let gone = classify_process_identity(&expected, ObservedProcessIdentity::DefinitelyGone);
    let pid_reused = classify_process_identity(&expected, ObservedProcessIdentity::Alive(reused));
    let unknown = classify_process_identity(&expected, ObservedProcessIdentity::Unknown);

    assert_eq!(alive, ProcessIdentityClassification::Alive);
    assert_eq!(gone, ProcessIdentityClassification::DefinitelyGone);
    assert_eq!(pid_reused, ProcessIdentityClassification::PidReused);
    assert_eq!(unknown, ProcessIdentityClassification::Unknown);
    assert!(!alive.is_definitely_stale());
    assert!(gone.is_definitely_stale());
    assert!(pid_reused.is_definitely_stale());
    assert!(!unknown.is_definitely_stale());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_process_identity_uses_boot_and_start_discriminator_without_registry_io() {
    let captured = current_process_identity().unwrap();
    let observed = observe_process_identity(std::process::id());
    assert_eq!(
        classify_process_identity(&captured, observed),
        ProcessIdentityClassification::Alive
    );

    assert_eq!(
        observe_process_identity(u32::MAX),
        ObservedProcessIdentity::DefinitelyGone
    );
}
