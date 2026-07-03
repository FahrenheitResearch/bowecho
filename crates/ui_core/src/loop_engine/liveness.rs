//! Engine-side liveness derivation (spec §3): ONE derivation, from
//! engine-own state only — feed variant + live-enabled + newest frame age.
//! This is the Phase-4c SKELETON: the full chip truth table (every (role,
//! feed, live, age) cell, replacing `mode_chip_state*` and
//! `pane_chip_is_live`) is the whole-phase gate and derives from this at
//! adoption time.

use std::time::Duration;

use chrono::{DateTime, Utc};
use data_source::sites::SiteRef;

use super::feed::FeedSource;
use super::{EngineRole, LoopEngine};

/// Stale-threshold floor for a live international feed, moved from app_ui
/// (which re-imports it): intl feeds publish on 5-15 minute cadences (vs
/// the 480 s NEXRAD-tuned default), so a live intl chip flags STALE only
/// past twice the slowest routine cadence — a normal publish gap must not
/// read as a fault.
pub const INTL_STALE_CHIP_FLOOR_SECONDS: i64 = 1800;

/// International catalog-poll cadence, moved from app_ui (which re-imports
/// it). National open-data feeds publish on 5-minute-ish schedules and
/// each tick costs an upstream API round trip — 60 s, not the custom URL
/// poll's 15 s dir.list cadence.
pub const INTL_POLL_SECONDS: u64 = 60;

/// Pane/overlay US realtime refresh cadence, moved from app_ui (which
/// re-imports it): secondary views poll the chunk chain at 5 s, not the
/// primary's 1 s.
pub const OVERLAY_REALTIME_LEVEL2_REFRESH_SECONDS: u64 = 5;

/// The primary US chunk-chain cadence. Documents the value of app_ui's
/// `PRIMARY_REALTIME_LEVEL2_REFRESH_SECONDS`, which deliberately stays in
/// main.rs: the US 1 s chunk-poll chain is byte-identical through Phase 4
/// (spec §9) and this engine constant must not become its source until
/// Phase 4e re-points it under the differential gate.
pub const PRIMARY_LIVE_CHUNK_POLL_SECONDS: u64 = 1;

/// The custom-URL dir.list poll cadence. Documents the inline
/// `Duration::from_secs(15)` in app_ui's `poll_custom_url` (same Phase-4e
/// re-point rule as [`PRIMARY_LIVE_CHUNK_POLL_SECONDS`]).
pub const CUSTOM_URL_POLL_SECONDS: u64 = 15;

/// What the mode chip derives from (spec §3): a feed is either following
/// live data (possibly stale) or showing a fixed record. Invariant: an
/// `Archive`/`LocalFiles` feed or a disabled live feed can NEVER produce
/// `Live` — the R8 bug class (live chips on archive displays) is
/// unrepresentable from this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Liveness {
    /// The feed follows live data. `stale` = the newest frame's age passed
    /// the effective threshold (`max(user, floor)`, see
    /// [`stale_floor_seconds`]).
    Live { age_seconds: i64, stale: bool },
    /// A fixed display: archive window, local files, or a paused live
    /// feed.
    Archive { age_seconds: i64 },
}

/// The cadence-aware stale floor (spec §12 owner decision 3):
/// `max(user, 1800 s, 2 × poll cadence)` for live international feeds;
/// 0 for everything else (US behavior unchanged — the user threshold
/// alone governs). Pure so the table test below is the contract.
pub fn stale_floor_seconds(feed: &FeedSource, poll_cadence: Option<Duration>) -> i64 {
    match feed {
        FeedSource::Live(SiteRef::Intl { .. }) => {
            let cadence_floor = poll_cadence
                .map(|cadence| 2 * cadence.as_secs() as i64)
                .unwrap_or(0);
            INTL_STALE_CHIP_FLOOR_SECONDS.max(cadence_floor)
        }
        FeedSource::Live(SiteRef::Us { .. })
        | FeedSource::CustomUrl(_)
        | FeedSource::Archive { .. }
        | FeedSource::LocalFiles { .. } => 0,
    }
}

impl LoopEngine {
    /// Poll cadence for this engine's (role, feed) cell (spec §3).
    /// `None` = this feed never polls (archive windows and local files are
    /// fixed records — structurally incapable of refresh).
    pub fn poll_cadence(&self) -> Option<Duration> {
        let seconds = match (&self.role, &self.feed) {
            (_, FeedSource::Archive { .. } | FeedSource::LocalFiles { .. }) => return None,
            (_, FeedSource::Live(SiteRef::Intl { .. })) => INTL_POLL_SECONDS,
            (EngineRole::Primary, FeedSource::Live(SiteRef::Us { .. })) => {
                PRIMARY_LIVE_CHUNK_POLL_SECONDS
            }
            (
                EngineRole::Pane { .. } | EngineRole::Overlay,
                FeedSource::Live(SiteRef::Us { .. }),
            ) => OVERLAY_REALTIME_LEVEL2_REFRESH_SECONDS,
            (_, FeedSource::CustomUrl(_)) => CUSTOM_URL_POLL_SECONDS,
        };
        Some(Duration::from_secs(seconds))
    }

    /// Liveness derived ONLY from engine-own state: feed variant,
    /// `live.enabled`, and the NEWEST history frame's age (histories are
    /// kept ascending by identity, so newest = last). `None` when the
    /// history is empty — there is nothing to be live or stale about.
    ///
    /// `user_stale_chip_seconds` is the user's stale threshold (style
    /// registry state, caller-owned); the effective threshold is
    /// `max(user.max(0), stale_floor_seconds(feed, cadence))`, exactly the
    /// legacy `mode_chip_state_with_live_and_stale_floor` clamp. Read-only:
    /// asserts the generation did not move (spec §3 harness).
    pub fn liveness(&self, now: DateTime<Utc>, user_stale_chip_seconds: i64) -> Option<Liveness> {
        let generation_before = self.history.generation();
        let newest = self.history.last()?;
        let age_seconds = now
            .signed_duration_since(newest.identity.scan_time_utc)
            .num_seconds()
            .max(0);
        let live_now = self.feed.is_live_variant() && self.live.enabled;
        let result = if live_now {
            let threshold = user_stale_chip_seconds
                .max(0)
                .max(stale_floor_seconds(&self.feed, self.poll_cadence()));
            Some(Liveness::Live {
                age_seconds,
                stale: age_seconds > threshold,
            })
        } else {
            Some(Liveness::Archive { age_seconds })
        };
        debug_assert_eq!(
            self.history.generation(),
            generation_before,
            "liveness() is read-only and must never bump the generation"
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::super::feed::ArchiveWindow;
    use super::super::frame_history::{FrameHistoryEntry, FrameStatus, frame_identity_for_volume};
    use super::super::{EngineId, EngineRole, LoopEngine};
    use super::*;
    use chrono::TimeZone;
    use radar_core::{RadarSite, RadarVolume};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn us_site() -> SiteRef {
        SiteRef::Us {
            level2_id: "KEAX".to_owned(),
        }
    }

    fn intl_site() -> SiteRef {
        SiteRef::Intl {
            provider_id: "smhi".to_owned(),
            site_id: "angelholm".to_owned(),
        }
    }

    fn engine_with_frame_age(
        role: EngineRole,
        feed: FeedSource,
        age_seconds: i64,
    ) -> (LoopEngine, DateTime<Utc>) {
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 6, 0, 0).unwrap();
        let mut engine = LoopEngine::new(EngineId(9), role, feed);
        let volume = Arc::new(RadarVolume::new(
            RadarSite::new("KEAX"),
            now - chrono::Duration::seconds(age_seconds),
        ));
        engine.history.push(FrameHistoryEntry {
            identity: frame_identity_for_volume(&volume),
            path: PathBuf::from("liveness-test"),
            volume,
            timings: None,
            status: FrameStatus::Complete,
            source_label: "test".to_owned(),
        });
        (engine, now)
    }

    /// The engine-side liveness truth table (skeleton for the whole-phase
    /// chip gate): live+fresh, live+stale, archive feed, paused live feed,
    /// empty history.
    #[test]
    fn liveness_truth_table_live_stale_archive_paused_empty() {
        // Live US, fresh (age 60 <= user 480).
        let (engine, now) =
            engine_with_frame_age(EngineRole::Primary, FeedSource::Live(us_site()), 60);
        assert_eq!(
            engine.liveness(now, 480),
            Some(Liveness::Live {
                age_seconds: 60,
                stale: false
            })
        );

        // Live US, past the user threshold: stale (no intl floor rescues).
        let (engine, now) =
            engine_with_frame_age(EngineRole::Primary, FeedSource::Live(us_site()), 600);
        assert_eq!(
            engine.liveness(now, 480),
            Some(Liveness::Live {
                age_seconds: 600,
                stale: true
            })
        );

        // Live intl at 20 minutes: the 1800 s floor keeps it fresh even
        // with a low user threshold (the pinned intl chip behavior).
        let (engine, now) =
            engine_with_frame_age(EngineRole::Primary, FeedSource::Live(intl_site()), 1200);
        assert_eq!(
            engine.liveness(now, 480),
            Some(Liveness::Live {
                age_seconds: 1200,
                stale: false
            })
        );

        // Live intl at 40 minutes: past the floor — a fault worth flagging
        // even on intl cadences (pinned by the ORD stale-chip test).
        let (engine, now) =
            engine_with_frame_age(EngineRole::Primary, FeedSource::Live(intl_site()), 2400);
        assert_eq!(
            engine.liveness(now, 480),
            Some(Liveness::Live {
                age_seconds: 2400,
                stale: true
            })
        );

        // A user threshold ABOVE the floor still wins.
        let (engine, now) =
            engine_with_frame_age(EngineRole::Primary, FeedSource::Live(intl_site()), 2400);
        assert_eq!(
            engine.liveness(now, 3600),
            Some(Liveness::Live {
                age_seconds: 2400,
                stale: false
            })
        );

        // Archive feed: NEVER live, whatever the age.
        let (engine, now) = engine_with_frame_age(
            EngineRole::Primary,
            FeedSource::Archive {
                site: us_site(),
                window: ArchiveWindow {
                    start_utc: Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap(),
                    end_utc: Utc.with_ymd_and_hms(2026, 6, 9, 6, 0, 0).unwrap(),
                    anchor_utc: None,
                    max_frames: 20,
                },
            },
            60,
        );
        assert_eq!(
            engine.liveness(now, 480),
            Some(Liveness::Archive { age_seconds: 60 })
        );

        // Paused live feed reads as archive (live.enabled gates).
        let (mut engine, now) =
            engine_with_frame_age(EngineRole::Primary, FeedSource::Live(us_site()), 60);
        engine.live.enabled = false;
        assert_eq!(
            engine.liveness(now, 480),
            Some(Liveness::Archive { age_seconds: 60 })
        );

        // Empty history: nothing to report.
        let empty = LoopEngine::new(
            EngineId(9),
            EngineRole::Primary,
            FeedSource::Live(us_site()),
        );
        assert_eq!(empty.liveness(Utc::now(), 480), None);

        // Future-stamped frames clamp age to zero (legacy .max(0)).
        let (engine, now) =
            engine_with_frame_age(EngineRole::Primary, FeedSource::Live(us_site()), -120);
        assert_eq!(
            engine.liveness(now, 480),
            Some(Liveness::Live {
                age_seconds: 0,
                stale: false
            })
        );
    }

    /// Owner decision 3 (spec §12): the intl floor is cadence-aware —
    /// `max(user, 1800, 2 × cadence)` — and non-intl feeds have no floor.
    #[test]
    fn stale_floor_is_cadence_aware_for_intl_only() {
        let intl = FeedSource::Live(intl_site());
        assert_eq!(
            stale_floor_seconds(&intl, Some(Duration::from_secs(60))),
            1800,
            "2x60s is far below the 1800 s floor"
        );
        assert_eq!(
            stale_floor_seconds(&intl, Some(Duration::from_secs(1200))),
            2400,
            "a slow provider cadence raises the floor to 2x cadence"
        );
        assert_eq!(stale_floor_seconds(&intl, None), 1800);
        assert_eq!(
            stale_floor_seconds(&FeedSource::Live(us_site()), Some(Duration::from_secs(1))),
            0,
            "US behavior unchanged: no floor"
        );
        assert_eq!(
            stale_floor_seconds(
                &FeedSource::CustomUrl("http://example.com/dow8".to_owned()),
                Some(Duration::from_secs(15))
            ),
            0
        );
    }

    /// The (role, feed) cadence table from spec §3.
    #[test]
    fn poll_cadence_table_matches_the_spec_cells() {
        let cell = |role: EngineRole, feed: FeedSource| {
            LoopEngine::new(EngineId(9), role, feed).poll_cadence()
        };
        assert_eq!(
            cell(EngineRole::Primary, FeedSource::Live(us_site())),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            cell(EngineRole::Pane { slot: 0 }, FeedSource::Live(us_site())),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            cell(EngineRole::Overlay, FeedSource::Live(us_site())),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            cell(EngineRole::Overlay, FeedSource::Live(intl_site())),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            cell(
                EngineRole::Primary,
                FeedSource::CustomUrl("http://example.com/dow8".to_owned())
            ),
            Some(Duration::from_secs(15))
        );
        assert_eq!(
            cell(
                EngineRole::Primary,
                FeedSource::LocalFiles {
                    label: "drop".to_owned()
                }
            ),
            None
        );
    }
}
