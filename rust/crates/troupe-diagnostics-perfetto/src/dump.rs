use std::{fmt, future::poll_fn, io, pin::Pin};

use prost::Message;
use tokio::io::AsyncWrite;
use troupe_diagnostics_core::event::DiagnosticEvent;
use troupe_diagnostics_core::scalar::SchemaU64;
use troupe_diagnostics_runtime::query::reader::{
    CapturedEventPage, CapturedEventSource, ReaderFailure,
};

use crate::{
    collect::{
        ProjectionCollector, ProjectionError, ProjectionLimits, ProjectionMetadata,
        StructuralIndexLimits, TRACE_METADATA_PREFIX,
    },
    schema::{
        BuiltinClock, TracePacket, encode_trace_packet_fragment, trace_packet, track_descriptor,
    },
};

const MAX_TRACE_PACKET_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum DumpError {
    Source(ReaderFailure),
    Projection(ProjectionError),
    Writer(io::Error),
    ResourceOverflow {
        metric: &'static str,
    },
    #[cfg(test)]
    TestFault(&'static str),
}

impl DumpError {
    pub const fn source_error(&self) -> Option<&ReaderFailure> {
        match self {
            Self::Source(error) => Some(error),
            _ => None,
        }
    }

    pub const fn projection_error(&self) -> Option<&ProjectionError> {
        match self {
            Self::Projection(error) => Some(error),
            _ => None,
        }
    }

    pub const fn writer_error(&self) -> Option<&io::Error> {
        match self {
            Self::Writer(error) => Some(error),
            _ => None,
        }
    }
}

impl fmt::Display for DumpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => fmt::Display::fmt(error, formatter),
            Self::Projection(error) => fmt::Display::fmt(error, formatter),
            Self::Writer(error) => write!(formatter, "Perfetto trace writer failed: {error}"),
            Self::ResourceOverflow { metric } => {
                write!(formatter, "Perfetto dump metric overflow: {metric}")
            }
            #[cfg(test)]
            Self::TestFault(detail) => write!(formatter, "injected Perfetto dump fault: {detail}"),
        }
    }
}

impl std::error::Error for DumpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Projection(error) => Some(error),
            Self::Writer(error) => Some(error),
            Self::ResourceOverflow { .. } => None,
            #[cfg(test)]
            Self::TestFault(_) => None,
        }
    }
}

impl From<ReaderFailure> for DumpError {
    fn from(error: ReaderFailure) -> Self {
        Self::Source(error)
    }
}

impl From<ProjectionError> for DumpError {
    fn from(error: ProjectionError) -> Self {
        Self::Projection(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumpSummary {
    captured_watermark: SchemaU64,
    exported_through: SchemaU64,
    event_count: u64,
    descriptor_count: u64,
    packet_count: u64,
    bytes_written: u64,
    source_page_reads: u64,
    peak_page_events: usize,
    peak_packet_bytes: usize,
    structural_index_entries: u64,
    structural_index_owned_payload_bytes: u64,
}

impl DumpSummary {
    pub const fn captured_watermark(self) -> SchemaU64 {
        self.captured_watermark
    }

    pub const fn exported_through(self) -> SchemaU64 {
        self.exported_through
    }

    pub const fn event_count(self) -> u64 {
        self.event_count
    }

    pub const fn descriptor_count(self) -> u64 {
        self.descriptor_count
    }

    pub const fn packet_count(self) -> u64 {
        self.packet_count
    }

    pub const fn event_packet_count(self) -> u64 {
        self.packet_count - self.descriptor_count
    }

    pub const fn bytes_written(self) -> u64 {
        self.bytes_written
    }

    pub const fn source_page_reads(self) -> u64 {
        self.source_page_reads
    }

    pub const fn peak_page_events(self) -> usize {
        self.peak_page_events
    }

    pub const fn peak_packet_bytes(self) -> usize {
        self.peak_packet_bytes
    }

    pub const fn structural_index_entries(self) -> u64 {
        self.structural_index_entries
    }

    pub const fn structural_index_owned_payload_bytes(self) -> u64 {
        self.structural_index_owned_payload_bytes
    }
}

#[derive(Default)]
struct DumpMetrics {
    source_page_reads: u64,
    peak_page_events: usize,
    descriptor_count: u64,
    packet_count: u64,
    bytes_written: u64,
    peak_packet_bytes: usize,
}

#[derive(Default)]
struct TimestampBounds {
    minimum: Option<u64>,
    maximum: Option<u64>,
    open_spans: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureBoundaryPosition {
    BeforeEvents,
    AfterEvents,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureBoundary {
    timestamp: u64,
    position: CaptureBoundaryPosition,
}

impl TimestampBounds {
    fn observe(&mut self, elapsed_ns: u64) {
        self.minimum = Some(
            self.minimum
                .map_or(elapsed_ns, |value| value.min(elapsed_ns)),
        );
        self.maximum = Some(
            self.maximum
                .map_or(elapsed_ns, |value| value.max(elapsed_ns)),
        );
    }

    fn observe_event(&mut self, event: &DiagnosticEvent) {
        self.observe(event.header().elapsed_ns().get());
        match event {
            DiagnosticEvent::SpanStarted(_) | DiagnosticEvent::CustomSpanStarted(_) => {
                self.open_spans += 1;
            }
            DiagnosticEvent::SpanFinished(_) | DiagnosticEvent::CustomSpanFinished(_) => {
                debug_assert!(
                    self.open_spans != 0,
                    "projection validated the finished span"
                );
                self.open_spans -= 1;
            }
            _ => {}
        }
    }

    fn capture_boundary(&self) -> Option<CaptureBoundary> {
        if self.open_spans == 0 {
            return None;
        }
        let timestamp = self
            .minimum
            .filter(|minimum| Some(*minimum) == self.maximum)?;
        Some(if timestamp < i64::MAX as u64 {
            CaptureBoundary {
                timestamp: timestamp + 1,
                position: CaptureBoundaryPosition::AfterEvents,
            }
        } else {
            CaptureBoundary {
                timestamp: timestamp - 1,
                position: CaptureBoundaryPosition::BeforeEvents,
            }
        })
    }
}

impl DumpMetrics {
    fn observe_page(&mut self, event_count: usize) -> Result<(), DumpError> {
        self.source_page_reads = checked_increment(self.source_page_reads, "source_page_reads")?;
        self.peak_page_events = self.peak_page_events.max(event_count);
        Ok(())
    }

    fn observe_packet(&mut self, encoded_bytes: usize) -> Result<(), DumpError> {
        self.packet_count = checked_increment(self.packet_count, "packet_count")?;
        self.peak_packet_bytes = self.peak_packet_bytes.max(encoded_bytes);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanPass {
    Preflight,
    Emit,
}

trait PrefixPage {
    fn len(&self) -> usize;
    fn sequence(&self, index: usize) -> SchemaU64;
    fn event(&self, index: usize) -> &DiagnosticEvent;
}

impl PrefixPage for CapturedEventPage {
    fn len(&self) -> usize {
        self.events().len()
    }

    fn sequence(&self, index: usize) -> SchemaU64 {
        self.events()[index].sequence()
    }

    fn event(&self, index: usize) -> &DiagnosticEvent {
        self.events()[index].event()
    }
}

trait PrefixSource {
    type Page: PrefixPage;

    fn read_event_page(&self, after: SchemaU64, pass: ScanPass) -> Result<Self::Page, DumpError>;
}

struct RuntimeCapturedSource<'source, 'connection>(&'source CapturedEventSource<'connection>);

impl PrefixSource for RuntimeCapturedSource<'_, '_> {
    type Page = CapturedEventPage;

    fn read_event_page(&self, after: SchemaU64, _pass: ScanPass) -> Result<Self::Page, DumpError> {
        self.0.read_event_page(after).map_err(DumpError::Source)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PacketPhase {
    Descriptor,
    Event,
}

trait PacketEncoder {
    fn encode(
        &mut self,
        phase: PacketPhase,
        packet: &TracePacket,
        buffer: &mut Vec<u8>,
    ) -> Result<(), ProjectionError>;
}

struct ProstPacketEncoder;

impl PacketEncoder for ProstPacketEncoder {
    fn encode(
        &mut self,
        _phase: PacketPhase,
        packet: &TracePacket,
        buffer: &mut Vec<u8>,
    ) -> Result<(), ProjectionError> {
        encode_trace_packet_fragment(packet, buffer)
            .map_err(|error| ProjectionError::ProtobufEncode(error.to_string()))
    }
}

pub async fn dump_captured_prefix<W>(
    source: &CapturedEventSource<'_>,
    writer: &mut W,
    through: Option<SchemaU64>,
) -> Result<DumpSummary, DumpError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    dump_captured_prefix_with_version(source, writer, through, env!("CARGO_PKG_VERSION")).await
}

pub async fn dump_captured_prefix_with_version<W>(
    source: &CapturedEventSource<'_>,
    writer: &mut W,
    through: Option<SchemaU64>,
    troupe_version: &str,
) -> Result<DumpSummary, DumpError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let captured_watermark = source.captured_watermark();
    let exported_through = through.unwrap_or(captured_watermark);
    let store_metadata = source.metadata();
    let projection_metadata = ProjectionMetadata::new(
        store_metadata.run_id(),
        captured_watermark,
        exported_through,
        troupe_version,
    )
    .with_completion(
        store_metadata.production_outcome().map(str::to_owned),
        store_metadata
            .ended_at()
            .is_some()
            .then_some(store_metadata.clean_shutdown()),
    );

    let source = RuntimeCapturedSource(source);
    let mut encoder = ProstPacketEncoder;
    dump_captured_prefix_core(
        &source,
        writer,
        captured_watermark,
        exported_through,
        projection_metadata,
        StructuralIndexLimits::FIXED,
        &mut encoder,
    )
    .await
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceBodyValidationError {
    code: &'static str,
    detail: String,
}

impl TraceBodyValidationError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for TraceBodyValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for TraceBodyValidationError {}

#[derive(Default)]
enum TraceBodyState {
    #[default]
    TopLevelKey,
    PacketLength,
    PacketPayload,
}

#[derive(Default)]
struct StreamingVarint {
    value: u64,
    shift: u32,
}

impl StreamingVarint {
    fn push(&mut self, byte: u8) -> Result<Option<u64>, TraceBodyValidationError> {
        if self.shift >= 64 || (self.shift == 63 && byte > 1) {
            return Err(invalid_trace_body("protobuf varint overflows u64"));
        }
        self.value |= u64::from(byte & 0x7f) << self.shift;
        if byte & 0x80 == 0 {
            let value = self.value;
            *self = Self::default();
            return Ok(Some(value));
        }
        self.shift += 7;
        if self.shift >= 64 {
            return Err(invalid_trace_body("protobuf varint is too long"));
        }
        Ok(None)
    }
}

/// Validates a streamed T03 trace without retaining the complete trace body.
/// The expected metadata is built from the response headers by the caller.
pub struct TraceBodyValidator {
    state: TraceBodyState,
    varint: StreamingVarint,
    packet_remaining: usize,
    packet: Vec<u8>,
    packet_count: u64,
    expected_metadata_track_name: String,
    metadata_descriptor_count: u8,
}

impl TraceBodyValidator {
    pub fn new(metadata: ProjectionMetadata) -> Self {
        Self {
            state: TraceBodyState::default(),
            varint: StreamingVarint::default(),
            packet_remaining: 0,
            packet: Vec::new(),
            packet_count: 0,
            expected_metadata_track_name: metadata.metadata_track_name(),
            metadata_descriptor_count: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), TraceBodyValidationError> {
        for &byte in bytes {
            match self.state {
                TraceBodyState::TopLevelKey => {
                    let Some(key) = self.varint.push(byte)? else {
                        continue;
                    };
                    if key >> 3 != 1 || key & 0x07 != 2 {
                        return Err(invalid_trace_body(
                            "trace body contains a field other than repeated Trace.packet",
                        ));
                    }
                    self.state = TraceBodyState::PacketLength;
                }
                TraceBodyState::PacketLength => {
                    let Some(length) = self.varint.push(byte)? else {
                        continue;
                    };
                    if length > MAX_TRACE_PACKET_BYTES {
                        return Err(TraceBodyValidationError::new(
                            "body_packet_too_large",
                            format!(
                                "trace packet has {length} bytes, limit is {MAX_TRACE_PACKET_BYTES}"
                            ),
                        ));
                    }
                    self.packet_remaining = usize::try_from(length).map_err(|_| {
                        invalid_trace_body("trace packet length does not fit in the client")
                    })?;
                    self.packet.clear();
                    self.packet
                        .try_reserve(self.packet_remaining)
                        .map_err(|_| {
                            TraceBodyValidationError::new(
                                "body_packet_allocation_failed",
                                "trace packet could not be buffered for validation",
                            )
                        })?;
                    if self.packet_remaining == 0 {
                        self.finish_packet()?;
                    } else {
                        self.state = TraceBodyState::PacketPayload;
                    }
                }
                TraceBodyState::PacketPayload => {
                    self.packet.push(byte);
                    self.packet_remaining -= 1;
                    if self.packet_remaining == 0 {
                        self.finish_packet()?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<(), TraceBodyValidationError> {
        if self.packet_count == 0 {
            return Err(invalid_trace_body("trace body contains no Trace.packet"));
        }
        if !matches!(self.state, TraceBodyState::TopLevelKey) || self.varint.shift != 0 {
            return Err(invalid_trace_body(
                "trace body ends in an incomplete protobuf field",
            ));
        }
        if self.metadata_descriptor_count != 1 {
            return Err(TraceBodyValidationError::new(
                "body_metadata_missing",
                "trace body does not contain its Troupe metadata descriptor",
            ));
        }
        Ok(())
    }

    fn finish_packet(&mut self) -> Result<(), TraceBodyValidationError> {
        let packet = TracePacket::decode(self.packet.as_slice()).map_err(|error| {
            invalid_trace_body(format!("trace packet protobuf is invalid: {error}"))
        })?;
        if !matches!(
            packet.optional_trusted_packet_sequence_id,
            Some(trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(1))
        ) {
            return Err(invalid_trace_body(
                "trace packet must use trusted_packet_sequence_id=1",
            ));
        }
        let data = packet
            .data
            .ok_or_else(|| invalid_trace_body("trace packet contains no descriptor or event"))?;
        match data {
            trace_packet::Data::TrackDescriptor(descriptor) => {
                if packet.timestamp.is_some() || packet.timestamp_clock_id.is_some() {
                    return Err(invalid_trace_body(
                        "trace descriptor packet must not carry a timestamp",
                    ));
                }
                if let Some(track_descriptor::StaticOrDynamicName::Name(name)) =
                    descriptor.static_or_dynamic_name
                    && name.starts_with(TRACE_METADATA_PREFIX)
                {
                    if name != self.expected_metadata_track_name {
                        return Err(TraceBodyValidationError::new(
                            "body_metadata_mismatch",
                            "trace Troupe metadata differs from the response headers",
                        ));
                    }
                    self.metadata_descriptor_count = self
                        .metadata_descriptor_count
                        .checked_add(1)
                        .ok_or_else(|| invalid_trace_body("trace metadata count overflows"))?;
                    if self.metadata_descriptor_count > 1 {
                        return Err(TraceBodyValidationError::new(
                            "body_metadata_duplicate",
                            "trace body contains more than one Troupe metadata descriptor",
                        ));
                    }
                }
            }
            trace_packet::Data::TrackEvent(_) => {
                if packet.timestamp.is_none()
                    || packet.timestamp_clock_id != Some(BuiltinClock::TraceFile as u32)
                {
                    return Err(invalid_trace_body(
                        "trace event packet must carry a timestamp with trace-file clock 11",
                    ));
                }
            }
        }
        self.packet_count = self
            .packet_count
            .checked_add(1)
            .ok_or_else(|| invalid_trace_body("trace packet count overflows u64"))?;
        self.state = TraceBodyState::TopLevelKey;
        Ok(())
    }
}

fn invalid_trace_body(detail: impl Into<String>) -> TraceBodyValidationError {
    TraceBodyValidationError::new("body_invalid", detail)
}

async fn dump_captured_prefix_core<S, W, E>(
    source: &S,
    writer: &mut W,
    captured_watermark: SchemaU64,
    exported_through: SchemaU64,
    projection_metadata: ProjectionMetadata,
    structural_limits: StructuralIndexLimits,
    encoder: &mut E,
) -> Result<DumpSummary, DumpError>
where
    S: PrefixSource,
    W: AsyncWrite + Unpin + ?Sized,
    E: PacketEncoder,
{
    // The collection pass fixes every export-local identity before any bytes are
    // emitted. The captured SQLite transaction makes both paged scans identical.
    let mut collector = ProjectionCollector::new_with_structural_limits(
        projection_metadata,
        ProjectionLimits::default(),
        structural_limits,
    )?;
    let mut metrics = DumpMetrics::default();
    let mut timestamp_bounds = TimestampBounds::default();
    scan_prefix(
        source,
        exported_through,
        ScanPass::Preflight,
        &mut metrics,
        |event| {
            collector.observe(event)?;
            timestamp_bounds.observe_event(event);
            Ok(())
        },
    )?;
    let plan = collector.finish()?;
    let structural_index_usage = plan.structural_index_usage();
    let capture_boundary = timestamp_bounds
        .capture_boundary()
        .map(|boundary| {
            plan.capture_boundary_packet(boundary.timestamp)
                .map(|packet| (boundary.position, packet))
        })
        .transpose()?;

    let mut packet_buffer = Vec::new();
    for packet in plan.descriptor_packets() {
        write_packet(
            writer,
            &packet,
            PacketPhase::Descriptor,
            encoder,
            &mut packet_buffer,
            &mut metrics,
        )
        .await?;
        metrics.descriptor_count = checked_increment(metrics.descriptor_count, "descriptor_count")?;
    }

    if let Some((CaptureBoundaryPosition::BeforeEvents, packet)) = capture_boundary.as_ref() {
        write_packet(
            writer,
            packet,
            PacketPhase::Event,
            encoder,
            &mut packet_buffer,
            &mut metrics,
        )
        .await?;
    }

    let mut projector = plan.packet_projector();
    let mut after = SchemaU64::new(0);
    while after.get() < exported_through.get() {
        let page = source.read_event_page(after, ScanPass::Emit)?;
        metrics.observe_page(page.len())?;
        let mut reached_through = false;
        for index in 0..page.len() {
            let sequence = page.sequence(index);
            if sequence.get() > exported_through.get() {
                reached_through = true;
                break;
            }
            for packet in projector.project_event(page.event(index))? {
                write_packet(
                    writer,
                    &packet,
                    PacketPhase::Event,
                    encoder,
                    &mut packet_buffer,
                    &mut metrics,
                )
                .await?;
            }
            after = sequence;
            if sequence == exported_through {
                reached_through = true;
                break;
            }
        }
        if reached_through || page.len() == 0 {
            break;
        }
    }
    projector.finish()?;
    if let Some((CaptureBoundaryPosition::AfterEvents, packet)) = capture_boundary.as_ref() {
        write_packet(
            writer,
            packet,
            PacketPhase::Event,
            encoder,
            &mut packet_buffer,
            &mut metrics,
        )
        .await?;
    }

    Ok(DumpSummary {
        captured_watermark,
        exported_through,
        event_count: exported_through.get(),
        descriptor_count: metrics.descriptor_count,
        packet_count: metrics.packet_count,
        bytes_written: metrics.bytes_written,
        source_page_reads: metrics.source_page_reads,
        peak_page_events: metrics.peak_page_events,
        peak_packet_bytes: metrics.peak_packet_bytes,
        structural_index_entries: structural_index_usage.entries(),
        structural_index_owned_payload_bytes: structural_index_usage.owned_payload_bytes(),
    })
}

fn scan_prefix<S>(
    source: &S,
    through: SchemaU64,
    pass: ScanPass,
    metrics: &mut DumpMetrics,
    mut observe: impl FnMut(&DiagnosticEvent) -> Result<(), ProjectionError>,
) -> Result<(), DumpError>
where
    S: PrefixSource,
{
    let mut after = SchemaU64::new(0);
    while after.get() < through.get() {
        let page = source.read_event_page(after, pass)?;
        metrics.observe_page(page.len())?;
        let mut reached_through = false;
        for index in 0..page.len() {
            let sequence = page.sequence(index);
            if sequence.get() > through.get() {
                reached_through = true;
                break;
            }
            observe(page.event(index))?;
            after = sequence;
            if sequence == through {
                reached_through = true;
                break;
            }
        }
        if reached_through || page.len() == 0 {
            break;
        }
    }
    Ok(())
}

async fn write_packet<W, E>(
    writer: &mut W,
    packet: &TracePacket,
    phase: PacketPhase,
    encoder: &mut E,
    buffer: &mut Vec<u8>,
    metrics: &mut DumpMetrics,
) -> Result<(), DumpError>
where
    W: AsyncWrite + Unpin + ?Sized,
    E: PacketEncoder,
{
    buffer.clear();
    encoder.encode(phase, packet, buffer)?;
    metrics.observe_packet(buffer.len())?;
    write_all_without_error_retry(writer, buffer, &mut metrics.bytes_written).await
}

async fn write_all_without_error_retry<W>(
    writer: &mut W,
    mut bytes: &[u8],
    bytes_written: &mut u64,
) -> Result<(), DumpError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    while !bytes.is_empty() {
        let written = poll_fn(|context| Pin::new(&mut *writer).poll_write(context, bytes))
            .await
            .map_err(DumpError::Writer)?;
        if written == 0 {
            return Err(DumpError::Writer(io::Error::new(
                io::ErrorKind::WriteZero,
                "writer accepted zero bytes for a non-empty Perfetto packet",
            )));
        }
        if written > bytes.len() {
            return Err(DumpError::Writer(io::Error::new(
                io::ErrorKind::InvalidData,
                "writer reported more bytes than were offered",
            )));
        }
        *bytes_written = bytes_written
            .checked_add(
                u64::try_from(written).map_err(|_| DumpError::ResourceOverflow {
                    metric: "bytes_written",
                })?,
            )
            .ok_or(DumpError::ResourceOverflow {
                metric: "bytes_written",
            })?;
        bytes = &bytes[written..];
    }
    Ok(())
}

fn checked_increment(value: u64, metric: &'static str) -> Result<u64, DumpError> {
    value
        .checked_add(1)
        .ok_or(DumpError::ResourceOverflow { metric })
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        pin::Pin,
        task::{Context, Poll},
    };

    use futures::executor::block_on;
    use troupe_diagnostics_core::{
        event::{CausalLink, CounterSampled, DiagnosticEventHeader, DiagnosticScope},
        id::CanonicalUuid,
        kinds::{CausalRelation, CounterKind},
        time::ElapsedNs,
    };

    use super::*;
    use crate::collect::{
        STRUCTURAL_INDEX_ENTRY_LIMIT, STRUCTURAL_INDEX_OWNED_PAYLOAD_LIMIT,
        StructuralIndexDimension,
    };

    const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";

    struct FakePage(Vec<DiagnosticEvent>);

    impl PrefixPage for FakePage {
        fn len(&self) -> usize {
            self.0.len()
        }

        fn sequence(&self, index: usize) -> SchemaU64 {
            self.0[index].header().sequence()
        }

        fn event(&self, index: usize) -> &DiagnosticEvent {
            &self.0[index]
        }
    }

    struct FakeSource {
        events: Vec<DiagnosticEvent>,
        page_size: usize,
        emit_reads: Cell<u64>,
        fail_emit_read: Option<u64>,
    }

    impl FakeSource {
        fn new(events: Vec<DiagnosticEvent>) -> Self {
            Self {
                events,
                page_size: 2,
                emit_reads: Cell::new(0),
                fail_emit_read: None,
            }
        }

        fn failing_emit_read(mut self, read: u64) -> Self {
            self.fail_emit_read = Some(read);
            self
        }
    }

    impl PrefixSource for FakeSource {
        type Page = FakePage;

        fn read_event_page(
            &self,
            after: SchemaU64,
            pass: ScanPass,
        ) -> Result<Self::Page, DumpError> {
            if pass == ScanPass::Emit {
                let read = self.emit_reads.get() + 1;
                self.emit_reads.set(read);
                if self.fail_emit_read == Some(read) {
                    return Err(DumpError::TestFault("second-pass source"));
                }
            }
            let start = usize::try_from(after.get())
                .map_err(|_| DumpError::TestFault("fake source cursor"))?;
            let end = start.saturating_add(self.page_size).min(self.events.len());
            Ok(FakePage(self.events[start..end].to_vec()))
        }
    }

    #[derive(Default)]
    struct CountingWriter {
        polls: u64,
        bytes: Vec<u8>,
    }

    impl AsyncWrite for CountingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.polls += 1;
            self.bytes.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct FailOnFirstEventEncoder {
        inner: ProstPacketEncoder,
        event_attempts: u64,
    }

    impl PacketEncoder for FailOnFirstEventEncoder {
        fn encode(
            &mut self,
            phase: PacketPhase,
            packet: &TracePacket,
            buffer: &mut Vec<u8>,
        ) -> Result<(), ProjectionError> {
            if phase == PacketPhase::Event {
                self.event_attempts += 1;
                return Err(ProjectionError::ProtobufEncode(
                    "injected second-pass encode failure".to_owned(),
                ));
            }
            self.inner.encode(phase, packet, buffer)
        }
    }

    fn run_id() -> CanonicalUuid {
        CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
    }

    fn metadata(watermark: u64) -> ProjectionMetadata {
        ProjectionMetadata::new(
            run_id(),
            SchemaU64::new(watermark),
            SchemaU64::new(watermark),
            env!("CARGO_PKG_VERSION"),
        )
    }

    #[test]
    fn capture_boundary_only_extends_an_open_zero_width_timeline() {
        let mut empty = TimestampBounds::default();
        assert_eq!(empty.capture_boundary(), None);

        empty.observe(10);
        assert_eq!(empty.capture_boundary(), None);
        empty.open_spans = 1;
        assert_eq!(
            empty.capture_boundary(),
            Some(CaptureBoundary {
                timestamp: 11,
                position: CaptureBoundaryPosition::AfterEvents,
            })
        );

        empty.observe(12);
        assert_eq!(empty.capture_boundary(), None);

        let mut maximum = TimestampBounds::default();
        maximum.observe(i64::MAX as u64);
        maximum.open_spans = 1;
        assert_eq!(
            maximum.capture_boundary(),
            Some(CaptureBoundary {
                timestamp: i64::MAX as u64 - 1,
                position: CaptureBoundaryPosition::BeforeEvents,
            })
        );
    }

    fn counter(sequence: u64) -> DiagnosticEvent {
        counter_with_causes(sequence, Vec::new())
    }

    fn counter_with_causes(sequence: u64, caused_by: Vec<CausalLink>) -> DiagnosticEvent {
        let header = DiagnosticEventHeader::new(
            run_id(),
            SchemaU64::new(sequence),
            ElapsedNs::new(sequence),
            DiagnosticScope::new(None, None, None, None, None, None, None),
            caused_by,
        )
        .expect("valid diagnostic event header");
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            header,
            CounterKind::DiagnosticDroppedEvents,
            SchemaU64::new(sequence),
        ))
    }

    #[test]
    fn deterministic_reference_failure_precedes_the_first_writer_poll() {
        let source = FakeSource::new(vec![counter_with_causes(
            1,
            vec![CausalLink::new(
                SchemaU64::new(2),
                CausalRelation::FollowsFrom,
            )],
        )]);
        let mut writer = CountingWriter::default();
        let mut encoder = ProstPacketEncoder;
        let error = block_on(dump_captured_prefix_core(
            &source,
            &mut writer,
            SchemaU64::new(1),
            SchemaU64::new(1),
            metadata(1),
            StructuralIndexLimits::new(128, 64 * 1024),
            &mut encoder,
        ))
        .unwrap_err();
        assert!(matches!(
            error.projection_error(),
            Some(ProjectionError::InvalidReference {
                code: "forward_link",
                sequence: 1,
                referenced_sequence: Some(2),
            })
        ));
        assert_eq!(writer.polls, 0);
    }

    #[test]
    fn fixed_structural_limits_accept_equal_and_reject_the_next_reservation_before_writer_poll() {
        assert_eq!(STRUCTURAL_INDEX_ENTRY_LIMIT, 1_000_000);
        assert_eq!(STRUCTURAL_INDEX_OWNED_PAYLOAD_LIMIT, 64 * 1024 * 1024);

        let source = FakeSource::new(Vec::new());
        let mut baseline_writer = CountingWriter::default();
        let mut baseline_encoder = ProstPacketEncoder;
        let baseline = block_on(dump_captured_prefix_core(
            &source,
            &mut baseline_writer,
            SchemaU64::new(0),
            SchemaU64::new(0),
            metadata(0),
            StructuralIndexLimits::new(128, 64 * 1024),
            &mut baseline_encoder,
        ))
        .expect("measure descriptor-only structural index");
        assert_eq!(baseline.structural_index_entries(), 6);
        assert!(baseline.structural_index_owned_payload_bytes() > 0);

        let exact_limits = StructuralIndexLimits::new(
            baseline.structural_index_entries(),
            baseline.structural_index_owned_payload_bytes(),
        );
        let mut exact_writer = CountingWriter::default();
        let mut exact_encoder = ProstPacketEncoder;
        block_on(dump_captured_prefix_core(
            &source,
            &mut exact_writer,
            SchemaU64::new(0),
            SchemaU64::new(0),
            metadata(0),
            exact_limits,
            &mut exact_encoder,
        ))
        .expect("equal structural limits are accepted");
        assert!(exact_writer.polls > 0);

        let entry_limit = baseline.structural_index_entries() - 1;
        let mut entry_writer = CountingWriter::default();
        let mut entry_encoder = ProstPacketEncoder;
        let error = block_on(dump_captured_prefix_core(
            &source,
            &mut entry_writer,
            SchemaU64::new(0),
            SchemaU64::new(0),
            metadata(0),
            StructuralIndexLimits::new(entry_limit, u64::MAX),
            &mut entry_encoder,
        ))
        .unwrap_err();
        let Some(ProjectionError::StructuralIndexLimitExceeded {
            dimension,
            limit,
            required,
        }) = error.projection_error()
        else {
            panic!("unexpected entry-limit error: {error}");
        };
        assert_eq!(*dimension, StructuralIndexDimension::Entries);
        assert_eq!(*limit, entry_limit);
        assert!(*required > *limit);
        assert_eq!(entry_writer.polls, 0);

        let payload_limit = baseline.structural_index_owned_payload_bytes() - 1;
        let mut payload_writer = CountingWriter::default();
        let mut payload_encoder = ProstPacketEncoder;
        let error = block_on(dump_captured_prefix_core(
            &source,
            &mut payload_writer,
            SchemaU64::new(0),
            SchemaU64::new(0),
            metadata(0),
            StructuralIndexLimits::new(u64::MAX, payload_limit),
            &mut payload_encoder,
        ))
        .unwrap_err();
        let Some(ProjectionError::StructuralIndexLimitExceeded {
            dimension,
            limit,
            required,
        }) = error.projection_error()
        else {
            panic!("unexpected payload-limit error: {error}");
        };
        assert_eq!(*dimension, StructuralIndexDimension::OwnedPayloadBytes);
        assert_eq!(*limit, payload_limit);
        assert!(*required > *limit);
        assert_eq!(payload_writer.polls, 0);
    }

    #[test]
    fn second_pass_source_and_encode_faults_preserve_partial_streams() {
        let source = FakeSource::new(vec![counter(1)]).failing_emit_read(1);
        let mut source_writer = CountingWriter::default();
        let mut source_encoder = ProstPacketEncoder;
        let error = block_on(dump_captured_prefix_core(
            &source,
            &mut source_writer,
            SchemaU64::new(1),
            SchemaU64::new(1),
            metadata(1),
            StructuralIndexLimits::new(128, 64 * 1024),
            &mut source_encoder,
        ))
        .unwrap_err();
        assert!(matches!(error, DumpError::TestFault("second-pass source")));
        assert_eq!(source.emit_reads.get(), 1);
        assert!(source_writer.polls > 0);
        assert!(!source_writer.bytes.is_empty());

        let source = FakeSource::new(vec![counter(1)]);
        let mut encode_writer = CountingWriter::default();
        let mut encode_encoder = FailOnFirstEventEncoder {
            inner: ProstPacketEncoder,
            event_attempts: 0,
        };
        let error = block_on(dump_captured_prefix_core(
            &source,
            &mut encode_writer,
            SchemaU64::new(1),
            SchemaU64::new(1),
            metadata(1),
            StructuralIndexLimits::new(128, 64 * 1024),
            &mut encode_encoder,
        ))
        .unwrap_err();
        assert!(matches!(
            error.projection_error(),
            Some(ProjectionError::ProtobufEncode(detail))
                if detail == "injected second-pass encode failure"
        ));
        assert_eq!(encode_encoder.event_attempts, 1);
        assert!(encode_writer.polls > 0);
        assert!(!encode_writer.bytes.is_empty());
    }
}
