use std::{fmt, future::poll_fn, io, pin::Pin};

use tokio::io::AsyncWrite;
use troupe_diagnostics_core::scalar::SchemaU64;
use troupe_diagnostics_runtime::query::reader::{CapturedEventSource, ReaderFailure};

use crate::{
    collect::{ProjectionCollector, ProjectionError, ProjectionLimits, ProjectionMetadata},
    schema::{TracePacket, encode_trace_packet_fragment},
};

#[derive(Debug)]
pub enum DumpError {
    Source(ReaderFailure),
    Projection(ProjectionError),
    Writer(io::Error),
    ResourceOverflow { metric: &'static str },
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

pub async fn dump_captured_prefix<W>(
    source: &CapturedEventSource<'_>,
    writer: &mut W,
    through: Option<SchemaU64>,
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
        env!("CARGO_PKG_VERSION"),
    )
    .with_completion(
        store_metadata.production_outcome().map(str::to_owned),
        store_metadata
            .ended_at()
            .is_some()
            .then_some(store_metadata.clean_shutdown()),
    );

    // The collection pass fixes every export-local identity before any bytes are
    // emitted. The captured SQLite transaction makes both paged scans identical.
    let mut collector = ProjectionCollector::new(projection_metadata, ProjectionLimits::default())?;
    let mut metrics = DumpMetrics::default();
    scan_prefix(source, exported_through, &mut metrics, |event| {
        collector.observe(event)
    })?;
    let plan = collector.finish()?;

    let mut packet_buffer = Vec::new();
    for packet in plan.descriptor_packets() {
        write_packet(writer, &packet, &mut packet_buffer, &mut metrics).await?;
        metrics.descriptor_count = checked_increment(metrics.descriptor_count, "descriptor_count")?;
    }

    let mut projector = plan.packet_projector();
    let mut after = SchemaU64::new(0);
    while after.get() < exported_through.get() {
        let page = source.read_event_page(after)?;
        metrics.observe_page(page.events().len())?;
        let mut reached_through = false;
        for captured in page.events() {
            let sequence = captured.sequence();
            if sequence.get() > exported_through.get() {
                reached_through = true;
                break;
            }
            for packet in projector.project_event(captured.event())? {
                write_packet(writer, &packet, &mut packet_buffer, &mut metrics).await?;
            }
            after = sequence;
            if sequence == exported_through {
                reached_through = true;
                break;
            }
        }
        if reached_through || page.events().is_empty() {
            break;
        }
    }
    projector.finish()?;

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
    })
}

fn scan_prefix(
    source: &CapturedEventSource<'_>,
    through: SchemaU64,
    metrics: &mut DumpMetrics,
    mut observe: impl FnMut(
        &troupe_diagnostics_core::event::DiagnosticEvent,
    ) -> Result<(), ProjectionError>,
) -> Result<(), DumpError> {
    let mut after = SchemaU64::new(0);
    while after.get() < through.get() {
        let page = source.read_event_page(after)?;
        metrics.observe_page(page.events().len())?;
        let mut reached_through = false;
        for captured in page.events() {
            let sequence = captured.sequence();
            if sequence.get() > through.get() {
                reached_through = true;
                break;
            }
            observe(captured.event())?;
            after = sequence;
            if sequence == through {
                reached_through = true;
                break;
            }
        }
        if reached_through || page.events().is_empty() {
            break;
        }
    }
    Ok(())
}

async fn write_packet<W>(
    writer: &mut W,
    packet: &TracePacket,
    buffer: &mut Vec<u8>,
    metrics: &mut DumpMetrics,
) -> Result<(), DumpError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    buffer.clear();
    encode_trace_packet_fragment(packet, buffer)
        .map_err(|error| ProjectionError::ProtobufEncode(error.to_string()))?;
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
