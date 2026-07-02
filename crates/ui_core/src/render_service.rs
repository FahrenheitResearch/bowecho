//! `render_service` — the CP-1 landing pad for Phase 4's render machinery
//! (docs/v029-engine-spec.md §4.2, §12a).
//!
//! **4a scope (step 1 — naming, zero behavior change):** lane naming
//! ([`LaneId`]), the per-lane drain budgets and stop rule ([`DrainBudget`]),
//! the post-drain repaint policy ([`post_drain_repaint`]), the one documented
//! routing-policy site ([`render_route_for`]), and the prewarm pool's channel
//! plumbing ([`PrewarmPool`]). The render workers themselves (they render
//! app-typed `RenderRequest`s) still live in `app_ui` and migrate here in
//! later Phase-4 slices; `app_ui` keeps thin typed adapters until then.
//!
//! Budget semantics pinned by the tests below (discovered from the three
//! pre-4a drains, deliberately NOT normalized):
//!
//! - **Primary** (+ extra panes riding the primary channel): 12 ms per pass.
//! - **Overlay**: 12 ms per pass, ONE clock shared across every overlay
//!   layer — a burst on one layer spills every later layer to the next frame.
//! - **Prewarm**: 8 ms per pass.
//! - The first message of a pass is ALWAYS accepted, however late the frame
//!   already is; the budget only stops the drain after at least one message.
//! - When the budget stops a drain mid-pass, the drain must request a repaint
//!   (messages may still be queued); the app-side drains do this inline.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// One render-result stream, as the drains see it (spec §4.2).
///
/// - `Primary` — the interactive primary view. Extra panes ride the SAME
///   channel today (`RenderRequest.pane != 0`); they become `Pane(n)` lanes
///   when the pane field migrates onto `LaneId` (Phase 4b+).
/// - `Pane(n)` — extra view pane `n` (1-based, matching today's nonzero
///   `pane` ids: pane `n` renders into `extra_panes[n - 1]`).
/// - `Overlay(id)` — one radar overlay layer. Until the 4b pool flip every
///   overlay layer still owns its worker + channel; the id keys per-lane
///   coalescing once the shared overlay pool lands.
/// - `Prewarm` — speculative loop-cache renders. Its drain is NOT unified
///   with the other lanes: results land in the loop render cache instead of
///   installing textures (migration table §6, `loop_prewarm_receiver` row).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LaneId {
    Primary,
    Pane(u8),
    Overlay(u64),
    Prewarm,
}

/// Interactive lanes (primary + panes) drain for up to 12 ms per frame:
/// each install does a ColorImage conversion + texture upload (~ms each),
/// so with several panes the drain is no longer one message deep.
const INTERACTIVE_DRAIN_BUDGET: Duration = Duration::from_millis(12);
/// All overlay lanes together share one 12 ms clock per pass.
const OVERLAY_DRAIN_BUDGET: Duration = Duration::from_millis(12);
/// Prewarm results only warm a cache; they yield the frame sooner.
const PREWARM_DRAIN_BUDGET: Duration = Duration::from_millis(8);

/// Per-lane drain budget. Values are pinned by tests; do NOT normalize them
/// without a measured reason — they reproduce the pre-4a drains exactly.
///
/// The budget for `Overlay(_)` is per PASS, not per layer: one clock is
/// shared by every overlay lane drained in a frame (the id is irrelevant).
pub fn lane_drain_budget(lane: LaneId) -> Duration {
    match lane {
        LaneId::Primary | LaneId::Pane(_) => INTERACTIVE_DRAIN_BUDGET,
        LaneId::Overlay(_) => OVERLAY_DRAIN_BUDGET,
        LaneId::Prewarm => PREWARM_DRAIN_BUDGET,
    }
}

/// The drain stop rule, as a pure function for tests: stop only after at
/// least one message has been seen AND the budget is strictly exceeded.
/// The first message of a pass is therefore always accepted.
pub fn drain_should_stop(saw_message: bool, elapsed: Duration, budget: Duration) -> bool {
    saw_message && elapsed > budget
}

/// One drain pass's budget clock. Start it before the `try_recv` loop, call
/// [`DrainBudget::note_message`] for every message taken (including a
/// `Disconnected` transition when the pre-4a drain counted one), and check
/// [`DrainBudget::should_stop`] at the top of each iteration.
pub struct DrainBudget {
    started: Instant,
    budget: Duration,
    saw_message: bool,
}

impl DrainBudget {
    /// Budget clock for `lane`, starting now.
    pub fn for_lane(lane: LaneId) -> Self {
        Self {
            started: Instant::now(),
            budget: lane_drain_budget(lane),
            saw_message: false,
        }
    }

    /// Record that the drain accepted one message this pass.
    pub fn note_message(&mut self) {
        self.saw_message = true;
    }

    /// True once at least one message was accepted this pass.
    pub fn saw_message(&self) -> bool {
        self.saw_message
    }

    /// [`drain_should_stop`] over this pass's clock. When this fires the
    /// drain must request a repaint before breaking — messages may remain.
    pub fn should_stop(&self) -> bool {
        drain_should_stop(self.saw_message, self.started.elapsed(), self.budget)
    }
}

/// What a drain schedules after its pass (see [`post_drain_repaint`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepaintDecision {
    /// Something arrived: repaint now.
    Now,
    /// Nothing arrived but a result is still expected: poll again soon
    /// (the app uses its `RENDER_RESULT_POLL_MS` cadence).
    PollSoon,
    /// Nothing arrived, nothing expected: stay idle.
    Idle,
}

/// Post-drain repaint policy, identical for every lane and pinned by tests:
/// repaint now if the pass accepted anything, otherwise schedule a short
/// follow-up poll only while a result is still awaited.
///
/// The prewarm drain passes `awaiting_result = false` — prewarm results are
/// opportunistic cache fills and never schedule a follow-up poll.
pub fn post_drain_repaint(saw_message: bool, awaiting_result: bool) -> RepaintDecision {
    if saw_message {
        RepaintDecision::Now
    } else if awaiting_result {
        RepaintDecision::PollSoon
    } else {
        RepaintDecision::Idle
    }
}

/// Which worker a lane's requests are routed to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderRoute {
    /// The one coalescing interactive worker (Primary cache mode). Kept a
    /// single worker so overlay bursts can never starve the primary view.
    Interactive,
    /// Until the Phase-4b pool flip, each overlay layer owns its own worker
    /// (Overlay cache mode); 4b replaces these with one shared overlay pool.
    OverlayOwned,
    /// The shared prewarm pool ([`PrewarmPool`], Overlay cache mode).
    Prewarm,
}

/// THE one documented render-routing policy site (spec §4.2 step 1). The
/// pre-4a routing was implicit in which sender each call site held; new code
/// consults this function, and the Phase-4b pool flip edits ONLY this map.
pub fn render_route_for(lane: LaneId) -> RenderRoute {
    match lane {
        LaneId::Primary | LaneId::Pane(_) => RenderRoute::Interactive,
        LaneId::Overlay(_) => RenderRoute::OverlayOwned,
        LaneId::Prewarm => RenderRoute::Prewarm,
    }
}

/// Prewarm worker count for a machine with `threads` effective worker
/// threads (moved verbatim from `app_ui`; also the K bound for the Phase-4b
/// overlay pool per spec §4.2).
pub fn loop_prewarm_worker_count(threads: usize) -> usize {
    match threads {
        0 | 1 => 1,
        2..=4 => 2,
        5..=8 => 3,
        9..=16 => 4,
        _ => 4,
    }
}

/// How many prewarm renders may be in flight at once (moved verbatim from
/// `app_ui`): two per worker, at least two.
pub fn loop_prewarm_inflight_limit(threads: usize) -> usize {
    loop_prewarm_worker_count(threads).saturating_mul(2).max(2)
}

/// N workers over ONE shared FIFO request queue (spec §4.2 `prewarm:
/// PrewarmPool`). Generic channel plumbing only — the per-request job (and
/// its worker-local caches) stays with the caller, built fresh on each
/// worker thread by the `make_job` factory.
///
/// Contract, identical to the hand-rolled pool this replaces:
/// - Requests are taken by whichever worker is idle; the queue lock is held
///   ACROSS the blocking `recv`, so exactly one worker waits on the queue at
///   a time and the others park on the lock.
/// - Workers exit when the pool (both channel ends) is dropped, or when the
///   queue lock is poisoned.
/// - Dropping the pool cancels cleanly: pending requests are dropped with
///   the queue; in-flight results fail their send and the worker exits.
pub struct PrewarmPool<Req, Res> {
    sender: mpsc::Sender<Req>,
    receiver: mpsc::Receiver<Res>,
}

impl<Req, Res> PrewarmPool<Req, Res>
where
    Req: Send + 'static,
    Res: Send + 'static,
{
    /// Spawn `worker_count` threads. `make_job` runs ON each worker thread
    /// to build that worker's job closure (worker-local caches live in the
    /// closure), which is then called once per request.
    pub fn spawn<Job, MakeJob>(worker_count: usize, make_job: MakeJob) -> Self
    where
        Job: FnMut(Req) -> Res,
        MakeJob: Fn() -> Job + Clone + Send + 'static,
    {
        let (request_sender, request_receiver) = mpsc::channel::<Req>();
        let (result_sender, result_receiver) = mpsc::channel::<Res>();
        let request_receiver = Arc::new(Mutex::new(request_receiver));

        for _ in 0..worker_count {
            let request_receiver = Arc::clone(&request_receiver);
            let result_sender = result_sender.clone();
            let make_job = make_job.clone();
            thread::spawn(move || {
                let mut job = make_job();
                loop {
                    let request = {
                        let Ok(receiver) = request_receiver.lock() else {
                            break;
                        };
                        receiver.recv()
                    };
                    let Ok(request) = request else {
                        break;
                    };
                    if result_sender.send(job(request)).is_err() {
                        break;
                    }
                }
            });
        }

        Self {
            sender: request_sender,
            receiver: result_receiver,
        }
    }
}

impl<Req, Res> PrewarmPool<Req, Res> {
    /// A pool with no workers whose channels are already closed: `send`
    /// fails and `try_recv` reports `Disconnected`. Test-harness stand-in
    /// matching the old dangling `mpsc::channel().0` / `.1` fields.
    pub fn disconnected() -> Self {
        Self {
            sender: mpsc::channel().0,
            receiver: mpsc::channel().1,
        }
    }

    /// Queue one request. `Err` when every worker has exited.
    pub fn send(&self, request: Req) -> Result<(), mpsc::SendError<Req>> {
        self.sender.send(request)
    }

    /// Non-blocking result poll, for the owner's per-frame drain.
    pub fn try_recv(&self) -> Result<Res, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_budgets_are_the_pre_4a_values() {
        // Pinned from poll_async_render (12 ms), poll_radar_layer_renders
        // (12 ms shared across layers), poll_loop_prewarm_renders (8 ms).
        assert_eq!(
            lane_drain_budget(LaneId::Primary),
            Duration::from_millis(12)
        );
        assert_eq!(
            lane_drain_budget(LaneId::Pane(3)),
            Duration::from_millis(12)
        );
        assert_eq!(
            lane_drain_budget(LaneId::Overlay(0)),
            Duration::from_millis(12)
        );
        assert_eq!(
            lane_drain_budget(LaneId::Overlay(u64::MAX)),
            Duration::from_millis(12)
        );
        assert_eq!(lane_drain_budget(LaneId::Prewarm), Duration::from_millis(8));
    }

    #[test]
    fn first_message_is_always_accepted() {
        // However late the frame is, a drain that has not yet accepted a
        // message keeps polling — the budget alone never stops it.
        let budget = Duration::from_millis(12);
        assert!(!drain_should_stop(false, Duration::from_secs(10), budget));
        assert!(!drain_should_stop(false, Duration::ZERO, budget));
    }

    #[test]
    fn budget_is_a_strict_bound_after_a_message() {
        let budget = Duration::from_millis(12);
        assert!(!drain_should_stop(true, Duration::from_millis(1), budget));
        // Exactly at the budget the drain keeps going (strict `>`).
        assert!(!drain_should_stop(true, budget, budget));
        assert!(drain_should_stop(
            true,
            budget + Duration::from_nanos(1),
            budget
        ));
    }

    #[test]
    fn fresh_drain_budget_never_stops_before_a_message() {
        let clock = DrainBudget::for_lane(LaneId::Prewarm);
        assert!(!clock.saw_message());
        assert!(!clock.should_stop());
    }

    #[test]
    fn post_drain_repaint_truth_table() {
        // Pinned from the three pre-4a drain tails: repaint now on any
        // message; otherwise poll again only while a result is awaited.
        assert_eq!(post_drain_repaint(true, true), RepaintDecision::Now);
        assert_eq!(post_drain_repaint(true, false), RepaintDecision::Now);
        assert_eq!(post_drain_repaint(false, true), RepaintDecision::PollSoon);
        assert_eq!(post_drain_repaint(false, false), RepaintDecision::Idle);
    }

    #[test]
    fn render_routes_match_pre_4b_ownership() {
        assert_eq!(render_route_for(LaneId::Primary), RenderRoute::Interactive);
        assert_eq!(render_route_for(LaneId::Pane(1)), RenderRoute::Interactive);
        assert_eq!(
            render_route_for(LaneId::Overlay(7)),
            RenderRoute::OverlayOwned
        );
        assert_eq!(render_route_for(LaneId::Prewarm), RenderRoute::Prewarm);
    }

    #[test]
    fn prewarm_worker_count_table() {
        assert_eq!(loop_prewarm_worker_count(0), 1);
        assert_eq!(loop_prewarm_worker_count(1), 1);
        assert_eq!(loop_prewarm_worker_count(2), 2);
        assert_eq!(loop_prewarm_worker_count(4), 2);
        assert_eq!(loop_prewarm_worker_count(5), 3);
        assert_eq!(loop_prewarm_worker_count(8), 3);
        assert_eq!(loop_prewarm_worker_count(9), 4);
        assert_eq!(loop_prewarm_worker_count(16), 4);
        assert_eq!(loop_prewarm_worker_count(32), 4);
    }

    #[test]
    fn prewarm_inflight_limit_is_two_per_worker_min_two() {
        assert_eq!(loop_prewarm_inflight_limit(0), 2);
        assert_eq!(loop_prewarm_inflight_limit(1), 2);
        assert_eq!(loop_prewarm_inflight_limit(8), 6);
        assert_eq!(loop_prewarm_inflight_limit(32), 8);
    }

    #[test]
    fn prewarm_pool_round_trips_requests() {
        let pool: PrewarmPool<u32, u32> = PrewarmPool::spawn(2, || |request: u32| request * 2);
        for value in 0..4u32 {
            pool.send(value).expect("workers alive");
        }
        let mut results = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while results.len() < 4 && Instant::now() < deadline {
            match pool.try_recv() {
                Ok(result) => results.push(result),
                Err(mpsc::TryRecvError::Empty) => thread::yield_now(),
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        results.sort_unstable();
        assert_eq!(results, vec![0, 2, 4, 6]);
    }

    #[test]
    fn disconnected_pool_fails_send_and_reports_disconnected() {
        let pool: PrewarmPool<u32, u32> = PrewarmPool::disconnected();
        assert!(pool.send(1).is_err());
        assert_eq!(pool.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }
}
