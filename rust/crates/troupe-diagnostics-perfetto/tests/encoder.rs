#[path = "../src/schema.rs"]
mod schema;

use prost::Message;
use schema::debug_annotation::{NameField as AnnotationName, Value as AnnotationValue};
use schema::trace_packet::Data;
use schema::track_descriptor::StaticOrDynamicName;
use schema::track_event::{CounterValueField, NameField};
use schema::{
    BuiltinClock, DebugAnnotation, Trace, TracePacket, TrackDescriptor, TrackEvent, TrackEventType,
    encode_trace_packet_fragment,
};

fn event_packet(timestamp: u64, event: TrackEvent) -> TracePacket {
    TracePacket {
        timestamp: Some(timestamp),
        data: Some(Data::TrackEvent(event)),
        timestamp_clock_id: Some(BuiltinClock::TraceFile as u32),
    }
}

fn event(event_type: TrackEventType, name: Option<&str>) -> TrackEvent {
    TrackEvent {
        debug_annotations: Vec::new(),
        r#type: Some(event_type as i32),
        track_uuid: Some(7),
        name_field: name.map(|name| NameField::Name(name.to_owned())),
        counter_value_field: None,
        flow_ids: Vec::new(),
        terminating_flow_ids: Vec::new(),
    }
}

fn fragment(packet: &TracePacket) -> Vec<u8> {
    let mut output = Vec::new();
    encode_trace_packet_fragment(packet, &mut output).unwrap();
    output
}

#[test]
fn trace_field_one_fragment_and_descriptor_are_byte_exact() {
    let packet = TracePacket {
        timestamp: None,
        data: Some(Data::TrackDescriptor(TrackDescriptor {
            uuid: Some(1),
            static_or_dynamic_name: Some(StaticOrDynamicName::Name("run".to_owned())),
            parent_uuid: Some(2),
        })),
        timestamp_clock_id: None,
    };

    let bytes = fragment(&packet);

    assert_eq!(
        bytes,
        [
            0x0a, 0x0c, 0xe2, 0x03, 0x09, 0x08, 0x01, 0x12, 0x03, b'r', b'u', b'n', 0x28, 0x02,
        ]
    );
    let trace = Trace::decode(bytes.as_slice()).unwrap();
    assert_eq!(trace.packet, vec![packet]);
}

#[test]
fn slice_begin_end_and_instant_packets_are_byte_exact() {
    let begin = fragment(&event_packet(
        5,
        event(TrackEventType::SliceBegin, Some("cue")),
    ));
    let end = fragment(&event_packet(9, event(TrackEventType::SliceEnd, None)));
    let instant = fragment(&event_packet(
        10,
        event(TrackEventType::Instant, Some("cue")),
    ));

    assert_eq!(
        begin,
        [
            0x0a, 0x11, 0x40, 0x05, 0x5a, 0x0a, 0x48, 0x01, 0x58, 0x07, 0xba, 0x01, 0x03, b'c',
            b'u', b'e', 0xd0, 0x03, 0x0b,
        ]
    );
    assert_eq!(
        end,
        [
            0x0a, 0x0b, 0x40, 0x09, 0x5a, 0x04, 0x48, 0x02, 0x58, 0x07, 0xd0, 0x03, 0x0b,
        ]
    );
    assert_eq!(
        instant,
        [
            0x0a, 0x11, 0x40, 0x0a, 0x5a, 0x0a, 0x48, 0x03, 0x58, 0x07, 0xba, 0x01, 0x03, b'c',
            b'u', b'e', 0xd0, 0x03, 0x0b,
        ]
    );
}

#[test]
fn integer_and_double_counter_packets_are_byte_exact() {
    let mut integer = event(TrackEventType::Counter, None);
    integer.counter_value_field = Some(CounterValueField::CounterValue(42));
    let mut double = event(TrackEventType::Counter, None);
    double.counter_value_field = Some(CounterValueField::DoubleCounterValue(1.5));

    assert_eq!(
        fragment(&event_packet(10, integer)),
        [
            0x0a, 0x0e, 0x40, 0x0a, 0x5a, 0x07, 0x48, 0x04, 0x58, 0x07, 0xf0, 0x01, 0x2a, 0xd0,
            0x03, 0x0b,
        ]
    );
    assert_eq!(
        fragment(&event_packet(10, double)),
        [
            0x0a, 0x15, 0x40, 0x0a, 0x5a, 0x0e, 0x48, 0x04, 0x58, 0x07, 0xe1, 0x02, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xf8, 0x3f, 0xd0, 0x03, 0x0b,
        ]
    );
}

#[test]
fn fixed64_flow_fields_are_unpacked_and_byte_exact() {
    let mut flow = event(TrackEventType::Instant, None);
    flow.flow_ids.push(0x0102_0304_0506_0708);
    flow.terminating_flow_ids.push(0x1112_1314_1516_1718);

    assert_eq!(
        fragment(&event_packet(12, flow)),
        [
            0x0a, 0x1f, 0x40, 0x0c, 0x5a, 0x18, 0x48, 0x03, 0x58, 0x07, 0xf9, 0x02, 0x08, 0x07,
            0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x81, 0x03, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13,
            0x12, 0x11, 0xd0, 0x03, 0x0b,
        ]
    );
}

#[test]
fn non_interned_debug_annotation_is_byte_exact() {
    let mut annotated = event(TrackEventType::Instant, Some("cue"));
    annotated.debug_annotations.push(DebugAnnotation {
        value: Some(AnnotationValue::StringValue("wait".to_owned())),
        name_field: Some(AnnotationName::Name("phase".to_owned())),
    });

    assert_eq!(
        fragment(&event_packet(13, annotated)),
        [
            0x0a, 0x20, 0x40, 0x0d, 0x5a, 0x19, 0x22, 0x0d, 0x32, 0x04, b'w', b'a', b'i', b't',
            0x52, 0x05, b'p', b'h', b'a', b's', b'e', 0x48, 0x03, 0x58, 0x07, 0xba, 0x01, 0x03,
            b'c', b'u', b'e', 0xd0, 0x03, 0x0b,
        ]
    );
}

#[test]
fn every_selected_debug_annotation_value_uses_its_stable_tag() {
    let cases = [
        (
            AnnotationValue::BoolValue(true),
            vec![0x10, 0x01, 0x52, 0x01, b'x'],
        ),
        (
            AnnotationValue::UintValue(42),
            vec![0x18, 0x2a, 0x52, 0x01, b'x'],
        ),
        (
            AnnotationValue::IntValue(42),
            vec![0x20, 0x2a, 0x52, 0x01, b'x'],
        ),
        (
            AnnotationValue::DoubleValue(1.5),
            vec![
                0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf8, 0x3f, 0x52, 0x01, b'x',
            ],
        ),
        (
            AnnotationValue::StringValue("v".to_owned()),
            vec![0x32, 0x01, b'v', 0x52, 0x01, b'x'],
        ),
    ];

    for (value, expected) in cases {
        let annotation = DebugAnnotation {
            value: Some(value),
            name_field: Some(AnnotationName::Name("x".to_owned())),
        };
        assert_eq!(annotation.encode_to_vec(), expected);
    }
}
