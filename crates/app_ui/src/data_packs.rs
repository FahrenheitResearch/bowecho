use chrono::{DateTime, Utc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DataPackLayout {
    DualPolTornadoReview,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BuiltInDataPack {
    pub(crate) id: &'static str,
    pub(crate) title: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) site_id: &'static str,
    pub(crate) start_utc: &'static str,
    pub(crate) end_utc: &'static str,
    pub(crate) anchor_utc: &'static str,
    pub(crate) focus_lat: f32,
    pub(crate) focus_lon: f32,
    pub(crate) map_scale: f32,
    pub(crate) pad_scans: usize,
    pub(crate) max_frames: usize,
    pub(crate) layout: DataPackLayout,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DataPackLoadOptions {
    pub(crate) extra_start_scans: usize,
    pub(crate) extra_end_scans: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DataPackScene {
    pub(crate) id: &'static str,
    pub(crate) title: &'static str,
    pub(crate) site_id: &'static str,
    pub(crate) focus_lat: f32,
    pub(crate) focus_lon: f32,
    pub(crate) map_scale: f32,
    pub(crate) layout: DataPackLayout,
    pub(crate) autoplay: bool,
    pub(crate) options: DataPackLoadOptions,
    pub(crate) resume_poll_url: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LoadedDataPack {
    pub(crate) id: &'static str,
    pub(crate) extra_start_scans: usize,
    pub(crate) extra_end_scans: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResearchFeedDataPack {
    pub(crate) id: &'static str,
    pub(crate) title: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) feed_id: &'static str,
    pub(crate) poll_url: &'static str,
    pub(crate) frame_count: usize,
    pub(crate) focus_lat: f32,
    pub(crate) focus_lon: f32,
    pub(crate) map_scale: f32,
    pub(crate) layout: DataPackLayout,
}

impl BuiltInDataPack {
    #[cfg(test)]
    pub(crate) fn window_request(self) -> Result<data_source::Level2ArchiveWindowRequest, String> {
        self.window_request_with_options(DataPackLoadOptions::default())
    }

    pub(crate) fn window_request_with_options(
        self,
        options: DataPackLoadOptions,
    ) -> Result<data_source::Level2ArchiveWindowRequest, String> {
        let start_utc = parse_pack_time(self.start_utc)?;
        let end_utc = parse_pack_time(self.end_utc)?;
        let anchor_utc = parse_pack_time(self.anchor_utc)?;
        if end_utc < start_utc {
            return Err(format!("{} has an end before its start", self.title));
        }
        if anchor_utc < start_utc || anchor_utc > end_utc {
            return Err(format!("{} anchor is outside its load window", self.title));
        }
        Ok(data_source::Level2ArchiveWindowRequest {
            start_utc,
            end_utc,
            anchor_utc,
            pad_scans: self.pad_scans,
            extra_start_scans: options.extra_start_scans,
            extra_end_scans: options.extra_end_scans,
            max_objects: self
                .max_frames
                .saturating_add(options.extra_start_scans)
                .saturating_add(options.extra_end_scans),
        })
    }

    pub(crate) fn scene(self, options: DataPackLoadOptions) -> DataPackScene {
        DataPackScene {
            id: self.id,
            title: self.title,
            site_id: self.site_id,
            focus_lat: self.focus_lat,
            focus_lon: self.focus_lon,
            map_scale: self.map_scale,
            layout: self.layout,
            autoplay: true,
            options,
            resume_poll_url: None,
        }
    }
}

impl ResearchFeedDataPack {
    pub(crate) fn scene(self) -> DataPackScene {
        DataPackScene {
            id: self.id,
            title: self.title,
            site_id: self.feed_id,
            focus_lat: self.focus_lat,
            focus_lon: self.focus_lon,
            map_scale: self.map_scale,
            layout: self.layout,
            autoplay: false,
            options: DataPackLoadOptions::default(),
            resume_poll_url: Some(self.poll_url),
        }
    }
}

pub(crate) const BUILT_IN_DATA_PACKS: &[BuiltInDataPack] = &[
    BuiltInDataPack {
        id: "kvwx-2026-dealias-debug",
        title: "KVWX Velocity Fold Debug",
        summary: "KVWX 2026-06-22 03:31 UTC 1.23 degree velocity case for hybrid dealias QA.",
        site_id: "KVWX",
        start_utc: "2026-06-22T03:20:00Z",
        end_utc: "2026-06-22T03:36:00Z",
        anchor_utc: "2026-06-22T03:31:06Z",
        focus_lat: 37.955,
        focus_lon: -87.433,
        map_scale: 5375.0,
        pad_scans: 2,
        max_frames: 20,
        layout: DataPackLayout::DualPolTornadoReview,
    },
    BuiltInDataPack {
        id: "moore-2013-ktlx",
        title: "Moore EF5",
        summary: "KTLX dual-pol review of the Newcastle-Moore tornadic supercell.",
        site_id: "KTLX",
        start_utc: "2013-05-20T19:50:00Z",
        end_utc: "2013-05-20T20:50:00Z",
        anchor_utc: "2013-05-20T20:21:00Z",
        focus_lat: 35.339,
        focus_lon: -97.486,
        map_scale: 760.0,
        pad_scans: 2,
        max_frames: 18,
        layout: DataPackLayout::DualPolTornadoReview,
    },
    BuiltInDataPack {
        id: "el-reno-2013-ktlx",
        title: "El Reno Wedge",
        summary: "KTLX dual-pol loop around the record-width El Reno circulation.",
        site_id: "KTLX",
        start_utc: "2013-05-31T22:55:00Z",
        end_utc: "2013-05-31T23:55:00Z",
        anchor_utc: "2013-05-31T23:33:00Z",
        focus_lat: 35.500,
        focus_lon: -97.980,
        map_scale: 880.0,
        pad_scans: 2,
        max_frames: 18,
        layout: DataPackLayout::DualPolTornadoReview,
    },
    BuiltInDataPack {
        id: "rochelle-fairdale-2015-klot",
        title: "Rochelle-Fairdale EF4",
        summary: "KLOT dual-pol view of the northern Illinois violent tornado.",
        site_id: "KLOT",
        start_utc: "2015-04-09T23:15:00Z",
        end_utc: "2015-04-10T01:00:00Z",
        anchor_utc: "2015-04-10T00:13:00Z",
        focus_lat: 42.10,
        focus_lon: -88.94,
        map_scale: 840.0,
        pad_scans: 2,
        max_frames: 18,
        layout: DataPackLayout::DualPolTornadoReview,
    },
    BuiltInDataPack {
        id: "mayfield-2021-kpah",
        title: "Mayfield EF4",
        summary: "KPAH dual-pol debris signature through western Kentucky.",
        site_id: "KPAH",
        start_utc: "2021-12-11T02:45:00Z",
        end_utc: "2021-12-11T04:05:00Z",
        anchor_utc: "2021-12-11T03:27:00Z",
        focus_lat: 36.74,
        focus_lon: -88.64,
        map_scale: 920.0,
        pad_scans: 2,
        max_frames: 18,
        layout: DataPackLayout::DualPolTornadoReview,
    },
    BuiltInDataPack {
        id: "rolling-fork-2023-kdgx",
        title: "Rolling Fork EF4",
        summary: "KDGX dual-pol loop for the Rolling Fork/Silver City tornado.",
        site_id: "KDGX",
        start_utc: "2023-03-25T00:50:00Z",
        end_utc: "2023-03-25T02:15:00Z",
        anchor_utc: "2023-03-25T01:07:00Z",
        focus_lat: 32.906,
        focus_lon: -90.878,
        map_scale: 880.0,
        pad_scans: 2,
        max_frames: 18,
        layout: DataPackLayout::DualPolTornadoReview,
    },
    // The SAME long-track supercell ~2.5 h after Rolling Fork: the
    // Amory/New Wren EF3 between Okolona and Aberdeen MS, as seen by
    // KGWX. Also the dealias battery's case H — the 0.48° velocity
    // tilts at 03:42:09 and 03:44:02 under-unfold the inbound (west)
    // side of the couplet (docs/dealias-v4-spec.md §18).
    BuiltInDataPack {
        id: "amory-2023-kgwx",
        title: "Amory EF3",
        summary: "KGWX velocity couplet from the Rolling Fork supercell's \
                  second violent tornado, west of Amory.",
        site_id: "KGWX",
        start_utc: "2023-03-25T03:18:00Z",
        end_utc: "2023-03-25T04:05:00Z",
        anchor_utc: "2023-03-25T03:38:00Z",
        focus_lat: 33.95,
        focus_lon: -88.62,
        map_scale: 880.0,
        pad_scans: 2,
        max_frames: 12,
        layout: DataPackLayout::DualPolTornadoReview,
    },
];

/// A ready-made review scene over an international provider archive —
/// the EU case of the one-archive-world unification (v0.29 spec Phase 2):
/// it loads through the same `archive_source()` window path as the
/// unified browser and the Unified Player, never a bespoke fetcher.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IntlArchiveDataPack {
    pub(crate) id: &'static str,
    pub(crate) title: &'static str,
    pub(crate) summary: &'static str,
    /// `data_source::international` provider id (`"ord"`).
    pub(crate) provider_id: &'static str,
    /// Provider-scoped, case-significant site code (`"plbrz"`).
    pub(crate) site_id: &'static str,
    pub(crate) start_utc: &'static str,
    pub(crate) end_utc: &'static str,
    pub(crate) focus_lat: f32,
    pub(crate) focus_lon: f32,
    pub(crate) map_scale: f32,
    pub(crate) max_frames: usize,
}

impl IntlArchiveDataPack {
    pub(crate) fn window_utc(self) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
        let start_utc = parse_pack_time(self.start_utc)?;
        let end_utc = parse_pack_time(self.end_utc)?;
        if end_utc < start_utc {
            return Err(format!("{} has an end before its start", self.title));
        }
        Ok((start_utc, end_utc))
    }
}

/// EUMETNET ORD archive review scene. Window verified against the
/// anonymous `openradar-archive` bucket on 2026-07-02: Brzuchania
/// publishes complete DBZH+TH+VRADH PVOL scans on a 5-minute cadence
/// across this window (CC BY 4.0 — OPERA and IMGW-PIB attribution).
pub(crate) const INTL_ARCHIVE_DATA_PACKS: &[IntlArchiveDataPack] = &[IntlArchiveDataPack {
    id: "ord-plbrz-archive-review",
    title: "Brzuchania ORD Archive",
    summary: "Two-hour EUMETNET ORD archive loop over Poland's Brzuchania radar — the \
              international proof case for the unified archive loader (reflectivity + \
              velocity split files merged per scan).",
    provider_id: "ord",
    site_id: "plbrz",
    start_utc: "2026-06-15T16:30:00Z",
    end_utc: "2026-06-15T18:30:00Z",
    focus_lat: 50.3942,
    focus_lon: 20.0832,
    map_scale: 880.0,
    max_frames: 24,
}];

pub(crate) const RESEARCH_FEED_DATA_PACKS: &[ResearchFeedDataPack] = &[ResearchFeedDataPack {
    id: "kcri-latest-20",
    title: "KCRI Latest 20",
    summary: "Newest 20 decodable KCRI research-radar frames from the IEM GR2A feed.",
    feed_id: "KCRI",
    poll_url: "https://mesonet-nexrad.agron.iastate.edu/level2/raw/KCRI",
    frame_count: 20,
    focus_lat: 35.238,
    focus_lon: -97.460,
    map_scale: 920.0,
    layout: DataPackLayout::DualPolTornadoReview,
}];

fn parse_pack_time(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|err| format!("invalid data pack time {value}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_data_packs_parse_to_valid_windows() {
        assert_eq!(BUILT_IN_DATA_PACKS.len(), 7);

        for pack in BUILT_IN_DATA_PACKS {
            let request = pack
                .window_request()
                .unwrap_or_else(|err| panic!("{} should parse: {err}", pack.title));
            assert!(request.start_utc <= request.anchor_utc);
            assert!(request.anchor_utc <= request.end_utc);
            assert!(request.max_objects > 0);
            // (The K-prefix assertion is gone: data packs are no longer
            // a US-only concept — see INTL_ARCHIVE_DATA_PACKS.)
        }
    }

    #[test]
    fn intl_archive_packs_parse_to_valid_windows_over_archive_capable_providers() {
        assert_eq!(INTL_ARCHIVE_DATA_PACKS.len(), 1);
        for pack in INTL_ARCHIVE_DATA_PACKS {
            let (start_utc, end_utc) = pack
                .window_utc()
                .unwrap_or_else(|err| panic!("{} should parse: {err}", pack.title));
            assert!(start_utc < end_utc);
            assert!(pack.max_frames > 0);
            assert_eq!(
                pack.site_id,
                pack.site_id.to_lowercase(),
                "intl site codes are case-significant lowercase"
            );
            // The pack must point at a provider whose derived capability
            // says archive — the same gate the loaders use.
            let provider = data_source::international::intl_providers()
                .into_iter()
                .find(|provider| provider.id() == pack.provider_id)
                .unwrap_or_else(|| panic!("{}: unknown provider", pack.title));
            assert!(
                provider.supports_archive(),
                "{}: provider '{}' has no archive source",
                pack.title,
                pack.provider_id
            );
        }
    }

    #[test]
    fn pack_set_is_dual_pol_first() {
        assert!(
            BUILT_IN_DATA_PACKS
                .iter()
                .all(|pack| pack.layout == DataPackLayout::DualPolTornadoReview)
        );
    }

    #[test]
    fn load_options_add_explicit_window_scans() {
        let pack = BUILT_IN_DATA_PACKS[0];
        let request = pack
            .window_request_with_options(DataPackLoadOptions {
                extra_start_scans: 3,
                extra_end_scans: 3,
            })
            .expect("request");

        assert_eq!(request.extra_start_scans, 3);
        assert_eq!(request.extra_end_scans, 3);
        assert_eq!(request.max_objects, pack.max_frames + 6);
    }

    #[test]
    fn scene_remembers_loaded_window_expansion() {
        let pack = BUILT_IN_DATA_PACKS[0];
        let options = DataPackLoadOptions {
            extra_start_scans: 6,
            extra_end_scans: 3,
        };
        let scene = pack.scene(options);

        assert_eq!(scene.id, pack.id);
        assert_eq!(scene.options.extra_start_scans, 6);
        assert_eq!(scene.options.extra_end_scans, 3);
        assert_eq!(scene.resume_poll_url, None);
    }

    #[test]
    fn research_feed_pack_resumes_its_poll_url() {
        let pack = RESEARCH_FEED_DATA_PACKS[0];
        let scene = pack.scene();

        assert_eq!(scene.site_id, "KCRI");
        assert_eq!(scene.resume_poll_url, Some(pack.poll_url));
        assert!(!scene.autoplay);
        assert_eq!(pack.frame_count, 20);
    }
}
