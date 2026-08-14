#[path = "../src/id.rs"]
mod id;
#[path = "../src/scalar.rs"]
mod scalar;
#[path = "../src/time.rs"]
mod time;
#[path = "../src/wire.rs"]
mod wire;

use std::time::{Duration, Instant};

use id::{CanonicalUuid, MAX_RUN_LOCAL_ID_BYTES, RunLocalId};
use scalar::{CurrencyCode, DecimalString, SchemaU64, TokenCount};
use time::{ElapsedNs, RunClock};
use uuid::Uuid;

#[test]
fn schema_u64_uses_canonical_decimal_string_for_the_full_domain() {
    for value in [0, 1, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
        let scalar = SchemaU64::new(value);
        assert_eq!(scalar.get(), value);
        let encoded = serde_json::to_string(&scalar).unwrap();
        assert_eq!(encoded, format!("\"{value}\""));
        assert_eq!(serde_json::from_str::<SchemaU64>(&encoded).unwrap(), scalar);
    }

    for invalid in [
        "0",
        "1",
        "-1",
        "true",
        "\"\"",
        "\"00\"",
        "\"+1\"",
        "\"18446744073709551616\"",
    ] {
        assert!(
            serde_json::from_str::<SchemaU64>(invalid).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn token_count_is_nonnegative_canonical_and_not_limited_to_u64() {
    let beyond_u64 = "184467440737095516160000000000000000000000000000000000000000000000";
    let count = TokenCount::parse(beyond_u64).unwrap();
    assert_eq!(count.as_str(), beyond_u64);
    assert_eq!(
        serde_json::to_string(&count).unwrap(),
        format!("\"{beyond_u64}\"")
    );
    assert_eq!(
        serde_json::from_str::<TokenCount>(&format!("\"{beyond_u64}\"")).unwrap(),
        count
    );

    for invalid in ["", "-1", "+1", "00", "01", "1.0", "false"] {
        assert!(TokenCount::parse(invalid).is_err(), "{invalid}");
    }
    assert!(serde_json::from_str::<TokenCount>("true").is_err());
    assert!(serde_json::from_str::<TokenCount>("123").is_err());
}

#[test]
fn decimal_string_normalizes_exact_finite_values_without_binary_float() {
    let cases = [
        ("0", "0"),
        ("-0.000", "0"),
        ("+001.2300", "1.23"),
        ("1e3", "1000"),
        ("12.3400e-2", "0.1234"),
        (".5", "0.5"),
        ("5.", "5"),
        ("-0005.0100", "-5.01"),
    ];
    for (input, expected) in cases {
        let value = DecimalString::parse(input).unwrap();
        assert_eq!(value.as_str(), expected);
        let encoded = format!("\"{expected}\"");
        assert_eq!(serde_json::to_string(&value).unwrap(), encoded);
        assert_eq!(
            serde_json::from_str::<DecimalString>(&encoded).unwrap(),
            value
        );
    }

    for invalid in [
        "",
        ".",
        "1e",
        "--1",
        " 1",
        "1 ",
        "NaN",
        "Infinity",
        "-Infinity",
        "1e999999999999999999999",
    ] {
        assert!(DecimalString::parse(invalid).is_err(), "{invalid}");
    }
    assert!(serde_json::from_str::<DecimalString>("1.5").is_err());
    assert!(serde_json::from_str::<DecimalString>("\"1.50\"").is_err());
}

#[test]
fn currency_is_an_exact_uppercase_iso_4217_code() {
    let currency = CurrencyCode::parse("USD").unwrap();
    assert_eq!(currency.as_str(), "USD");
    assert_eq!(serde_json::to_string(&currency).unwrap(), "\"USD\"");
    assert_eq!(
        serde_json::from_str::<CurrencyCode>("\"USD\"").unwrap(),
        currency
    );

    for invalid in ["", "US", "USDD", "usd", "U1D", "EURO", "\u{00a5}JP"] {
        assert!(CurrencyCode::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn canonical_uuid_has_one_lowercase_hyphenated_wire_form() {
    let text = "12345678-1234-4234-9234-123456789abc";
    let id = CanonicalUuid::parse(text).unwrap();
    assert_eq!(id.as_uuid(), Uuid::parse_str(text).unwrap());
    assert_eq!(CanonicalUuid::new(id.as_uuid()), id);
    assert_eq!(id.to_string(), text);
    assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{text}\""));
    assert_eq!(
        serde_json::from_str::<CanonicalUuid>(&format!("\"{text}\"")).unwrap(),
        id
    );

    for invalid in [
        "12345678123442349234123456789abc",
        "12345678-1234-4234-9234-123456789ABC",
        "{12345678-1234-4234-9234-123456789abc}",
        "not-a-uuid",
    ] {
        assert!(CanonicalUuid::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn run_local_id_is_nonempty_bounded_opaque_ascii() {
    let maximum = "x".repeat(MAX_RUN_LOCAL_ID_BYTES);
    assert_eq!(RunLocalId::parse(&maximum).unwrap().as_str(), maximum);

    for invalid in [
        "".to_owned(),
        "x".repeat(MAX_RUN_LOCAL_ID_BYTES + 1),
        "actor-☃".to_owned(),
    ] {
        assert!(RunLocalId::parse(&invalid).is_err(), "{invalid:?}");
    }
    assert!(serde_json::from_str::<RunLocalId>("0").is_err());
}

#[test]
fn elapsed_time_is_run_relative_monotonic_and_checked() {
    let origin = Instant::now();
    let clock = RunClock::from_origin(origin);
    assert_eq!(clock.origin(), origin);
    assert_eq!(clock.elapsed_at(origin).unwrap(), ElapsedNs::new(0));
    assert_eq!(
        clock.elapsed_at(origin + Duration::from_nanos(42)).unwrap(),
        ElapsedNs::new(42),
    );
    assert!(clock.elapsed_at(origin - Duration::from_nanos(1)).is_err());
    assert!(ElapsedNs::from_duration(Duration::from_secs(u64::MAX)).is_err());
    let elapsed_now = clock.elapsed_now().unwrap();
    assert_eq!(ElapsedNs::new(elapsed_now.get()), elapsed_now);
    assert_eq!(
        serde_json::to_string(&ElapsedNs::new(u64::MAX)).unwrap(),
        format!("\"{}\"", u64::MAX)
    );
}
