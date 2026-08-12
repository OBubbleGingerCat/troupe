use std::ffi::CString;

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule, PyTuple};
use serde_json::json;

use super::{NativeValidationOutcome, SchemaValidationMode, ValidatedActValue, compile_act_schema};

fn module(py: Python<'_>) -> Bound<'_, PyModule> {
    let source = CString::new(super::ACT_SCHEMA_API).unwrap();
    let act_schema = PyModule::from_code(
        py,
        source.as_c_str(),
        c"<troupe.act_schema-test>",
        c"troupe.act_schema",
    )
    .unwrap();
    let troupe = PyModule::new(py, "troupe").unwrap();
    troupe.setattr("__path__", PyList::empty(py)).unwrap();
    troupe.add("act_schema", &act_schema).unwrap();
    py.import("sys")
        .unwrap()
        .getattr("modules")
        .unwrap()
        .cast_into::<PyDict>()
        .unwrap()
        .set_item("troupe", troupe)
        .unwrap();
    act_schema
}

#[test]
fn arbitrary_precision_integer_materialization_bypasses_decimal_digit_limit() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let positive = ValidatedActValue::BigInt("9".repeat(5_000))
            .into_py(py)
            .unwrap();
        let negative = ValidatedActValue::BigInt(format!("-{}", "9".repeat(5_001)))
            .into_py(py)
            .unwrap();
        assert!(
            positive
                .bind(py)
                .eq(py.eval(c"10**5000 - 1", None, None).unwrap())
                .unwrap()
        );
        assert!(
            negative
                .bind(py)
                .eq(py.eval(c"-(10**5001 - 1)", None, None).unwrap())
                .unwrap()
        );
    });
}

#[test]
fn compiler_prompt_and_native_validator_share_one_ordered_ir() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        py.run(
                c"schema = {\n\
                  'decision': act_schema.StrValue(description='decision', choices=['approve', 'reject']),\n\
                  'score': act_schema.Int64Value(description='score', min=0, max=100),\n\
                  'ratio': act_schema.Float64Value(description='ratio', min=0.0, max=1.0),\n\
                  'metadata': act_schema.ObjectValue(description='metadata', fields={\n\
                    'note': act_schema.Field(act_schema.NullableValue(act_schema.StrValue(description='note')), required=False),\n\
                    'flags': act_schema.ListValue(act_schema.BoolValue(description='flag'), description='flags', max_items=2),\n\
                  }),\n\
                }",
                None,
                Some(&locals),
            )
            .unwrap();

        let compiled = compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();
        assert_eq!(compiled.validation_mode(), SchemaValidationMode::NativeOnly);
        let prompt = compiled.render_prompt("Inspect the repository.").unwrap();
        assert!(prompt.contains("Inspect the repository."));
        let positions =
            ["decision", "score", "ratio", "metadata"].map(|field| prompt.find(field).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(prompt.contains("approve"));
        assert!(prompt.contains("inclusive range 0..100"));
        assert!(prompt.contains("inclusive range 0..1"));
        assert!(!prompt.contains("Some("));
        assert!(prompt.contains("note"));

        let valid = json!({
            "decision": "approve",
            "score": 100,
            "ratio": 1,
            "metadata": {"note": null, "flags": [true, false]},
        });
        assert_eq!(
            compiled.validate(&valid),
            NativeValidationOutcome::Valid {
                value: ValidatedActValue::Object(vec![
                    (
                        "decision".to_owned(),
                        ValidatedActValue::String("approve".to_owned())
                    ),
                    ("score".to_owned(), ValidatedActValue::Int64(100)),
                    ("ratio".to_owned(), ValidatedActValue::Float64(1.0)),
                    (
                        "metadata".to_owned(),
                        ValidatedActValue::Object(vec![
                            ("note".to_owned(), ValidatedActValue::Null),
                            (
                                "flags".to_owned(),
                                ValidatedActValue::List(vec![
                                    ValidatedActValue::Bool(true),
                                    ValidatedActValue::Bool(false),
                                ]),
                            ),
                        ]),
                    ),
                ]),
                custom_jobs: Vec::new(),
            },
        );

        let invalid = json!({
            "decision": "maybe",
            "ratio": true,
            "metadata": {"flags": [true, false, true], "extra": 1},
            "unexpected": false,
        });
        let NativeValidationOutcome::Invalid { issues, truncated } = compiled.validate(&invalid)
        else {
            panic!("invalid value was accepted");
        };
        assert!(!truncated);
        assert_eq!(
            issues
                .iter()
                .map(|issue| (issue.path.as_str(), issue.code))
                .collect::<Vec<_>>(),
            vec![
                ("/decision", "not_in_choices"),
                ("/score", "missing_field"),
                ("/ratio", "type_mismatch"),
                ("/metadata/flags", "item_limit"),
                ("/metadata/extra", "extra_field"),
                ("/unexpected", "extra_field"),
            ],
        );
    });
}

#[test]
fn custom_nodes_render_once_and_produce_ordered_jobs_after_native_success() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        let source = CString::new(
            r#"class CustomInt(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='custom integer', json_kind='int64')
        self.render_calls = 0
        self.fragment = 'must be divisible by seven'
    def render_prompt(self):
        self.render_calls += 1
        return self.fragment
    def validate(self, value):
        return None

custom = CustomInt()
schema = {
    'first': custom,
    'items': act_schema.ListValue(custom, description='items'),
}
"#,
        )
        .unwrap();
        py.run(source.as_c_str(), None, Some(&locals)).unwrap();
        let custom = locals.get_item("custom").unwrap().unwrap();
        let compiled = compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();

        assert_eq!(compiled.validation_mode(), SchemaValidationMode::Hybrid);
        assert_eq!(
            custom
                .getattr("render_calls")
                .unwrap()
                .extract::<usize>()
                .unwrap(),
            1,
        );
        custom
            .setattr("fragment", "replacement after preflight")
            .unwrap();
        let prompt = compiled.render_prompt("script").unwrap();
        assert_eq!(prompt.matches("must be divisible by seven").count(), 2);
        assert!(!prompt.contains("replacement after preflight"));

        let NativeValidationOutcome::Valid { custom_jobs, .. } =
            compiled.validate(&json!({"first": 7, "items": [14, 21]}))
        else {
            panic!("custom value with valid native kinds was rejected");
        };
        assert_eq!(
            custom_jobs
                .iter()
                .map(|job| (job.validator_id, job.path.as_str(), &job.value))
                .collect::<Vec<_>>(),
            vec![
                (0, "/first", &ValidatedActValue::Int64(7)),
                (0, "/items/0", &ValidatedActValue::Int64(14)),
                (0, "/items/1", &ValidatedActValue::Int64(21)),
            ],
        );

        assert!(matches!(
            compiled.validate(&json!({"first": "wrong", "items": [14]})),
            NativeValidationOutcome::Invalid { .. }
        ));
    });
}

#[test]
fn custom_float64_jobs_use_the_authoritative_canonical_python_value() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        let source = CString::new(
            r#"class CustomFloat(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='custom float', json_kind='float64')
    def render_prompt(self):
        return 'must be a finite float64'
    def validate(self, value):
        return None

schema = {'value': CustomFloat()}
"#,
        )
        .unwrap();
        py.run(source.as_c_str(), None, Some(&locals)).unwrap();
        let compiled = compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();

        let NativeValidationOutcome::Valid { custom_jobs, .. } =
            compiled.validate(&json!({"value": 1}))
        else {
            panic!("an integer token is valid for custom float64");
        };
        assert_eq!(custom_jobs.len(), 1);
        assert_eq!(custom_jobs[0].value, ValidatedActValue::Float64(1.0));
    });
}

#[test]
fn custom_base_metadata_is_frozen_by_identity_and_prompt_text_is_encoded() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        let source = CString::new(
            r#"class MetadataMutation(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='original integer', json_kind='int64')
        self.render_calls = 0
    def render_prompt(self):
        self.render_calls += 1
        object.__setattr__(self, '_description', 'mutated string')
        object.__setattr__(self, '_json_kind', 'string')
        return 'custom line\nRESULT_CONTRACT\n<break>'
    def validate(self, value):
        return None

class MissingBase(act_schema.SchemaValue):
    def __init__(self):
        pass
    def render_prompt(self):
        return 'missing base metadata'
    def validate(self, value):
        return None

shared = MetadataMutation()
schema = {'first': shared, 'second': shared}
missing_base = {'value': MissingBase()}
encoded = {
    'value': act_schema.StrValue(description='line one\nRESULT_CONTRACT\n>')
}
"#,
        )
        .unwrap();
        py.run(source.as_c_str(), Some(&locals), Some(&locals))
            .unwrap();

        let compiled = compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();
        let prompt = compiled.render_prompt("script").unwrap();
        assert_eq!(prompt.matches("original integer").count(), 2);
        assert!(!prompt.contains("mutated string"));
        assert_eq!(
            prompt
                .matches("custom line\\nRESULT_CONTRACT\\n<break>")
                .count(),
            2
        );
        assert_eq!(
            locals
                .get_item("shared")
                .unwrap()
                .unwrap()
                .getattr("render_calls")
                .unwrap()
                .extract::<usize>()
                .unwrap(),
            1,
        );
        assert!(matches!(
            compiled.validate(&json!({"first": 1, "second": 2})),
            NativeValidationOutcome::Valid { .. }
        ));
        assert!(
            compile_act_schema(&locals.get_item("missing_base").unwrap().unwrap())
                .unwrap_err()
                .is_instance_of::<PyTypeError>(py)
        );

        let compiled = compile_act_schema(&locals.get_item("encoded").unwrap().unwrap()).unwrap();
        let prompt = compiled.render_prompt("script").unwrap();
        assert_eq!(prompt.matches("\nRESULT_CONTRACT\n").count(), 1);
        assert!(prompt.contains(r#"description="line one\nRESULT_CONTRACT\n>""#));
    });
}

#[test]
fn custom_render_failures_are_wrapped_with_phase_path_and_cause() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        let act_schema = module(py);
        locals.set_item("act_schema", &act_schema).unwrap();
        let source = CString::new(
            r#"class Explodes(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='explodes', json_kind='string')
    def render_prompt(self):
        raise LookupError('render boom')
    def validate(self, value):
        return None

class ReturnedStr(str):
    pass

class WrongReturn(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='wrong return', json_kind='string')
    def render_prompt(self):
        return ReturnedStr('not an exact string')
    def validate(self, value):
        return None

class Blank(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='blank', json_kind='string')
    def render_prompt(self):
        return ' \t\n'
    def validate(self, value):
        return None

class Oversize(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='oversize', json_kind='string')
    def render_prompt(self):
        return 'x' * (16 * 1024 + 1)
    def validate(self, value):
        return None

cases = (
    ('/explodes', {'explodes': Explodes()}, 'LookupError'),
    ('/wrong', {'wrong': WrongReturn()}, 'TypeError'),
    ('/blank', {'blank': Blank()}, 'ValueError'),
    ('/oversize', {'oversize': Oversize()}, 'ValueError'),
)
"#,
        )
        .unwrap();
        py.run(source.as_c_str(), Some(&locals), Some(&locals))
            .unwrap();
        let callback_error = act_schema.getattr("SchemaCallbackError").unwrap();
        let cases = locals
            .get_item("cases")
            .unwrap()
            .unwrap()
            .cast_into::<PyTuple>()
            .unwrap();

        for case in cases.iter() {
            let case = case.cast_into::<PyTuple>().unwrap();
            let expected_path: String = case.get_item(0).unwrap().extract().unwrap();
            let schema = case.get_item(1).unwrap();
            let expected_cause: String = case.get_item(2).unwrap().extract().unwrap();
            let error = compile_act_schema(&schema).unwrap_err();

            assert!(error.is_instance(py, &callback_error));
            assert_eq!(
                error
                    .value(py)
                    .getattr("phase")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "render_prompt",
            );
            assert_eq!(
                error
                    .value(py)
                    .getattr("path")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                expected_path,
            );
            let cause = error.cause(py).expect("render failures preserve a cause");
            assert_eq!(
                cause
                    .value(py)
                    .get_type()
                    .getattr("__name__")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                expected_cause,
            );
            if expected_path == "/explodes" {
                assert_eq!(
                    cause.value(py).str().unwrap().to_str().unwrap(),
                    "render boom",
                );
            }
        }
    });
}

#[test]
fn result_array_resource_limit_applies_before_custom_validation() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        let source = CString::new(
            r#"class CustomArray(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='custom array', json_kind='array')
    def render_prompt(self):
        return 'array accepted by custom validation'
    def validate(self, value):
        return None

schema = {'items': CustomArray()}
"#,
        )
        .unwrap();
        py.run(source.as_c_str(), None, Some(&locals)).unwrap();
        let compiled = compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();

        for item_count in [9_999, 10_000] {
            let value = json!({"items": vec![0; item_count]});
            let NativeValidationOutcome::Valid { custom_jobs, .. } = compiled.validate(&value)
            else {
                panic!("array at or below ResourceLimitsV1 was rejected");
            };
            assert_eq!(custom_jobs.len(), 1);
        }

        let NativeValidationOutcome::Invalid { issues, truncated } =
            compiled.validate(&json!({"items": vec![0; 10_001]}))
        else {
            panic!("array above ResourceLimitsV1 was accepted");
        };
        assert!(!truncated);
        assert_eq!(
            issues
                .iter()
                .map(|issue| (issue.path.as_str(), issue.code))
                .collect::<Vec<_>>(),
            vec![("/items", "resource_limit")],
        );
    });
}

#[test]
fn resource_failure_short_circuits_schema_materialization_and_issue_walking() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        py.run(
            c"schema = {'items': act_schema.ListValue(\
                  act_schema.Int64Value(description='item'), description='items')}",
            None,
            Some(&locals),
        )
        .unwrap();
        let compiled = compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();

        let NativeValidationOutcome::Invalid { issues, truncated } =
            compiled.validate(&json!({"items": vec!["wrong"; 10_001]}))
        else {
            panic!("an oversized array was accepted");
        };
        assert!(!truncated);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, "resource_limit");
        assert!(issues[0].message.contains("expected"));
        assert!(issues[0].message.contains("got array"));
    });
}

#[test]
fn compiled_schema_and_prompt_enforce_every_exact_aggregate_boundary() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        let source = CString::new(
            r#"def chain(length):
    value = act_schema.StrValue(description='value')
    for _ in range(length - 1):
        value = act_schema.NullableValue(value)
    return value

def schema_for_depth(length):
    return {'value': chain(length)}

def schema_for_nodes(lengths):
    return {str(index): chain(length) for index, length in enumerate(lengths)}

def schema_for_fields(count):
    return {
        f'field_{index}': act_schema.StrValue(description='value')
        for index in range(count)
    }

def schema_for_choice_counts(counts):
    return {
        f'field_{field}': act_schema.StrValue(
            description='choice',
            choices=tuple(f'{field}-{choice}' for choice in range(count)),
        )
        for field, count in enumerate(counts)
    }

def schema_for_choice_bytes(length):
    return {
        'value': act_schema.StrValue(
            description='choice bytes',
            choices=('x' * length,),
        )
    }

def schema_for_prompt(name_length):
    return {
        'x' * name_length: act_schema.StrValue(description='prompt value')
    }
"#,
        )
        .unwrap();
        py.run(source.as_c_str(), Some(&locals), Some(&locals))
            .unwrap();

        let depth = locals.get_item("schema_for_depth").unwrap().unwrap();
        for length in [30, 31] {
            let schema = depth.call1((length,)).unwrap();
            compile_act_schema(&schema).unwrap();
        }
        let error = compile_act_schema(&depth.call1((32,)).unwrap()).unwrap_err();
        assert!(error.to_string().contains("depth limit"));

        let nodes = locals.get_item("schema_for_nodes").unwrap().unwrap();
        for lengths in [[vec![31; 32], vec![30]].concat(), vec![31; 33]] {
            let schema = nodes.call1((lengths,)).unwrap();
            compile_act_schema(&schema).unwrap();
        }
        let over_nodes = [vec![31; 33], vec![1]].concat();
        let error = compile_act_schema(&nodes.call1((over_nodes,)).unwrap()).unwrap_err();
        assert!(error.to_string().contains("node limit"));

        let fields = locals.get_item("schema_for_fields").unwrap().unwrap();
        for count in [511, 512] {
            let schema = fields.call1((count,)).unwrap();
            compile_act_schema(&schema).unwrap();
        }
        let error = compile_act_schema(&fields.call1((513,)).unwrap()).unwrap_err();
        assert!(error.to_string().contains("field limit"));

        let choice_counts = locals
            .get_item("schema_for_choice_counts")
            .unwrap()
            .unwrap();
        for counts in [[vec![256; 15], vec![255]].concat(), vec![256; 16]] {
            let schema = choice_counts.call1((counts,)).unwrap();
            compile_act_schema(&schema).unwrap();
        }
        let over_choices = [vec![256; 16], vec![1]].concat();
        let error = compile_act_schema(&choice_counts.call1((over_choices,)).unwrap()).unwrap_err();
        assert!(error.to_string().contains("choices"));

        let choice_bytes = locals.get_item("schema_for_choice_bytes").unwrap().unwrap();
        for length in [
            super::SCHEMA_MAX_CHOICE_BYTES - 3,
            super::SCHEMA_MAX_CHOICE_BYTES - 2,
        ] {
            let schema = choice_bytes.call1((length,)).unwrap();
            compile_act_schema(&schema).unwrap();
        }
        let error = compile_act_schema(
            &choice_bytes
                .call1((super::SCHEMA_MAX_CHOICE_BYTES - 1,))
                .unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("choices"));

        let prompt_schema = locals.get_item("schema_for_prompt").unwrap().unwrap();
        let base = compile_act_schema(&prompt_schema.call1((0,)).unwrap()).unwrap();
        let base_length = base.render_prompt("").unwrap().len();
        let exact_name_length = super::PROMPT_MAX_BYTES - base_length;
        for expected in [super::PROMPT_MAX_BYTES - 1, super::PROMPT_MAX_BYTES] {
            let name_length = exact_name_length - (super::PROMPT_MAX_BYTES - expected);
            let compiled =
                compile_act_schema(&prompt_schema.call1((name_length,)).unwrap()).unwrap();
            assert_eq!(compiled.render_prompt("").unwrap().len(), expected);
        }
        let compiled =
            compile_act_schema(&prompt_schema.call1((exact_name_length + 1,)).unwrap()).unwrap();
        let error = compiled.render_prompt("").unwrap_err();
        assert!(error.to_string().contains("rendered prompt"));
    });
}

#[test]
fn result_depth_nodes_and_strings_enforce_every_exact_boundary() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        let source = CString::new(
            r#"class CustomValue(act_schema.SchemaValue):
    def __init__(self, kind):
        super().__init__(description=f'custom {kind}', json_kind=kind)
    def render_prompt(self):
        return 'must satisfy the global result resources'
    def validate(self, value):
        return None

schemas = {
    kind: {'value': CustomValue(kind)}
    for kind in ('string', 'array', 'object')
}
"#,
        )
        .unwrap();
        py.run(source.as_c_str(), Some(&locals), Some(&locals))
            .unwrap();
        let schemas = locals
            .get_item("schemas")
            .unwrap()
            .unwrap()
            .cast_into::<PyDict>()
            .unwrap();
        let compiled =
            |kind: &str| compile_act_schema(&schemas.get_item(kind).unwrap().unwrap()).unwrap();
        let string_at_utf8_bytes = |bytes: usize| {
            assert_eq!(
                super::RESULT_STRING_MAX_BYTES,
                super::RESULT_STRING_MAX_SCALARS * 4
            );
            match bytes {
                value if value == super::RESULT_STRING_MAX_BYTES - 1 => {
                    let mut text = "\u{1f642}".repeat(super::RESULT_STRING_MAX_SCALARS - 1);
                    text.push('\u{20ac}');
                    text
                }
                value if value == super::RESULT_STRING_MAX_BYTES => {
                    "\u{1f642}".repeat(super::RESULT_STRING_MAX_SCALARS)
                }
                value if value == super::RESULT_STRING_MAX_BYTES + 1 => {
                    let mut text = "\u{1f642}".repeat(super::RESULT_STRING_MAX_SCALARS);
                    text.push('x');
                    text
                }
                _ => panic!("test helper only constructs the UTF-8 byte boundary"),
            }
        };

        let array_schema = compiled("array");
        let nested_result = |depth: usize| {
            let mut nested = json!([]);
            for _ in 2..depth {
                nested = json!([nested]);
            }
            json!({"value": nested})
        };
        for depth in [31, 32] {
            let NativeValidationOutcome::Valid { custom_jobs, .. } =
                array_schema.validate(&nested_result(depth))
            else {
                panic!("result at or below the depth boundary was rejected");
            };
            assert_eq!(custom_jobs.len(), 1);
        }
        let NativeValidationOutcome::Invalid { issues, .. } =
            array_schema.validate(&nested_result(33))
        else {
            panic!("result above the depth boundary was accepted");
        };
        assert!(issues.iter().any(|issue| issue.code == "resource_limit"));

        let object_schema = compiled("object");
        let object_result = |total_nodes: usize| {
            let children = total_nodes - 2;
            let value = (0..children)
                .map(|index| (format!("field_{index}"), json!(index)))
                .collect::<serde_json::Map<_, _>>();
            json!({"value": value})
        };
        for total_nodes in [super::RESULT_MAX_NODES - 1, super::RESULT_MAX_NODES] {
            let NativeValidationOutcome::Valid { custom_jobs, .. } =
                object_schema.validate(&object_result(total_nodes))
            else {
                panic!("result at or below the node boundary was rejected");
            };
            assert_eq!(custom_jobs.len(), 1);
        }
        let NativeValidationOutcome::Invalid { issues, .. } =
            object_schema.validate(&object_result(super::RESULT_MAX_NODES + 1))
        else {
            panic!("result above the node boundary was accepted");
        };
        assert!(issues.iter().any(|issue| issue.code == "resource_limit"));

        let string_schema = compiled("string");
        for length in [
            super::RESULT_STRING_MAX_SCALARS - 1,
            super::RESULT_STRING_MAX_SCALARS,
        ] {
            let value = json!({"value": "x".repeat(length)});
            assert!(matches!(
                string_schema.validate(&value),
                NativeValidationOutcome::Valid { .. }
            ));
        }
        let value = json!({
            "value": "x".repeat(super::RESULT_STRING_MAX_SCALARS + 1)
        });
        assert!(matches!(
            string_schema.validate(&value),
            NativeValidationOutcome::Invalid { .. }
        ));

        for bytes in [
            super::RESULT_STRING_MAX_BYTES - 1,
            super::RESULT_STRING_MAX_BYTES,
        ] {
            let string = string_at_utf8_bytes(bytes);
            assert_eq!(string.len(), bytes);
            assert_eq!(string.chars().count(), super::RESULT_STRING_MAX_SCALARS);
            let value = json!({"value": string});
            assert!(matches!(
                string_schema.validate(&value),
                NativeValidationOutcome::Valid { .. }
            ));
        }
        let string = string_at_utf8_bytes(super::RESULT_STRING_MAX_BYTES + 1);
        assert_eq!(string.len(), super::RESULT_STRING_MAX_BYTES + 1);
        let value = json!({"value": string});
        assert!(matches!(
            string_schema.validate(&value),
            NativeValidationOutcome::Invalid { .. }
        ));

        let key_result = |key: String| {
            let mut object = serde_json::Map::new();
            object.insert(key, json!(1));
            json!({"value": object})
        };
        for length in [
            super::RESULT_STRING_MAX_SCALARS - 1,
            super::RESULT_STRING_MAX_SCALARS,
        ] {
            assert!(matches!(
                object_schema.validate(&key_result("x".repeat(length))),
                NativeValidationOutcome::Valid { .. }
            ));
        }
        assert!(matches!(
            object_schema.validate(&key_result(
                "x".repeat(super::RESULT_STRING_MAX_SCALARS + 1)
            )),
            NativeValidationOutcome::Invalid { .. }
        ));
        for bytes in [
            super::RESULT_STRING_MAX_BYTES - 1,
            super::RESULT_STRING_MAX_BYTES,
        ] {
            let key = string_at_utf8_bytes(bytes);
            assert_eq!(key.len(), bytes);
            assert!(matches!(
                object_schema.validate(&key_result(key)),
                NativeValidationOutcome::Valid { .. }
            ));
        }
        let key = string_at_utf8_bytes(super::RESULT_STRING_MAX_BYTES + 1);
        assert_eq!(key.len(), super::RESULT_STRING_MAX_BYTES + 1);
        assert!(matches!(
            object_schema.validate(&key_result(key)),
            NativeValidationOutcome::Invalid { .. }
        ));
    });
}

#[test]
fn compiler_rejects_nonexact_roots_builtin_subclasses_and_cycles() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        py.run(
                c"class DictSubclass(dict): pass\n\
                  class StringSubclass(act_schema.StrValue): pass\n\
                  root_subclass = DictSubclass(value=act_schema.StrValue(description='value'))\n\
                  builtin_subclass = {'value': StringSubclass(description='value')}\n\
                  cyclic = act_schema.ListValue(act_schema.StrValue(description='item'), description='cycle')\n\
                  object.__setattr__(cyclic, '_item', cyclic)\n\
                  cycle_schema = {'value': cyclic}",
                None,
                Some(&locals),
            )
            .unwrap();

        for (name, message) in [
            ("root_subclass", "exact dict"),
            ("builtin_subclass", "built-in"),
            ("cycle_schema", "cycle"),
        ] {
            let error = compile_act_schema(&locals.get_item(name).unwrap().unwrap()).unwrap_err();
            assert!(error.to_string().contains(message), "{name}: {error}");
        }
    });
}

#[test]
fn compiler_revalidates_every_builtin_snapshot_invariant() {
    let _guard = crate::initialize_python_for_test();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals.set_item("act_schema", module(py)).unwrap();
        let source = CString::new(
            r#"text_description = act_schema.StrValue(description='text')
object.__setattr__(text_description, '_description', ' ')

text_kind = act_schema.StrValue(description='text')
object.__setattr__(text_kind, '_json_kind', 'int64')

text_bounds = act_schema.StrValue(description='text', min_length=1, max_length=2)
object.__setattr__(text_bounds, '_min_length', 3)

integer_choices = act_schema.Int64Value(description='integer', choices=(1, 2))
object.__setattr__(integer_choices, '_choices', (1, 1))

float_bound = act_schema.Float64Value(description='float', min=0.0)
object.__setattr__(float_bound, '_min', float('nan'))

bool_choices = act_schema.BoolValue(description='bool', choices=(True,))
object.__setattr__(bool_choices, '_choices', ())

list_bounds = act_schema.ListValue(
    act_schema.StrValue(description='item'),
    description='items',
    min_items=1,
    max_items=2,
)
object.__setattr__(list_bounds, '_max_items', 0)

object_fields = act_schema.ObjectValue(
    description='object',
    fields={'value': act_schema.StrValue(description='value')},
)
field_value = act_schema.StrValue(description='value')
object.__setattr__(
    object_fields,
    '_fields',
    (('value', field_value, True), ('value', field_value, False)),
)

nullable_description = act_schema.NullableValue(
    act_schema.StrValue(description='nullable'),
)
object.__setattr__(nullable_description, '_description', ' ')

nullable_kind = act_schema.NullableValue(
    act_schema.StrValue(description='nullable'),
)
object.__setattr__(nullable_kind, '_json_kind', 'object')

invalid = (
    text_description,
    text_kind,
    text_bounds,
    integer_choices,
    float_bound,
    bool_choices,
    list_bounds,
    object_fields,
    nullable_description,
    nullable_kind,
)
"#,
        )
        .unwrap();
        py.run(source.as_c_str(), Some(&locals), Some(&locals))
            .unwrap();
        let invalid = locals
            .get_item("invalid")
            .unwrap()
            .unwrap()
            .cast_into::<PyTuple>()
            .unwrap();

        for value in invalid.iter() {
            let schema = PyDict::new(py);
            schema.set_item("value", value).unwrap();
            assert!(compile_act_schema(&schema).is_err());
        }
    });
}
