use std::{
    collections::VecDeque,
    convert::Infallible,
    fmt,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures::task::AtomicWaker;
use hyper::body::{Body, Frame, SizeHint};
use tokio::sync::{Notify, watch};
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64};

use crate::store::watermark::{CommitNotification, CommitObserver};

use super::frame::{
    BUFFER_OVERFLOW_REASON, CURSOR_INCONSISTENT_REASON, CommittedEvent, FrameError, SseFrame,
    SseFrameKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriberLimits {
    max_buffered_events: usize,
    max_buffered_bytes: usize,
}

impl SubscriberLimits {
    pub fn new(
        max_buffered_events: usize,
        max_buffered_bytes: usize,
    ) -> Result<Self, SubscriberError> {
        if max_buffered_events == 0 || max_buffered_bytes == 0 {
            return Err(SubscriberError::InvalidLimits);
        }
        Ok(Self {
            max_buffered_events,
            max_buffered_bytes,
        })
    }

    pub const fn max_buffered_events(self) -> usize {
        self.max_buffered_events
    }

    pub const fn max_buffered_bytes(self) -> usize {
        self.max_buffered_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryStatus {
    Enqueued,
    Duplicate,
    DroppedControl,
    Overflowed,
    ResyncRequired,
    Closed,
}

#[derive(Debug)]
pub enum SubscriberError {
    InvalidLimits,
    InitialFrameExceedsBuffer,
    RunIdentityMismatch,
    WatermarkBehindCursor,
    Frame(FrameError),
}

impl fmt::Display for SubscriberError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("subscriber limits must be positive"),
            Self::InitialFrameExceedsBuffer => {
                formatter.write_str("stream_ready frame exceeds subscriber byte limit")
            }
            Self::RunIdentityMismatch => {
                formatter.write_str("committed event belongs to a different Run")
            }
            Self::WatermarkBehindCursor => {
                formatter.write_str("committed watermark is behind the subscriber cursor")
            }
            Self::Frame(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for SubscriberError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Frame(error) => Some(error),
            Self::InvalidLimits
            | Self::InitialFrameExceedsBuffer
            | Self::RunIdentityMismatch
            | Self::WatermarkBehindCursor => None,
        }
    }
}

impl From<FrameError> for SubscriberError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

pub fn open_subscriber(
    run_id: CanonicalUuid,
    resume_after: SchemaU64,
    replay_through: SchemaU64,
    limits: SubscriberLimits,
) -> Result<(SseSender, SseBody), SubscriberError> {
    let ready = SseFrame::stream_ready(run_id, resume_after, replay_through)?;
    let ready_bytes = ready.byte_len();
    if ready_bytes > limits.max_buffered_bytes {
        return Err(SubscriberError::InitialFrameExceedsBuffer);
    }
    let shared = Arc::new(SubscriberShared {
        state: Mutex::new(SubscriberState {
            queue: VecDeque::from([ready]),
            queued_event_count: 0,
            queued_bytes: ready_bytes,
            last_enqueued_sequence: resume_after,
            last_delivered_sequence: resume_after,
            first_frame_delivered: false,
            terminal: None,
            accepting: true,
            receiver_open: true,
        }),
        run_id,
        limits,
        reader_waker: AtomicWaker::new(),
        space_available: Notify::new(),
    });
    Ok((
        SseSender {
            shared: Arc::clone(&shared),
        },
        SseBody { shared },
    ))
}

pub fn resync_body(
    run_id: CanonicalUuid,
    reason: &str,
    committed_watermark: SchemaU64,
    earliest_available_sequence: Option<SchemaU64>,
) -> Result<SseBody, SubscriberError> {
    let terminal = SseFrame::resync_required(
        run_id,
        reason,
        committed_watermark,
        earliest_available_sequence,
    )?;
    Ok(SseBody {
        shared: Arc::new(SubscriberShared {
            state: Mutex::new(SubscriberState {
                queue: VecDeque::new(),
                queued_event_count: 0,
                queued_bytes: 0,
                last_enqueued_sequence: SchemaU64::new(0),
                last_delivered_sequence: SchemaU64::new(0),
                first_frame_delivered: false,
                terminal: Some(terminal),
                accepting: false,
                receiver_open: true,
            }),
            run_id,
            limits: SubscriberLimits {
                max_buffered_events: 1,
                max_buffered_bytes: 1,
            },
            reader_waker: AtomicWaker::new(),
            space_available: Notify::new(),
        }),
    })
}

struct SubscriberShared {
    state: Mutex<SubscriberState>,
    run_id: CanonicalUuid,
    limits: SubscriberLimits,
    reader_waker: AtomicWaker,
    space_available: Notify,
}

struct SubscriberState {
    queue: VecDeque<SseFrame>,
    queued_event_count: usize,
    queued_bytes: usize,
    last_enqueued_sequence: SchemaU64,
    last_delivered_sequence: SchemaU64,
    first_frame_delivered: bool,
    terminal: Option<SseFrame>,
    accepting: bool,
    receiver_open: bool,
}

#[derive(Clone)]
pub struct SseSender {
    shared: Arc<SubscriberShared>,
}

impl SseSender {
    pub fn try_send_event(
        &self,
        event: &CommittedEvent,
        committed_watermark: SchemaU64,
    ) -> Result<DeliveryStatus, SubscriberError> {
        self.send_event(event, committed_watermark, SendMode::DisconnectOnFull)
    }

    pub async fn send_replay_event(
        &self,
        event: &CommittedEvent,
        committed_watermark: SchemaU64,
    ) -> Result<DeliveryStatus, SubscriberError> {
        let frame = SseFrame::diagnostic_event(event);
        loop {
            let notified = self.shared.space_available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_enqueue_event(
                event,
                frame.clone(),
                committed_watermark,
                SendMode::Wait,
            )? {
                EnqueueAttempt::Complete(status) => return Ok(status),
                EnqueueAttempt::Wait => notified.as_mut().await,
            }
        }
    }

    pub fn try_send_heartbeat(
        &self,
        committed_watermark: SchemaU64,
    ) -> Result<DeliveryStatus, SubscriberError> {
        let frame = SseFrame::heartbeat(self.shared.run_id, committed_watermark)?;
        let mut state = lock(&self.shared.state);
        if !state.accepting || !state.receiver_open {
            return Ok(DeliveryStatus::Closed);
        }
        if committed_watermark.get() < state.last_enqueued_sequence.get() {
            return Err(SubscriberError::WatermarkBehindCursor);
        }
        if state
            .queue
            .iter()
            .any(|queued| queued.kind() == SseFrameKind::Heartbeat)
            || state
                .queued_bytes
                .checked_add(frame.byte_len())
                .is_none_or(|bytes| bytes > self.shared.limits.max_buffered_bytes)
        {
            return Ok(DeliveryStatus::DroppedControl);
        }
        state.queued_bytes += frame.byte_len();
        state.queue.push_back(frame);
        drop(state);
        self.shared.reader_waker.wake();
        Ok(DeliveryStatus::Enqueued)
    }

    pub fn close(
        &self,
        reason: &str,
        committed_watermark: SchemaU64,
    ) -> Result<DeliveryStatus, SubscriberError> {
        let terminal = SseFrame::stream_closed(self.shared.run_id, reason, committed_watermark)?;
        let mut state = lock(&self.shared.state);
        if committed_watermark.get() < state.last_enqueued_sequence.get() {
            return Err(SubscriberError::WatermarkBehindCursor);
        }
        if !state.receiver_open || !state.accepting {
            return Ok(DeliveryStatus::Closed);
        }
        state.accepting = false;
        state.terminal = Some(terminal);
        drop(state);
        self.shared.reader_waker.wake();
        self.shared.space_available.notify_waiters();
        Ok(DeliveryStatus::Enqueued)
    }

    pub fn resync_required(
        &self,
        reason: &str,
        committed_watermark: SchemaU64,
        earliest_available_sequence: Option<SchemaU64>,
    ) -> Result<DeliveryStatus, SubscriberError> {
        let terminal = SseFrame::resync_required(
            self.shared.run_id,
            reason,
            committed_watermark,
            earliest_available_sequence,
        )?;
        let mut state = lock(&self.shared.state);
        if !state.receiver_open || !state.accepting {
            return Ok(DeliveryStatus::Closed);
        }
        terminate_delivery(&mut state, terminal);
        drop(state);
        self.shared.reader_waker.wake();
        self.shared.space_available.notify_waiters();
        Ok(DeliveryStatus::ResyncRequired)
    }

    pub fn last_enqueued_sequence(&self) -> SchemaU64 {
        lock(&self.shared.state).last_enqueued_sequence
    }

    pub fn is_closed(&self) -> bool {
        !lock(&self.shared.state).accepting
    }

    fn send_event(
        &self,
        event: &CommittedEvent,
        committed_watermark: SchemaU64,
        mode: SendMode,
    ) -> Result<DeliveryStatus, SubscriberError> {
        let frame = SseFrame::diagnostic_event(event);
        match self.try_enqueue_event(event, frame, committed_watermark, mode)? {
            EnqueueAttempt::Complete(status) => Ok(status),
            EnqueueAttempt::Wait => unreachable!("nonblocking delivery cannot wait"),
        }
    }

    fn try_enqueue_event(
        &self,
        event: &CommittedEvent,
        frame: SseFrame,
        committed_watermark: SchemaU64,
        mode: SendMode,
    ) -> Result<EnqueueAttempt, SubscriberError> {
        if event.run_id() != self.shared.run_id {
            return Err(SubscriberError::RunIdentityMismatch);
        }
        let mut state = lock(&self.shared.state);
        if !state.accepting || !state.receiver_open {
            return Ok(EnqueueAttempt::Complete(DeliveryStatus::Closed));
        }
        let sequence = event.sequence();
        if committed_watermark.get() < sequence.get()
            || committed_watermark.get() < state.last_enqueued_sequence.get()
        {
            let current_head = SchemaU64::new(
                committed_watermark
                    .get()
                    .max(state.last_delivered_sequence.get()),
            );
            let terminal = SseFrame::resync_required(
                self.shared.run_id,
                CURSOR_INCONSISTENT_REASON,
                current_head,
                retained_earliest(current_head),
            )?;
            terminate_delivery(&mut state, terminal);
            drop(state);
            self.shared.reader_waker.wake();
            self.shared.space_available.notify_waiters();
            return Ok(EnqueueAttempt::Complete(DeliveryStatus::ResyncRequired));
        }
        if sequence.get() <= state.last_enqueued_sequence.get() {
            return Ok(EnqueueAttempt::Complete(DeliveryStatus::Duplicate));
        }
        let expected = state.last_enqueued_sequence.get().checked_add(1);
        if expected != Some(sequence.get()) {
            let current_head = SchemaU64::new(
                committed_watermark
                    .get()
                    .max(state.last_delivered_sequence.get()),
            );
            let terminal = SseFrame::resync_required(
                self.shared.run_id,
                CURSOR_INCONSISTENT_REASON,
                current_head,
                retained_earliest(current_head),
            )?;
            terminate_delivery(&mut state, terminal);
            drop(state);
            self.shared.reader_waker.wake();
            self.shared.space_available.notify_waiters();
            return Ok(EnqueueAttempt::Complete(DeliveryStatus::ResyncRequired));
        }

        let event_count = state.queued_event_count.checked_add(1);
        let byte_count = state.queued_bytes.checked_add(frame.byte_len());
        let full = event_count.is_none_or(|count| count > self.shared.limits.max_buffered_events)
            || byte_count.is_none_or(|bytes| bytes > self.shared.limits.max_buffered_bytes);
        if full {
            if mode == SendMode::Wait && frame.byte_len() <= self.shared.limits.max_buffered_bytes {
                return Ok(EnqueueAttempt::Wait);
            }
            let current_head = SchemaU64::new(
                committed_watermark
                    .get()
                    .max(state.last_delivered_sequence.get()),
            );
            let terminal = SseFrame::delivery_gap(
                self.shared.run_id,
                BUFFER_OVERFLOW_REASON,
                state.last_delivered_sequence,
                current_head,
            )?;
            terminate_delivery(&mut state, terminal);
            drop(state);
            self.shared.reader_waker.wake();
            self.shared.space_available.notify_waiters();
            return Ok(EnqueueAttempt::Complete(DeliveryStatus::Overflowed));
        }

        state.queued_event_count = event_count.expect("subscriber event capacity was checked");
        state.queued_bytes = byte_count.expect("subscriber byte capacity was checked");
        state.last_enqueued_sequence = sequence;
        state.queue.push_back(frame);
        drop(state);
        self.shared.reader_waker.wake();
        Ok(EnqueueAttempt::Complete(DeliveryStatus::Enqueued))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SendMode {
    DisconnectOnFull,
    Wait,
}

enum EnqueueAttempt {
    Complete(DeliveryStatus),
    Wait,
}

pub struct SseBody {
    shared: Arc<SubscriberShared>,
}

impl SseBody {
    pub fn last_delivered_sequence(&self) -> SchemaU64 {
        lock(&self.shared.state).last_delivered_sequence
    }

    pub fn pending_frame_count(&self) -> usize {
        let state = lock(&self.shared.state);
        state.queue.len() + usize::from(state.terminal.is_some())
    }
}

impl Body for SseBody {
    type Data = bytes::Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.shared.reader_waker.register(context.waker());
        let mut state = lock(&self.shared.state);
        if let Some(frame) = state.queue.pop_front() {
            state.first_frame_delivered = true;
            state.queued_bytes -= frame.byte_len();
            if frame.kind() == SseFrameKind::DiagnosticEvent {
                state.queued_event_count -= 1;
                state.last_delivered_sequence = frame
                    .id()
                    .expect("diagnostic event frames always carry an ID");
            }
            drop(state);
            self.shared.space_available.notify_waiters();
            return Poll::Ready(Some(Ok(frame.into_http_frame())));
        }
        if let Some(frame) = state.terminal.take() {
            drop(state);
            self.shared.space_available.notify_waiters();
            return Poll::Ready(Some(Ok(frame.into_http_frame())));
        }
        if !state.accepting || !state.receiver_open {
            return Poll::Ready(None);
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        let state = lock(&self.shared.state);
        state.queue.is_empty() && state.terminal.is_none() && !state.accepting
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

impl Drop for SseBody {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        state.receiver_open = false;
        state.accepting = false;
        state.queue.clear();
        state.terminal = None;
        state.queued_event_count = 0;
        state.queued_bytes = 0;
        drop(state);
        self.shared.space_available.notify_waiters();
    }
}

fn terminate_delivery(state: &mut SubscriberState, terminal: SseFrame) {
    let retained_ready = if !state.first_frame_delivered
        && state
            .queue
            .front()
            .is_some_and(|frame| frame.kind() == SseFrameKind::StreamReady)
    {
        state.queue.pop_front()
    } else {
        None
    };
    state.queue.clear();
    state.queued_event_count = 0;
    state.queued_bytes = 0;
    if let Some(ready) = retained_ready {
        state.queued_bytes = ready.byte_len();
        state.queue.push_back(ready);
    }
    state.terminal = Some(terminal);
    state.accepting = false;
}

const fn retained_earliest(head: SchemaU64) -> Option<SchemaU64> {
    if head.get() == 0 {
        None
    } else {
        Some(SchemaU64::new(1))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitTailStatus {
    Open,
    Invalid { reason: String },
    Closed { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitTailState {
    run_id: CanonicalUuid,
    committed_head: SchemaU64,
    status: CommitTailStatus,
}

impl CommitTailState {
    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn committed_head(&self) -> SchemaU64 {
        self.committed_head
    }

    pub const fn status(&self) -> &CommitTailStatus {
        &self.status
    }
}

#[derive(Clone)]
pub struct CommitSignal {
    shared: Arc<CommitSignalShared>,
}

struct CommitSignalShared {
    state: Mutex<CommitTailState>,
    sender: watch::Sender<CommitTailState>,
}

impl CommitSignal {
    pub fn new(run_id: CanonicalUuid, committed_head: SchemaU64) -> Self {
        let initial = CommitTailState {
            run_id,
            committed_head,
            status: CommitTailStatus::Open,
        };
        let (sender, _receiver) = watch::channel(initial.clone());
        Self {
            shared: Arc::new(CommitSignalShared {
                state: Mutex::new(initial),
                sender,
            }),
        }
    }

    pub fn subscribe(&self) -> CommitListener {
        CommitListener {
            receiver: self.shared.sender.subscribe(),
        }
    }

    pub fn state(&self) -> CommitTailState {
        lock(&self.shared.state).clone()
    }

    pub fn advance(
        &self,
        run_id: CanonicalUuid,
        previous: SchemaU64,
        committed: SchemaU64,
    ) -> Result<(), CommitSignalError> {
        let mut state = lock(&self.shared.state);
        let result = validate_advance(&state, run_id, previous, committed);
        if let Err(error) = result {
            latch_invalid(&self.shared, &mut state, error.reason());
            return Err(error);
        }
        state.committed_head = committed;
        let snapshot = state.clone();
        self.shared.sender.send_replace(snapshot);
        Ok(())
    }

    pub fn close(
        &self,
        reason: impl Into<String>,
        final_committed_head: SchemaU64,
    ) -> Result<(), CommitSignalError> {
        let reason = reason.into();
        let mut state = lock(&self.shared.state);
        if reason.is_empty() {
            return Err(CommitSignalError::new(CommitSignalErrorKind::EmptyReason));
        }
        if !matches!(state.status, CommitTailStatus::Open) {
            return Err(CommitSignalError::new(CommitSignalErrorKind::NotOpen));
        }
        if final_committed_head != state.committed_head {
            return Err(CommitSignalError::new(
                CommitSignalErrorKind::FinalWatermarkMismatch,
            ));
        }
        state.committed_head = final_committed_head;
        state.status = CommitTailStatus::Closed { reason };
        let snapshot = state.clone();
        self.shared.sender.send_replace(snapshot);
        Ok(())
    }

    fn observe_notification(
        &self,
        notification: CommitNotification,
    ) -> Result<(), CommitSignalError> {
        let previous = SchemaU64::new(notification.previous().get());
        let committed = SchemaU64::new(notification.committed().get());
        let actual_count = committed
            .get()
            .checked_sub(previous.get())
            .and_then(|count| usize::try_from(count).ok());
        if actual_count != Some(notification.event_count()) {
            let mut state = lock(&self.shared.state);
            let error = CommitSignalError::new(CommitSignalErrorKind::NonDenseNotification);
            latch_invalid(&self.shared, &mut state, error.reason());
            return Err(error);
        }
        self.advance(notification.run_id(), previous, committed)
    }
}

impl CommitObserver for CommitSignal {
    fn committed(&mut self, notification: CommitNotification) {
        let _ = self.observe_notification(notification);
    }
}

pub struct CommitListener {
    receiver: watch::Receiver<CommitTailState>,
}

impl CommitListener {
    pub fn current(&self) -> CommitTailState {
        self.receiver.borrow().clone()
    }

    pub async fn changed(&mut self) -> CommitTailState {
        if self.receiver.changed().await.is_err() {
            let current = self.receiver.borrow().clone();
            return CommitTailState {
                status: CommitTailStatus::Invalid {
                    reason: "commit_signal_closed".to_owned(),
                },
                ..current
            };
        }
        self.current()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitSignalErrorKind {
    RunIdentityMismatch,
    StalePrevious,
    NonIncreasingCommit,
    NonDenseNotification,
    FinalWatermarkMismatch,
    EmptyReason,
    NotOpen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitSignalError {
    kind: CommitSignalErrorKind,
}

impl CommitSignalError {
    const fn new(kind: CommitSignalErrorKind) -> Self {
        Self { kind }
    }

    pub const fn kind(self) -> CommitSignalErrorKind {
        self.kind
    }

    pub const fn reason(self) -> &'static str {
        match self.kind {
            CommitSignalErrorKind::RunIdentityMismatch => "commit_run_identity_mismatch",
            CommitSignalErrorKind::StalePrevious => "commit_previous_watermark_mismatch",
            CommitSignalErrorKind::NonIncreasingCommit => "commit_watermark_not_increasing",
            CommitSignalErrorKind::NonDenseNotification => "commit_notification_not_dense",
            CommitSignalErrorKind::FinalWatermarkMismatch => "final_commit_watermark_mismatch",
            CommitSignalErrorKind::EmptyReason => "stream_close_reason_empty",
            CommitSignalErrorKind::NotOpen => "commit_signal_not_open",
        }
    }
}

impl fmt::Display for CommitSignalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.reason())
    }
}

impl std::error::Error for CommitSignalError {}

fn validate_advance(
    state: &CommitTailState,
    run_id: CanonicalUuid,
    previous: SchemaU64,
    committed: SchemaU64,
) -> Result<(), CommitSignalError> {
    if !matches!(state.status, CommitTailStatus::Open) {
        return Err(CommitSignalError::new(CommitSignalErrorKind::NotOpen));
    }
    if run_id != state.run_id {
        return Err(CommitSignalError::new(
            CommitSignalErrorKind::RunIdentityMismatch,
        ));
    }
    if previous != state.committed_head {
        return Err(CommitSignalError::new(CommitSignalErrorKind::StalePrevious));
    }
    if committed.get() <= previous.get() {
        return Err(CommitSignalError::new(
            CommitSignalErrorKind::NonIncreasingCommit,
        ));
    }
    Ok(())
}

fn latch_invalid(shared: &CommitSignalShared, state: &mut CommitTailState, reason: &str) {
    if !matches!(state.status, CommitTailStatus::Invalid { .. }) {
        state.status = CommitTailStatus::Invalid {
            reason: reason.to_owned(),
        };
        shared.sender.send_replace(state.clone());
    }
}
