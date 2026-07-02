//! `WorkerSlot` / `StreamSlot` — the one grammar for the app's
//! `thread::spawn` + `Option<mpsc::Receiver<T>>` background-worker idiom
//! (docs/v029-engine-spec.md §4.1).
//!
//! Contract (identical to the hand-rolled slots this replaces):
//!
//! - **At most one in-flight job per slot.** [`WorkerSlot::spawn`] refuses
//!   (returns `false`) while a job is in flight.
//! - **Cancel = drop the receiver.** The job's next [`WorkerTx::send`]
//!   errors and it exits — IDENTICAL to today's `self.x_rx = None` contract
//!   (e.g. `cancel_extra_pane_load_for_user_command`), so cancellation
//!   semantics are preserved exactly.
//! - **Send = send + repaint.** Every delivered message requests a repaint,
//!   so results appear without waiting for the next natural frame.
//! - **Polling never blocks**, and every `Ready`/`Disconnected` transition
//!   clears the receiver — no half-drained states.
//! - **A job never writes app state or the global status bar** — it only
//!   sends `T`; status strings are chosen by the owner at drain time.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

/// The owning slot dropped its receiver (user cancelled / slot replaced):
/// the worker should stop — nobody is listening.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Cancelled;

/// The job's sending half: an `mpsc::Sender` paired with the egui context so
/// every delivered message also wakes the UI thread.
pub(crate) struct WorkerTx<T> {
    sender: mpsc::Sender<T>,
    ctx: egui::Context,
}

impl<T> WorkerTx<T> {
    /// Send one message and request a repaint. `Err(Cancelled)` when the
    /// owner dropped the receiver — the job should exit.
    ///
    /// The repaint is requested even when the send fails, matching the
    /// pre-slot workers (`let _ = sender.send(..); ctx.request_repaint();`);
    /// a stray repaint after cancellation is harmless.
    pub(crate) fn send(&self, value: T) -> Result<(), Cancelled> {
        let result = self.sender.send(value).map_err(|_| Cancelled);
        self.ctx.request_repaint();
        result
    }

    /// Raw sender, for §9-protected worker bodies whose signatures must stay
    /// untouched (the updater's `run_self_update_worker` throttles its own
    /// progress sends). New code sends through [`WorkerTx::send`].
    pub(crate) fn sender(&self) -> &mpsc::Sender<T> {
        &self.sender
    }

    /// Paired egui context — same escape hatch as [`WorkerTx::sender`].
    pub(crate) fn ctx(&self) -> &egui::Context {
        &self.ctx
    }
}

/// Non-blocking answer from [`WorkerSlot::poll`].
#[derive(Debug)]
pub(crate) enum SlotPoll<T> {
    /// No job in flight.
    Idle,
    /// Job in flight, no message yet.
    Pending,
    /// The job's one result. The slot is idle again.
    Ready(T),
    /// The worker vanished without sending (panic). The slot is idle again.
    Disconnected,
}

/// At most one in-flight one-shot job; see the module docs for the contract.
pub(crate) struct WorkerSlot<T> {
    rx: Option<mpsc::Receiver<T>>,
    started: Option<Instant>,
    /// Stable diagnostic name (the background-activity panel enumerates
    /// slots by label as they migrate onto this type).
    label: &'static str,
}

impl<T: Send + 'static> WorkerSlot<T> {
    pub(crate) fn idle(label: &'static str) -> Self {
        Self {
            rx: None,
            started: None,
            label,
        }
    }

    /// Start `job` on a background thread. Returns `false` (and spawns
    /// nothing) while a previous job is still in flight; callers that want
    /// replace-semantics call [`WorkerSlot::cancel`] first.
    pub(crate) fn spawn(
        &mut self,
        ctx: &egui::Context,
        job: impl FnOnce(WorkerTx<T>) + Send + 'static,
    ) -> bool {
        if self.rx.is_some() {
            return false;
        }
        let (sender, receiver) = mpsc::channel();
        self.rx = Some(receiver);
        self.started = Some(Instant::now());
        let tx = WorkerTx {
            sender,
            ctx: ctx.clone(),
        };
        thread::spawn(move || job(tx));
        true
    }

    /// Never blocks; `Ready`/`Disconnected` clear the slot.
    pub(crate) fn poll(&mut self) -> SlotPoll<T> {
        let Some(rx) = &self.rx else {
            return SlotPoll::Idle;
        };
        match rx.try_recv() {
            Ok(value) => {
                self.clear();
                SlotPoll::Ready(value)
            }
            Err(mpsc::TryRecvError::Empty) => SlotPoll::Pending,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.clear();
                SlotPoll::Disconnected
            }
        }
    }

    pub(crate) fn in_flight(&self) -> bool {
        self.rx.is_some()
    }

    /// Drop the receiver: the job's next send errors and it exits.
    pub(crate) fn cancel(&mut self) {
        self.clear();
    }

    /// Diagnostic name (see field docs). Enumerated by the
    /// background-activity panel as slots migrate onto this type.
    #[allow(dead_code)]
    pub(crate) fn label(&self) -> &'static str {
        self.label
    }

    /// When the in-flight job was spawned (diagnostics: "running for Ns").
    #[allow(dead_code)]
    pub(crate) fn started(&self) -> Option<Instant> {
        self.started
    }

    fn clear(&mut self) {
        self.rx = None;
        self.started = None;
    }

    /// Tests inject a hand-made receiver so drains can be driven without a
    /// real worker thread (deterministic, no scheduling races).
    #[cfg(test)]
    pub(crate) fn inject_for_test(&mut self, rx: mpsc::Receiver<T>) {
        self.rx = Some(rx);
        self.started = Some(Instant::now());
    }
}

/// Messages a [`StreamSlot`] job sends; the slot stays busy until it sees a
/// terminal one.
pub(crate) trait SlotMessage {
    fn is_terminal(&self) -> bool;
}

/// What a [`StreamSlot::drain`] left behind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StreamState {
    /// No job in flight.
    Idle,
    /// Job still in flight (channel empty, or the budget ran out first).
    Pending,
    /// A terminal message was drained. The slot is idle again.
    Finished,
    /// The worker vanished without a terminal message (panic). The slot is
    /// idle again; the owner decides how to report the truncated stream.
    Disconnected,
}

/// Streaming variant of [`WorkerSlot`]: the job sends any number of
/// messages and the slot stays busy until a terminal one (fits
/// `SelfUpdateEvent` progress streams, and later the `AsyncLoadResult` /
/// `IntlLoopFrameMessage` batches).
pub(crate) struct StreamSlot<T: SlotMessage> {
    rx: Option<mpsc::Receiver<T>>,
    started: Option<Instant>,
    label: &'static str,
}

impl<T: SlotMessage + Send + 'static> StreamSlot<T> {
    pub(crate) fn idle(label: &'static str) -> Self {
        Self {
            rx: None,
            started: None,
            label,
        }
    }

    /// Same contract as [`WorkerSlot::spawn`].
    pub(crate) fn spawn(
        &mut self,
        ctx: &egui::Context,
        job: impl FnOnce(WorkerTx<T>) + Send + 'static,
    ) -> bool {
        if self.rx.is_some() {
            return false;
        }
        let (sender, receiver) = mpsc::channel();
        self.rx = Some(receiver);
        self.started = Some(Instant::now());
        let tx = WorkerTx {
            sender,
            ctx: ctx.clone(),
        };
        thread::spawn(move || job(tx));
        true
    }

    /// Collect every already-delivered message (never blocks), stopping at a
    /// terminal message, a `budget` overrun, or an empty/disconnected
    /// channel. `Finished`/`Disconnected` clear the slot.
    pub(crate) fn drain(&mut self, budget: Duration) -> (Vec<T>, StreamState) {
        let Some(rx) = &self.rx else {
            return (Vec::new(), StreamState::Idle);
        };
        let start = Instant::now();
        let mut messages = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(message) => {
                    let terminal = message.is_terminal();
                    messages.push(message);
                    if terminal {
                        self.clear();
                        return (messages, StreamState::Finished);
                    }
                    if start.elapsed() >= budget {
                        return (messages, StreamState::Pending);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return (messages, StreamState::Pending),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.clear();
                    return (messages, StreamState::Disconnected);
                }
            }
        }
    }

    pub(crate) fn in_flight(&self) -> bool {
        self.rx.is_some()
    }

    /// Drop the receiver: the job's next send errors and it exits. (No
    /// production caller yet — the migrated self-update stream has no cancel
    /// affordance; kept so the type carries the full slot contract.)
    #[allow(dead_code)]
    pub(crate) fn cancel(&mut self) {
        self.clear();
    }

    /// Diagnostic name — see [`WorkerSlot::label`].
    #[allow(dead_code)]
    pub(crate) fn label(&self) -> &'static str {
        self.label
    }

    /// When the in-flight job was spawned — see [`WorkerSlot::started`].
    #[allow(dead_code)]
    pub(crate) fn started(&self) -> Option<Instant> {
        self.started
    }

    fn clear(&mut self) {
        self.rx = None;
        self.started = None;
    }

    /// See [`WorkerSlot::inject_for_test`].
    #[cfg(test)]
    pub(crate) fn inject_for_test(&mut self, rx: mpsc::Receiver<T>) {
        self.rx = Some(rx);
        self.started = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deadline-poll a slot until its one-shot result arrives; panics if the
    /// worker never delivers (test hang guard).
    fn poll_until_settled<T: Send + 'static>(slot: &mut WorkerSlot<T>) -> SlotPoll<T> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match slot.poll() {
                SlotPoll::Pending => {
                    assert!(Instant::now() < deadline, "worker never settled");
                    thread::yield_now();
                }
                settled => return settled,
            }
        }
    }

    #[test]
    fn spawn_while_in_flight_returns_false_and_spawns_nothing() {
        let ctx = egui::Context::default();
        let mut slot: WorkerSlot<u32> = WorkerSlot::idle("test");
        let (release, gate) = mpsc::channel::<()>();

        assert!(!slot.in_flight());
        assert!(slot.spawn(&ctx, move |tx| {
            let _ = gate.recv();
            let _ = tx.send(1);
        }));
        assert!(slot.in_flight());
        assert!(slot.started().is_some());
        assert_eq!(slot.label(), "test");

        // Second spawn refuses while the first job is in flight.
        assert!(!slot.spawn(&ctx, |tx| {
            let _ = tx.send(2);
        }));

        release.send(()).unwrap();
        match poll_until_settled(&mut slot) {
            SlotPoll::Ready(value) => assert_eq!(value, 1),
            other => panic!("want Ready(1), got {other:?}"),
        }
        // Ready cleared the slot.
        assert!(!slot.in_flight());
        assert!(slot.started().is_none());
        assert!(matches!(slot.poll(), SlotPoll::Idle));
    }

    #[test]
    fn cancel_then_worker_send_is_a_clean_no_op() {
        let ctx = egui::Context::default();
        let mut slot: WorkerSlot<u32> = WorkerSlot::idle("test");
        let (release, gate) = mpsc::channel::<()>();
        let (send_result_tx, send_result_rx) = mpsc::channel();

        assert!(slot.spawn(&ctx, move |tx| {
            let _ = gate.recv();
            let _ = send_result_tx.send(tx.send(7));
        }));
        // Cancel = drop the receiver; the job's next send errors and it
        // exits — today's `self.x_rx = None` contract.
        slot.cancel();
        assert!(!slot.in_flight());
        release.send(()).unwrap();
        assert_eq!(
            send_result_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("worker reports its send result"),
            Err(Cancelled)
        );
        // The cancelled worker's message never surfaces.
        assert!(matches!(slot.poll(), SlotPoll::Idle));
    }

    #[test]
    fn disconnected_clears_the_slot() {
        let mut slot: WorkerSlot<u32> = WorkerSlot::idle("test");
        let (sender, receiver) = mpsc::channel::<u32>();
        slot.inject_for_test(receiver);
        drop(sender); // worker vanished without sending (panic analogue)

        assert!(matches!(slot.poll(), SlotPoll::Disconnected));
        assert!(!slot.in_flight());
        assert!(slot.started().is_none());
        assert!(matches!(slot.poll(), SlotPoll::Idle));
    }

    #[derive(Debug, Eq, PartialEq)]
    enum TestMessage {
        Progress(u32),
        Done,
    }

    impl SlotMessage for TestMessage {
        fn is_terminal(&self) -> bool {
            matches!(self, TestMessage::Done)
        }
    }

    #[test]
    fn stream_slot_stays_busy_until_terminal_message() {
        let mut slot: StreamSlot<TestMessage> = StreamSlot::idle("test-stream");
        assert_eq!(slot.drain(Duration::MAX).1, StreamState::Idle);

        let (sender, receiver) = mpsc::channel();
        slot.inject_for_test(receiver);
        sender.send(TestMessage::Progress(1)).unwrap();
        sender.send(TestMessage::Progress(2)).unwrap();

        // Non-terminal messages drain but the slot stays busy.
        let (messages, state) = slot.drain(Duration::MAX);
        assert_eq!(
            messages,
            vec![TestMessage::Progress(1), TestMessage::Progress(2)]
        );
        assert_eq!(state, StreamState::Pending);
        assert!(slot.in_flight());

        // The terminal message finishes the stream and clears the slot.
        sender.send(TestMessage::Done).unwrap();
        let (messages, state) = slot.drain(Duration::MAX);
        assert_eq!(messages, vec![TestMessage::Done]);
        assert_eq!(state, StreamState::Finished);
        assert!(!slot.in_flight());
        assert_eq!(slot.drain(Duration::MAX).1, StreamState::Idle);
    }

    #[test]
    fn stream_slot_disconnect_without_terminal_reports_disconnected() {
        let mut slot: StreamSlot<TestMessage> = StreamSlot::idle("test-stream");
        let (sender, receiver) = mpsc::channel();
        slot.inject_for_test(receiver);
        sender.send(TestMessage::Progress(1)).unwrap();
        drop(sender);

        // Already-delivered messages still drain before the disconnect is
        // reported (matches the pre-slot loop-until-disconnected drains).
        let (messages, state) = slot.drain(Duration::MAX);
        assert_eq!(messages, vec![TestMessage::Progress(1)]);
        assert_eq!(state, StreamState::Disconnected);
        assert!(!slot.in_flight());
    }

    #[test]
    fn stream_slot_zero_budget_drains_at_most_one_message() {
        let mut slot: StreamSlot<TestMessage> = StreamSlot::idle("test-stream");
        let (sender, receiver) = mpsc::channel();
        slot.inject_for_test(receiver);
        sender.send(TestMessage::Progress(1)).unwrap();
        sender.send(TestMessage::Progress(2)).unwrap();

        let (messages, state) = slot.drain(Duration::ZERO);
        assert_eq!(messages, vec![TestMessage::Progress(1)]);
        assert_eq!(state, StreamState::Pending);
        assert!(slot.in_flight());
    }
}
