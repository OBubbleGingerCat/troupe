use prost::Message;

// This is a deliberately private mirror of the stable Perfetto fields listed in
// schema/used-fields.json. Keep schema changes auditable instead of generating
// code from Perfetto's much larger, partially unstable schema graph.

// perfetto-schema: definition perfetto.protos.BuiltinClock
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum BuiltinClock {
    // perfetto-schema: enum-value perfetto.protos.BuiltinClock.BUILTIN_CLOCK_UNKNOWN
    Unknown = 0,
    // perfetto-schema: enum-value perfetto.protos.BuiltinClock.BUILTIN_CLOCK_TRACE_FILE
    TraceFile = 11,
}

// perfetto-schema: definition perfetto.protos.Trace
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Trace {
    // perfetto-schema: field perfetto.protos.Trace.packet
    #[prost(message, repeated, tag = "1")]
    pub(crate) packet: Vec<TracePacket>,
}

// perfetto-schema: definition perfetto.protos.TracePacket
#[derive(Clone, PartialEq, Message)]
pub(crate) struct TracePacket {
    // perfetto-schema: field perfetto.protos.TracePacket.timestamp
    #[prost(uint64, optional, tag = "8")]
    pub(crate) timestamp: Option<u64>,
    #[prost(oneof = "trace_packet::OptionalTrustedPacketSequenceId", tags = "10")]
    pub(crate) optional_trusted_packet_sequence_id:
        Option<trace_packet::OptionalTrustedPacketSequenceId>,
    #[prost(oneof = "trace_packet::Data", tags = "11, 60")]
    pub(crate) data: Option<trace_packet::Data>,
    // perfetto-schema: field perfetto.protos.TracePacket.timestamp_clock_id
    #[prost(uint32, optional, tag = "58")]
    pub(crate) timestamp_clock_id: Option<u32>,
}

pub(crate) mod trace_packet {
    // perfetto-schema: oneof perfetto.protos.TracePacket.optional_trusted_packet_sequence_id
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum OptionalTrustedPacketSequenceId {
        // perfetto-schema: field perfetto.protos.TracePacket.trusted_packet_sequence_id
        #[prost(uint32, tag = "10")]
        TrustedPacketSequenceId(u32),
    }

    // perfetto-schema: oneof perfetto.protos.TracePacket.data
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum Data {
        // perfetto-schema: field perfetto.protos.TracePacket.track_event
        #[prost(message, tag = "11")]
        TrackEvent(super::TrackEvent),
        // perfetto-schema: field perfetto.protos.TracePacket.track_descriptor
        #[prost(message, tag = "60")]
        TrackDescriptor(super::TrackDescriptor),
    }
}

// perfetto-schema: definition perfetto.protos.TrackDescriptor
#[derive(Clone, PartialEq, Message)]
pub(crate) struct TrackDescriptor {
    // perfetto-schema: field perfetto.protos.TrackDescriptor.uuid
    #[prost(uint64, optional, tag = "1")]
    pub(crate) uuid: Option<u64>,
    #[prost(oneof = "track_descriptor::StaticOrDynamicName", tags = "2")]
    pub(crate) static_or_dynamic_name: Option<track_descriptor::StaticOrDynamicName>,
    // perfetto-schema: field perfetto.protos.TrackDescriptor.parent_uuid
    #[prost(uint64, optional, tag = "5")]
    pub(crate) parent_uuid: Option<u64>,
    // perfetto-schema: field perfetto.protos.TrackDescriptor.counter
    #[prost(message, optional, tag = "8")]
    pub(crate) counter: Option<CounterDescriptor>,
}

pub(crate) mod track_descriptor {
    // perfetto-schema: oneof perfetto.protos.TrackDescriptor.static_or_dynamic_name
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum StaticOrDynamicName {
        // perfetto-schema: field perfetto.protos.TrackDescriptor.name
        #[prost(string, tag = "2")]
        Name(String),
    }
}

// perfetto-schema: definition perfetto.protos.CounterDescriptor
#[derive(Clone, PartialEq, Message)]
pub(crate) struct CounterDescriptor {}

// perfetto-schema: definition perfetto.protos.TrackEvent
#[derive(Clone, PartialEq, Message)]
pub(crate) struct TrackEvent {
    // perfetto-schema: field perfetto.protos.TrackEvent.debug_annotations
    #[prost(message, repeated, tag = "4")]
    pub(crate) debug_annotations: Vec<DebugAnnotation>,
    // perfetto-schema: field perfetto.protos.TrackEvent.type
    #[prost(enumeration = "TrackEventType", optional, tag = "9")]
    pub(crate) r#type: Option<i32>,
    // perfetto-schema: field perfetto.protos.TrackEvent.track_uuid
    #[prost(uint64, optional, tag = "11")]
    pub(crate) track_uuid: Option<u64>,
    #[prost(oneof = "track_event::NameField", tags = "23")]
    pub(crate) name_field: Option<track_event::NameField>,
    #[prost(oneof = "track_event::CounterValueField", tags = "30, 44")]
    pub(crate) counter_value_field: Option<track_event::CounterValueField>,
    // perfetto-schema: field perfetto.protos.TrackEvent.flow_ids
    #[prost(fixed64, repeated, packed = "false", tag = "47")]
    pub(crate) flow_ids: Vec<u64>,
    // perfetto-schema: field perfetto.protos.TrackEvent.terminating_flow_ids
    #[prost(fixed64, repeated, packed = "false", tag = "48")]
    pub(crate) terminating_flow_ids: Vec<u64>,
}

// perfetto-schema: definition perfetto.protos.TrackEvent.Type
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum TrackEventType {
    // perfetto-schema: enum-value perfetto.protos.TrackEvent.Type.TYPE_UNSPECIFIED
    Unspecified = 0,
    // perfetto-schema: enum-value perfetto.protos.TrackEvent.Type.TYPE_SLICE_BEGIN
    SliceBegin = 1,
    // perfetto-schema: enum-value perfetto.protos.TrackEvent.Type.TYPE_SLICE_END
    SliceEnd = 2,
    // perfetto-schema: enum-value perfetto.protos.TrackEvent.Type.TYPE_INSTANT
    Instant = 3,
    // perfetto-schema: enum-value perfetto.protos.TrackEvent.Type.TYPE_COUNTER
    Counter = 4,
}

pub(crate) mod track_event {
    // perfetto-schema: oneof perfetto.protos.TrackEvent.name_field
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum NameField {
        // perfetto-schema: field perfetto.protos.TrackEvent.name
        #[prost(string, tag = "23")]
        Name(String),
    }

    // perfetto-schema: oneof perfetto.protos.TrackEvent.counter_value_field
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum CounterValueField {
        // perfetto-schema: field perfetto.protos.TrackEvent.counter_value
        #[prost(int64, tag = "30")]
        CounterValue(i64),
        // perfetto-schema: field perfetto.protos.TrackEvent.double_counter_value
        #[prost(double, tag = "44")]
        DoubleCounterValue(f64),
    }
}

// perfetto-schema: definition perfetto.protos.DebugAnnotation
#[derive(Clone, PartialEq, Message)]
pub(crate) struct DebugAnnotation {
    #[prost(oneof = "debug_annotation::Value", tags = "2, 3, 4, 5, 6")]
    pub(crate) value: Option<debug_annotation::Value>,
    #[prost(oneof = "debug_annotation::NameField", tags = "10")]
    pub(crate) name_field: Option<debug_annotation::NameField>,
}

pub(crate) mod debug_annotation {
    // perfetto-schema: oneof perfetto.protos.DebugAnnotation.value
    // Keep the generated-style variants aligned with the audited Perfetto field names.
    #[allow(clippy::enum_variant_names)]
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum Value {
        // perfetto-schema: field perfetto.protos.DebugAnnotation.bool_value
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        // perfetto-schema: field perfetto.protos.DebugAnnotation.uint_value
        #[prost(uint64, tag = "3")]
        UintValue(u64),
        // perfetto-schema: field perfetto.protos.DebugAnnotation.int_value
        #[prost(int64, tag = "4")]
        IntValue(i64),
        // perfetto-schema: field perfetto.protos.DebugAnnotation.double_value
        #[prost(double, tag = "5")]
        DoubleValue(f64),
        // perfetto-schema: field perfetto.protos.DebugAnnotation.string_value
        #[prost(string, tag = "6")]
        StringValue(String),
    }

    // perfetto-schema: oneof perfetto.protos.DebugAnnotation.name_field
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum NameField {
        // perfetto-schema: field perfetto.protos.DebugAnnotation.name
        #[prost(string, tag = "10")]
        Name(String),
    }
}

pub(crate) fn encode_trace_packet_fragment(
    packet: &TracePacket,
    output: &mut Vec<u8>,
) -> Result<(), prost::EncodeError> {
    prost::encoding::encode_key(1, prost::encoding::WireType::LengthDelimited, output);
    prost::encoding::encode_varint(packet.encoded_len() as u64, output);
    packet.encode(output)
}
