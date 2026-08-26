//! `render_service` — the CP-1 landing pad for Phase 4's render machinery
//! (docs/v029-engine-spec.md §4.2, §12a).
//!
//! **4a scope (step 1 — naming, zero behavior change):** lane naming
//! ([`LaneId`]), the per-lane drain budgets and stop rule ([`DrainBudget`]),
//! the post-drain repaint policy ([`post_drain_repaint`]), the one documented
//! routing-policy site ([`render_route_for`]), and the prewarm pool's channel
//! plumbing ([`PrewarmPool`]).
//!
//! **4b scope (step 2 — the overlay pool flip, its own revertible commit):**
//! the shared overlay pool's channel plumbing ([`OverlayPool`], per-lane
//! coalescing via [`merge_lane_request`]) and its worker bound
//! ([`overlay_pool_worker_target`]). The render jobs themselves (they render
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

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// One render-result stream, as the drains see it (spec §4.2).
///
/// - `Primary` — the interactive primary view. Extra panes ride the SAME
///   channel (`RenderRequest.lane != Primary` routes to the pane installer);
///   the legacy `pane: usize` field became this lane in Phase 4b slice 1
///   (`Primary` = legacy pane 0, `Pane(n)` = legacy nonzero pane id `n`).
/// - `Pane(n)` — extra view pane `n` (1-based, matching today's nonzero
///   `pane` ids: pane `n` renders into `extra_panes[n - 1]`).
/// - `Overlay(id)` — one radar overlay layer. Since the 4b pool flip all
///   overlay lanes share one [`OverlayPool`]; the id keys the pool's
///   per-lane coalescing and routes results back to the layer.
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
/// The prewarm drain may pass `awaiting_result = true` while app-side request
/// accounting still owns a slot. That guarantees a completed stale request is
/// eventually drained and retired even if the UI becomes idle after changing
/// render context.
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
    /// Pre-4b routing: each overlay layer owned its own worker thread
    /// (Overlay cache mode). No lane routes here since the Phase-4b flip;
    /// the variant is kept as the documented revert target (see
    /// [`render_route_for`]).
    OverlayOwned,
    /// The shared overlay pool ([`OverlayPool`], Overlay cache mode):
    /// K workers over one per-lane-coalescing queue (Phase-4b flip).
    OverlayPool,
    /// The shared prewarm pool ([`PrewarmPool`], Overlay cache mode).
    Prewarm,
}

/// THE one documented render-routing policy site (spec §4.2 step 1). The
/// pre-4a routing was implicit in which sender each call site held; new code
/// consults this function, and the Phase-4b pool flip edited ONLY this map
/// (plus the app-side worker ownership the map describes).
///
/// REVERT PATH for the Phase-4b overlay flip (spec §12 decision 1 — the
/// flip is gated and individually revertible): `git revert` the single 4b
/// flip commit. That commit is the only place that (a) changed the
/// `Overlay` arm below from `RenderRoute::OverlayOwned` to
/// `RenderRoute::OverlayPool`, and (b) replaced the per-layer
/// `spawn_overlay_render_worker` fields on `RadarOverlayLayer` with the
/// shared [`OverlayPool`] in `app_ui`. Reverting it restores
/// thread-per-overlay exactly.
pub fn render_route_for(lane: LaneId) -> RenderRoute {
    match lane {
        LaneId::Primary | LaneId::Pane(_) => RenderRoute::Interactive,
        LaneId::Overlay(_) => RenderRoute::OverlayPool,
        LaneId::Prewarm => RenderRoute::Prewarm,
    }
}

/// Worker count for the shared overlay pool (spec §4.2): never more workers
/// than overlay layers, bounded by [`loop_prewarm_worker_count`] — the ONE
/// thread-count sizing table (deliberately not a new table). On 2-4 core
/// machines this caps overlay rendering at 2 threads where thread-per-layer
/// used to run up to 10 (the overlay layer cap), which is what keeps the
/// primary lane's worker and the UI thread from being starved at low core
/// counts.
pub fn overlay_pool_worker_target(overlay_layers: usize, threads: usize) -> usize {
    overlay_layers.min(loop_prewarm_worker_count(threads))
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

/// Merge a request into a per-lane-coalescing queue: a newer request
/// REPLACES the queued one for the SAME lane in place (keeping its queue
/// position), and never displaces another lane's request. This is exactly
/// `app_ui`'s `merge_render_request` discipline, lifted so the shared
/// overlay pool queue and the interactive worker queue obey one rule.
pub fn merge_lane_request<Req>(queue: &mut VecDeque<(LaneId, Req)>, lane: LaneId, request: Req) {
    if let Some(slot) = queue.iter_mut().find(|(queued, _)| *queued == lane) {
        slot.1 = request;
    } else {
        queue.push_back((lane, request));
    }
}

struct OverlayQueue<Req> {
    state: Mutex<OverlayQueueState<Req>>,
    ready: Condvar,
}

struct OverlayQueueState<Req> {
    queue: VecDeque<(LaneId, Req)>,
    shutdown: bool,
}

/// The pool has no live queue: the mutex was poisoned (a worker panicked
/// while holding it) or the pool is shutting down. The owner treats this
/// like the old per-layer "render worker disconnected" send failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlayPoolClosed;

/// The Phase-4b shared overlay render pool (spec §4.2 step 2): K workers
/// over ONE queue with PER-LANE coalescing ([`merge_lane_request`]).
///
/// Why not [`PrewarmPool`]: prewarm requests are all distinct keys throttled
/// by an in-flight limit, so its plain FIFO never needs to drop anything.
/// Overlay requests are the opposite — during a zoom/pan burst each layer
/// re-requests every frame, and a stale queued render must never win over a
/// newer one. Pre-4b, each layer's own serial worker gave it a single
/// newest-wins queue slot; the shared queue reproduces exactly that (one
/// slot per lane), so layer A's pending request is never starved or
/// replaced by layer B's, pinned by the tests below.
///
/// Contract:
/// - Workers pop front (FIFO across lanes), run the job OUTSIDE the queue
///   lock, and send the result; the owner drains results per frame.
/// - Workers spawn lazily via [`OverlayPool::ensure_workers`] and are never
///   torn down until the pool drops; the count only grows, bounded by the
///   caller ([`overlay_pool_worker_target`]).
/// - Results never report `Disconnected` while the pool is alive (the pool
///   keeps a sender for future workers); worker death therefore cannot be
///   observed on the result channel. The job must not panic — the app's
///   render job returns `Result` for every failure it knows about.
/// - Dropping the pool cancels cleanly: the shutdown flag + notify wakes
///   every idle worker to exit; in-flight results fail their send and that
///   worker exits.
pub struct OverlayPool<Req, Res> {
    queue: Arc<OverlayQueue<Req>>,
    result_sender: mpsc::Sender<Res>,
    result_receiver: mpsc::Receiver<Res>,
    workers: usize,
}

impl<Req, Res> OverlayPool<Req, Res>
where
    Req: Send + 'static,
    Res: Send + 'static,
{
    /// An empty pool with no workers. Submissions queue up (and coalesce)
    /// until [`OverlayPool::ensure_workers`] spawns someone to serve them.
    pub fn new() -> Self {
        let (result_sender, result_receiver) = mpsc::channel::<Res>();
        Self {
            queue: Arc::new(OverlayQueue {
                state: Mutex::new(OverlayQueueState {
                    queue: VecDeque::new(),
                    shutdown: false,
                }),
                ready: Condvar::new(),
            }),
            result_sender,
            result_receiver,
            workers: 0,
        }
    }

    /// Workers spawned so far (monotonic).
    pub fn worker_count(&self) -> usize {
        self.workers
    }

    /// Spawn workers until `target` are alive. `make_job` runs ON each new
    /// worker thread to build that worker's job closure (worker-local render
    /// caches live in the closure), which is then called once per request —
    /// the same factory shape as [`PrewarmPool::spawn`].
    pub fn ensure_workers<Job, MakeJob>(&mut self, target: usize, make_job: MakeJob)
    where
        Job: FnMut(Req) -> Res,
        MakeJob: Fn() -> Job + Clone + Send + 'static,
    {
        while self.workers < target {
            let queue = Arc::clone(&self.queue);
            let result_sender = self.result_sender.clone();
            let make_job = make_job.clone();
            thread::spawn(move || {
                let mut job = make_job();
                loop {
                    let request = {
                        let Ok(mut state) = queue.state.lock() else {
                            break;
                        };
                        loop {
                            if state.shutdown {
                                return;
                            }
                            if let Some((_, request)) = state.queue.pop_front() {
                                break request;
                            }
                            let Ok(next) = queue.ready.wait(state) else {
                                return;
                            };
                            state = next;
                        }
                    };
                    if result_sender.send(job(request)).is_err() {
                        break;
                    }
                }
            });
            self.workers += 1;
        }
    }

    /// Queue one request for `lane`, superseding the lane's queued older
    /// request if one is still waiting ([`merge_lane_request`]).
    pub fn submit(&self, lane: LaneId, request: Req) -> Result<(), OverlayPoolClosed> {
        let Ok(mut state) = self.queue.state.lock() else {
            return Err(OverlayPoolClosed);
        };
        if state.shutdown {
            return Err(OverlayPoolClosed);
        }
        merge_lane_request(&mut state.queue, lane, request);
        drop(state);
        self.queue.ready.notify_one();
        Ok(())
    }

    /// Non-blocking result poll, for the owner's per-frame drain. Never
    /// `Disconnected` while the pool is alive (see the type doc).
    pub fn try_recv(&self) -> Result<Res, mpsc::TryRecvError> {
        self.result_receiver.try_recv()
    }
}

impl<Req, Res> Default for OverlayPool<Req, Res>
where
    Req: Send + 'static,
    Res: Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<Req, Res> Drop for OverlayPool<Req, Res> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.queue.state.lock() {
            state.shutdown = true;
            state.queue.clear();
        }
        self.queue.ready.notify_all();
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
    fn render_routes_match_post_4b_ownership() {
        assert_eq!(render_route_for(LaneId::Primary), RenderRoute::Interactive);
        assert_eq!(render_route_for(LaneId::Pane(1)), RenderRoute::Interactive);
        // The Phase-4b flip: overlays share one pool. Reverting the flip
        // commit puts this back to RenderRoute::OverlayOwned.
        assert_eq!(
            render_route_for(LaneId::Overlay(7)),
            RenderRoute::OverlayPool
        );
        assert_eq!(render_route_for(LaneId::Prewarm), RenderRoute::Prewarm);
    }

    #[test]
    fn overlay_pool_worker_target_is_layers_capped_by_the_prewarm_table() {
        // Low-core machines (2-4 threads): at most 2 overlay workers, even
        // with a 4-overlay storm scene — thread-per-layer would have used 4.
        assert_eq!(overlay_pool_worker_target(4, 2), 2);
        assert_eq!(overlay_pool_worker_target(4, 4), 2);
        // Mid/high core counts follow the prewarm table (3 at 8, 4 at 16+).
        assert_eq!(overlay_pool_worker_target(4, 8), 3);
        assert_eq!(overlay_pool_worker_target(4, 16), 4);
        assert_eq!(overlay_pool_worker_target(10, 32), 4);
        // Never more workers than layers; zero layers need zero workers.
        assert_eq!(overlay_pool_worker_target(1, 16), 1);
        assert_eq!(overlay_pool_worker_target(0, 16), 0);
    }

    #[test]
    fn merge_lane_request_replaces_same_lane_in_place() {
        let mut queue: VecDeque<(LaneId, u32)> = VecDeque::new();
        merge_lane_request(&mut queue, LaneId::Overlay(1), 10);
        merge_lane_request(&mut queue, LaneId::Overlay(2), 20);
        // A newer request for lane 1 supersedes its queued older one and
        // keeps its position — lane 2 is neither displaced nor delayed.
        merge_lane_request(&mut queue, LaneId::Overlay(1), 11);
        assert_eq!(
            queue,
            VecDeque::from([(LaneId::Overlay(1), 11), (LaneId::Overlay(2), 20)])
        );
        // A third lane queues behind both.
        merge_lane_request(&mut queue, LaneId::Overlay(3), 30);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[2], (LaneId::Overlay(3), 30));
    }

    #[test]
    fn overlay_pool_coalesces_per_lane_while_no_worker_is_free() {
        // Submit before any worker exists: the queue must hold exactly one
        // slot per lane, newest request winning within a lane.
        let mut pool: OverlayPool<u32, (LaneId, u32)> = OverlayPool::new();
        pool.submit(LaneId::Overlay(1), 1).expect("pool alive");
        pool.submit(LaneId::Overlay(2), 2).expect("pool alive");
        pool.submit(LaneId::Overlay(1), 3).expect("pool alive");

        // One worker drains the queue in FIFO-across-lanes order.
        pool.ensure_workers(1, || {
            |request: u32| {
                let lane = if request == 2 {
                    LaneId::Overlay(2)
                } else {
                    LaneId::Overlay(1)
                };
                (lane, request)
            }
        });
        assert_eq!(pool.worker_count(), 1);

        let mut results = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while results.len() < 2 && Instant::now() < deadline {
            match pool.try_recv() {
                Ok(result) => results.push(result),
                Err(mpsc::TryRecvError::Empty) => thread::yield_now(),
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        // Lane 1's stale request (1) never rendered; its newer request (3)
        // kept lane 1's queue position ahead of lane 2's untouched request.
        assert_eq!(
            results,
            vec![(LaneId::Overlay(1), 3), (LaneId::Overlay(2), 2)]
        );
    }

    #[test]
    fn overlay_pool_round_trips_across_workers_and_drops_cleanly() {
        let mut pool: OverlayPool<u32, u32> = OverlayPool::new();
        pool.ensure_workers(2, || |request: u32| request * 2);
        pool.ensure_workers(2, || |request: u32| request * 2); // idempotent
        assert_eq!(pool.worker_count(), 2);
        for value in 0..4u32 {
            pool.submit(LaneId::Overlay(u64::from(value)), value)
                .expect("pool alive");
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
        // Dropping the pool wakes idle workers to exit; the test completing
        // without a hang is the assertion.
        drop(pool);
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
