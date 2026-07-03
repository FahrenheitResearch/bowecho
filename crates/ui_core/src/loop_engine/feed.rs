//! The engine's feed model (spec §2): live vs archive is a property of the
//! SOURCE, not a flag choreography. `PollSource` stays alive in
//! `app_ui/main.rs` for the primary view until Phase 4e; the overlay
//! adoption stage maps `IntlOverlayFeed` onto [`FeedSource`] first.

use chrono::{DateTime, Utc};
use data_source::sites::SiteRef;

/// What a [`LoopEngine`](super::LoopEngine) follows. Invariant: an
/// `Archive` feed never refreshes and never polls — an archive engine
/// structurally CANNOT claim LIVE ([`liveness`](super::LoopEngine::liveness)
/// matches on the variant). GO LIVE is an explicit
/// `set_feed(Live(same site))`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedSource {
    /// Follow the newest data for a site on the source's cadence
    /// (US chunk chain or `IntlProvider::latest`).
    Live(SiteRef),
    /// A fixed UTC window. Never refreshes, never polls.
    Archive {
        site: SiteRef,
        window: ArchiveWindow,
    },
    /// GR2A-style dir.list poll. Primary only. Kept verbatim: the payload
    /// records the URL the poll was STARTED with, exactly like
    /// `PollSource::CustomUrl`.
    CustomUrl(String),
    /// Drag-drop / file dialog / data packs. The label is display-only.
    LocalFiles { label: String },
}

/// A fixed archive window. Invariant: `start_utc <= end_utc` is the
/// caller's contract (listers treat a backwards window as empty);
/// `anchor_utc` is the loop-ending-at-scan anchor when the load was
/// started from a "loop ending at" control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveWindow {
    pub start_utc: DateTime<Utc>,
    pub end_utc: DateTime<Utc>,
    pub anchor_utc: Option<DateTime<Utc>>,
    pub max_frames: usize,
}

/// What a feed switch does to the engine's history (spec §2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitchAction {
    /// Same-source restart: the loop history survives (poll bookkeeping
    /// still resets — that is the caller's/engine's side of the switch).
    KeepHistory,
    /// Everything goes: history, cursor, displayed volume, in-flight
    /// loads of the old source.
    ClearAll,
}

/// ONE home for every history-clear rule on a feed switch (spec §2). Pure
/// so the table below is unit-testable without an event loop.
///
/// `old_live_active` is the legacy `poll_active` gate: the same-site KEEP
/// requires the old feed to have been actively polling ("inactive
/// same-site start clears" — pinned by
/// `intl_poll_switch_keeps_active_same_site_history_and_clears_the_rest`
/// in app_ui). It is a separate argument, not a `FeedSource` variant,
/// because a paused live feed is still a Live feed (`LiveState.enabled`
/// carries the pause).
///
/// The rule table, each row pinned by a legacy regression test that stays
/// green in app_ui:
///
/// | old                     | new                    | action | pinned by |
/// |-------------------------|------------------------|--------|-----------|
/// | Live(A), active         | Live(A)                | Keep   | `intl_poll_switch_keeps…` (KEEP arm) |
/// | Live(A), active         | Live(B)                | Clear  | same test (cross-site arm) |
/// | Live(A), INACTIVE       | Live(A)                | Clear  | same test ("poll_active gates the keep") |
/// | anything US/archive     | Live(intl)             | Clear  | `starting_intl_poll_clears_previous_us_primary_display` |
/// | CustomUrl(u), active    | CustomUrl(u)           | Keep   | `start_known_feed_poll` same-source guard (layers_rail.rs) |
/// | CustomUrl(u), active    | CustomUrl(v)           | Clear  | same guard |
/// | any                     | Archive { .. }         | Clear  | ORD/SMHI/intl archive loads clear at start (census D3 related row) |
/// | any                     | LocalFiles             | Keep   | local loads never clear at switch; the INSTALL cross-site guard clears if the file is another site's (census D3) |
///
/// Cross-provider with an identical site-id string clears because
/// [`SiteRef`] equality includes the provider.
pub fn switch_policy(old: &FeedSource, old_live_active: bool, new: &FeedSource) -> SwitchAction {
    match new {
        FeedSource::Live(new_site) => {
            let same_active_live = old_live_active
                && matches!(old, FeedSource::Live(old_site) if old_site == new_site);
            if same_active_live {
                SwitchAction::KeepHistory
            } else {
                SwitchAction::ClearAll
            }
        }
        FeedSource::CustomUrl(new_url) => {
            let same_active_url = old_live_active
                && matches!(old, FeedSource::CustomUrl(old_url) if old_url == new_url);
            if same_active_url {
                SwitchAction::KeepHistory
            } else {
                SwitchAction::ClearAll
            }
        }
        FeedSource::Archive { .. } => SwitchAction::ClearAll,
        FeedSource::LocalFiles { .. } => SwitchAction::KeepHistory,
    }
}

impl FeedSource {
    /// Persist the selection, mirroring `PollSource::save_to_settings`
    /// (app_ui) byte-for-byte: each variant writes only its own settings
    /// fields, so switching sources never forgets the other one.
    ///
    /// `Live(Us)`, `Archive`, and `LocalFiles` deliberately write NOTHING:
    /// v0.28 has no settings fields for them (startup restore of a display
    /// owner is Phase 3's encoded-string key), and inventing keys here
    /// would break the forward-compat rule (spec §10 risk 6).
    pub fn save_to_settings(&self, settings: &mut settings::AppSettings) {
        match self {
            FeedSource::CustomUrl(url) => settings.poll_url = url.clone(),
            FeedSource::Live(SiteRef::Intl {
                provider_id,
                site_id,
            }) => {
                settings.intl_provider = provider_id.clone();
                settings.intl_site = site_id.clone();
            }
            FeedSource::Live(SiteRef::Us { .. })
            | FeedSource::Archive { .. }
            | FeedSource::LocalFiles { .. } => {}
        }
    }

    /// Rebuild the last session's international selection — `None` until
    /// the user has ever started an international poll. Mirrors
    /// `PollSource::intl_from_settings` (app_ui): trims both fields and
    /// requires both non-empty, so a half-written pair never arms Start.
    pub fn intl_from_settings(settings: &settings::AppSettings) -> Option<FeedSource> {
        let provider_id = settings.intl_provider.trim();
        let site_id = settings.intl_site.trim();
        (!provider_id.is_empty() && !site_id.is_empty()).then(|| {
            FeedSource::Live(SiteRef::Intl {
                provider_id: provider_id.to_owned(),
                site_id: site_id.to_owned(),
            })
        })
    }

    /// True for the variants that follow a feed (poll on a cadence):
    /// `Live` and `CustomUrl`. Archive and local feeds never poll.
    pub fn is_live_variant(&self) -> bool {
        matches!(self, FeedSource::Live(_) | FeedSource::CustomUrl(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn intl(provider: &str, site: &str) -> FeedSource {
        FeedSource::Live(SiteRef::Intl {
            provider_id: provider.to_owned(),
            site_id: site.to_owned(),
        })
    }

    fn us(id: &str) -> FeedSource {
        FeedSource::Live(SiteRef::Us {
            level2_id: id.to_owned(),
        })
    }

    fn archive(site: SiteRef) -> FeedSource {
        FeedSource::Archive {
            site,
            window: ArchiveWindow {
                start_utc: Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap(),
                end_utc: Utc.with_ymd_and_hms(2026, 6, 9, 6, 0, 0).unwrap(),
                anchor_utc: None,
                max_frames: 20,
            },
        }
    }

    /// The full switch table from the doc comment, one assert per row.
    /// Ported from the legacy regression tests named there (which stay
    /// green in app_ui, still driving `start_intl_poll` directly).
    #[test]
    fn switch_policy_table_matches_the_pinned_legacy_clear_rules() {
        use SwitchAction::{ClearAll, KeepHistory};
        let table: &[(FeedSource, bool, FeedSource, SwitchAction, &str)] = &[
            (
                intl("smhi", "angelholm"),
                true,
                intl("smhi", "angelholm"),
                KeepHistory,
                "active same-site restart keeps the loop",
            ),
            (
                intl("smhi", "angelholm"),
                true,
                intl("smhi", "sella"),
                ClearAll,
                "cross-site intl switch clears",
            ),
            (
                intl("smhi", "sella"),
                false,
                intl("smhi", "sella"),
                ClearAll,
                "inactive same-site start clears (poll_active gates the keep)",
            ),
            (
                intl("smhi", "sella"),
                true,
                intl("dwd", "sella"),
                ClearAll,
                "cross-provider switch clears even with an identical site id",
            ),
            (
                us("KTLX"),
                false,
                intl("smhi", "angelholm"),
                ClearAll,
                "arriving from a US primary display clears",
            ),
            (
                us("KTLX"),
                true,
                us("KTLX"),
                KeepHistory,
                "active same-site US live restart keeps",
            ),
            (
                FeedSource::CustomUrl("http://example.com/dow8".to_owned()),
                true,
                FeedSource::CustomUrl("http://example.com/dow8".to_owned()),
                KeepHistory,
                "active same-URL custom poll keeps (start_known_feed_poll guard)",
            ),
            (
                FeedSource::CustomUrl("http://example.com/dow8".to_owned()),
                true,
                FeedSource::CustomUrl("http://example.com/cow2".to_owned()),
                ClearAll,
                "different URL clears",
            ),
            (
                FeedSource::CustomUrl("http://example.com/dow8".to_owned()),
                true,
                intl("smhi", "angelholm"),
                ClearAll,
                "custom URL to intl clears",
            ),
            (
                intl("ord", "deess"),
                true,
                archive(SiteRef::Intl {
                    provider_id: "ord".to_owned(),
                    site_id: "deess".to_owned(),
                }),
                ClearAll,
                "any archive target clears, even same-site (ORD/SMHI load-start clears)",
            ),
            (
                us("KTLX"),
                true,
                FeedSource::LocalFiles {
                    label: "drop".to_owned(),
                },
                KeepHistory,
                "local files keep at switch; the install guard clears cross-site",
            ),
        ];
        for (old, active, new, want, why) in table {
            assert_eq!(switch_policy(old, *active, new), *want, "{why}");
        }
    }

    /// The settings shims mirror `PollSource` exactly: per-variant writes
    /// that never forget the other world, and an intl restore that
    /// requires both trimmed fields.
    #[test]
    fn feed_source_settings_shims_mirror_poll_source_encodings() {
        let mut settings = settings::AppSettings {
            poll_url: "http://example.com/dow8".to_owned(),
            ..Default::default()
        };

        intl("smhi", "angelholm").save_to_settings(&mut settings);
        assert_eq!(settings.intl_provider, "smhi");
        assert_eq!(settings.intl_site, "angelholm");
        assert_eq!(
            settings.poll_url, "http://example.com/dow8",
            "intl write must not forget the custom URL"
        );

        FeedSource::CustomUrl("http://example.com/cow2".to_owned()).save_to_settings(&mut settings);
        assert_eq!(settings.poll_url, "http://example.com/cow2");
        assert_eq!(
            settings.intl_provider, "smhi",
            "custom URL write must not forget the intl pair"
        );

        // US live / archive / local: deliberately no settings writes.
        us("KTLX").save_to_settings(&mut settings);
        archive(SiteRef::Us {
            level2_id: "KTLX".to_owned(),
        })
        .save_to_settings(&mut settings);
        FeedSource::LocalFiles {
            label: "drop".to_owned(),
        }
        .save_to_settings(&mut settings);
        assert_eq!(settings.poll_url, "http://example.com/cow2");
        assert_eq!(settings.intl_provider, "smhi");
        assert_eq!(settings.intl_site, "angelholm");

        // Restore mirrors PollSource::intl_from_settings: trim + both
        // fields required.
        assert_eq!(
            FeedSource::intl_from_settings(&settings),
            Some(intl("smhi", "angelholm"))
        );
        settings.intl_site = "  ".to_owned();
        assert_eq!(FeedSource::intl_from_settings(&settings), None);
        settings.intl_site = " angelholm ".to_owned();
        assert_eq!(
            FeedSource::intl_from_settings(&settings),
            Some(intl("smhi", "angelholm")),
            "restore trims whitespace like the legacy shim"
        );
    }
}
