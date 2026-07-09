//! CHMI (Český hydrometeorologický ústav, Czechia) radar volume feed.
//!
//! Catalog: `https://opendata.chmi.cz/meteorology/weather/radar/sites/`,
//! an nginx autoindex tree (captured live 2026-06-12 UTC):
//!
//! ```text
//! sites/
//!   brd/  ska/                              stations
//!     vol_z/ vol_v/ vol_w/ vol_u/ vol_zdr/ vol_rhohv/ vol_phidp/
//!       hdf5/
//!         T_PAGZ60_C_OKPR_20260612063911.hdf
//!         T_PAYA60_C_OKPR_20260612063948.hdf
//!         T_PAYB60_C_OKPR_20260612064025.hdf
//! ```
//!
//! Files are ODIM_H5 (EUMETNET OPERA Data Information Model; Michelson et
//! al., OPERA WP 2.1/2.2, v2.2-2.3), split by product directory AND by
//! scan task. The task is the fourth letter of the WMO bulletin header
//! (`T_PAGZ60` -> task `Z`), and live decodes show what each task is:
//!
//! - task `Z` (`PAGZ`/`PAHZ`/`PAKZ`..., every 5 min): the FULL volume —
//!   12 PPI cuts, 360 radials, one moment per product file;
//! - task `B` (`PAYB`/`PAHB`..., every 5 min): one supplemental 1.5° cut
//!   at finer gate spacing (200 m vs 400 m);
//! - task `A` (`PAYA`/`PAHA`..., every 10 min): one supplemental 0.3° cut.
//!
//! Timestamps agree across products WITHIN a task (vol_z `PAYB` and vol_v
//! `PAHB` share stamps) but differ BETWEEN tasks, so "newest common
//! timestamp" cannot bind every supplemental cut. Instead the newest task-Z
//! timestamp shared by required products anchors the frame; supplemental
//! tasks are admitted only at exact timestamps shared by those products.
//! Parts are ordered task-major — full volumes (`Z`) first so the
//! 12-cut reflectivity PVOL is the merge base, then `B`, then `A`. The
//! supplemental cuts either union in as new elevations (0.3°) or collide
//! with a same-elevation full-volume cut of different gate geometry
//! (1.5°), which `radar_core::merge_radar_volumes` reports as
//! `skipped_geometry` rather than mixing geometries.

use chrono::NaiveDateTime;

use super::listing::{ListingEntry, digit_run, fnv1a64, join_url, parse_autoindex};
use super::{FramePlan, IntlProvider, IntlSite, PlanPart, RecentFrames};
use crate::{fetch_listing_text, fetch_text};

const CHMI_SITES_ROOT: &str = "https://opendata.chmi.cz/meteorology/weather/radar/sites/";
/// Freshness window: must cover the 10-minute `A` task (plus jitter) but
/// stay short enough that stale tasks drop out instead of showing old air.
const FRESHNESS_WINDOW_MINUTES: i64 = 12;
/// B/A normally land shortly after task Z. Admit normal upload jitter without
/// stealing an early supplemental cut from the next cycle.
const SUPPLEMENTAL_LEAD_MINUTES: i64 = 2;

struct ChmiProduct {
    dir: &'static str,
    required: bool,
}

/// Product directories to assemble, in merge order per task.
const CHMI_PRODUCTS: [ChmiProduct; 7] = [
    ChmiProduct {
        dir: "vol_z",
        required: true,
    },
    ChmiProduct {
        dir: "vol_v",
        required: true,
    },
    ChmiProduct {
        dir: "vol_w",
        required: false,
    },
    ChmiProduct {
        dir: "vol_u",
        required: false,
    },
    ChmiProduct {
        dir: "vol_zdr",
        required: false,
    },
    ChmiProduct {
        dir: "vol_rhohv",
        required: false,
    },
    ChmiProduct {
        dir: "vol_phidp",
        required: false,
    },
];

/// Station labels and coordinates, verified 2026-06-12 against the
/// `/where` group of live CHMI ODIM volumes (lat/lon as decoded).
const CHMI_STATIONS: [(&str, &str, f32, f32); 2] = [
    ("brd", "Brdy-Praha", 49.6583, 13.8178),
    ("ska", "Skalky", 49.5011, 16.7885),
];

/// Czechia's CHMI open-data radar feed (per-product, per-scan-task files).
#[derive(Clone, Copy, Debug, Default)]
pub struct ChmiProvider;

impl ChmiProvider {
    pub fn new() -> Self {
        Self
    }
}

impl IntlProvider for ChmiProvider {
    fn id(&self) -> &'static str {
        "chmi"
    }

    fn label(&self) -> &'static str {
        "CHMI Czechia"
    }

    fn country(&self) -> &'static str {
        "Czechia"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        let html = fetch_text(CHMI_SITES_ROOT)
            .map_err(|err| format!("CHMI station listing {CHMI_SITES_ROOT}: {err}"))?;
        let mut sites: Vec<IntlSite> = parse_autoindex(&html)
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| {
                let known = CHMI_STATIONS.iter().find(|(id, _, _, _)| *id == entry.name);
                IntlSite {
                    provider_id: self.id(),
                    label: known.map_or_else(
                        || entry.name.to_ascii_uppercase(),
                        |(_, label, _, _)| (*label).to_owned(),
                    ),
                    latitude_deg: known.map(|(_, _, lat, _)| *lat),
                    longitude_deg: known.map(|(_, _, _, lon)| *lon),
                    site_id: entry.name,
                    country: self.country(),
                }
            })
            .collect();
        if sites.is_empty() {
            return Err(format!(
                "CHMI station listing {CHMI_SITES_ROOT} held no station directories"
            ));
        }
        sites.sort_by(|left, right| left.site_id.cmp(&right.site_id));
        Ok(sites)
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        if !is_safe_path_segment(site_id) {
            return Err(format!("CHMI: invalid site id '{site_id}'"));
        }
        let listings = fetch_product_listings(site_id)?;
        let Some(anchor) = chmi_frame_anchors(&listings, 1).into_iter().next() else {
            return Err(format!(
                "CHMI {}: no task-Z cycle shared by the required products in {}",
                listings[0].dir, listings[0].dir_url
            ));
        };
        assemble_frame(site_id, anchor, &listings)
    }

    fn recent_source(&self) -> Option<&dyn RecentFrames> {
        Some(self)
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        CHMI_STATIONS
            .iter()
            .map(|&(site_id, label, latitude_deg, longitude_deg)| IntlSite {
                provider_id: self.id(),
                site_id: site_id.to_owned(),
                label: label.to_owned(),
                country: self.country(),
                latitude_deg: Some(latitude_deg),
                longitude_deg: Some(longitude_deg),
            })
            .collect()
    }
}

impl RecentFrames for ChmiProvider {
    fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        if !is_safe_path_segment(site_id) {
            return Err(format!("CHMI: invalid site id '{site_id}'"));
        }
        let listings = fetch_product_listings(site_id)?;
        let anchors = chmi_frame_anchors(&listings, count);
        if anchors.is_empty() {
            return Err(format!(
                "CHMI {}: could not resolve a frame anchor from {}",
                listings[0].dir, listings[0].dir_url
            ));
        }
        let mut plans = Vec::with_capacity(anchors.len());
        for (index, anchor) in anchors.iter().enumerate() {
            match assemble_frame(site_id, *anchor, &listings) {
                Ok(plan) => plans.push(plan),
                // The newest frame is the poll dedupe key: its failure is
                // the loop's failure. Older frames just shorten the loop.
                Err(err) if index == 0 => return Err(err),
                Err(_) => continue,
            }
        }
        plans.reverse();
        Ok(plans)
    }
}

/// One fetched-and-parsed product directory listing.
struct ChmiProductListing {
    product_rank: usize,
    dir: &'static str,
    required: bool,
    dir_url: String,
    files: Vec<ChmiFile>,
}

/// Fetch and parse every product directory listing for `site_id` (the one
/// catalog probe both `latest` and `recent` run). The first entry is always
/// the required `vol_z` listing with at least one parsed file — missing or
/// empty required listings are errors — so callers can anchor frames on
/// `listings[0]`.
fn fetch_product_listings(site_id: &str) -> Result<Vec<ChmiProductListing>, String> {
    let mut listings = Vec::new();
    for (product_rank, product) in CHMI_PRODUCTS.iter().enumerate() {
        let dir_url = format!("{CHMI_SITES_ROOT}{site_id}/{}/hdf5/", product.dir);
        let html = match fetch_listing_text(&dir_url) {
            Ok(html) => html,
            Err(err) if product.required => {
                return Err(format!("CHMI file listing {dir_url}: {err}"));
            }
            Err(_) => continue,
        };
        let files = parse_chmi_files(&parse_autoindex(&html));
        if product.required && files.is_empty() {
            return Err(format!(
                "CHMI file listing {dir_url}: no T_..._C_..._<timestamp>.hdf files"
            ));
        }
        listings.push(ChmiProductListing {
            product_rank,
            dir: product.dir,
            required: product.required,
            dir_url,
            files,
        });
    }
    if listings.is_empty() {
        return Err(format!("CHMI: no products resolved for site '{site_id}'"));
    }
    Ok(listings)
}

/// Task-Z full-volume timestamps shared by every required product, NEWEST
/// FIRST. A supplemental B/A file can appear before the next Z upload; it is
/// never allowed to advance the frame generation on its own.
fn chmi_frame_anchors(listings: &[ChmiProductListing], count: usize) -> Vec<NaiveDateTime> {
    let Some(base) = listings.first() else {
        return Vec::new();
    };
    let mut anchors: Vec<NaiveDateTime> = base
        .files
        .iter()
        .filter(|file| file.task == 'Z')
        .filter(|file| {
            listings
                .iter()
                .filter(|listing| listing.required)
                .all(|listing| {
                    listing
                        .files
                        .iter()
                        .any(|candidate| candidate.task == 'Z' && candidate.time == file.time)
                })
        })
        .map(|file| file.time)
        .collect();
    anchors.sort_unstable();
    anchors.dedup();
    anchors.reverse();
    anchors.truncate(count.max(1));
    anchors
}

/// Assemble the frame anchored at `anchor` from already-fetched product
/// listings (pure; unit-testable). Identity and part order are exactly what
/// `latest` has always produced for its (newest) anchor.
fn assemble_frame(
    site_id: &str,
    anchor: NaiveDateTime,
    listings: &[ChmiProductListing],
) -> Result<FramePlan, String> {
    let Some(base_listing) = listings.first() else {
        return Err(format!("CHMI {site_id}: no product listings to assemble"));
    };
    let coherent_tasks: Vec<ChmiFile> = freshest_per_task(&base_listing.files, anchor)
        .into_iter()
        .filter(|base_file| {
            listings
                .iter()
                .filter(|listing| listing.required)
                .all(|listing| {
                    listing
                        .files
                        .iter()
                        .any(|file| file.task == base_file.task && file.time == base_file.time)
                })
        })
        .collect();
    if !coherent_tasks
        .iter()
        .any(|file| file.task == 'Z' && file.time == anchor)
    {
        return Err(format!(
            "CHMI {site_id}: task-Z cycle {anchor} is not complete across required products"
        ));
    }

    let mut picks: Vec<(usize, ChmiFile, &str)> = Vec::new();
    for listing in listings {
        let coherent: Vec<ChmiFile> = coherent_tasks
            .iter()
            .filter_map(|task| {
                listing
                    .files
                    .iter()
                    .find(|file| file.task == task.task && file.time == task.time)
                    .cloned()
            })
            .collect();
        if listing.required
            && !coherent
                .iter()
                .any(|file| file.task == 'Z' && file.time == anchor)
        {
            return Err(format!(
                "CHMI {}/{site_id}: no task-Z file for coherent cycle {anchor} \
                 ({} files inspected)",
                listing.dir,
                listing.files.len()
            ));
        }
        picks.extend(
            coherent
                .into_iter()
                .map(|file| (listing.product_rank, file, listing.dir_url.as_str())),
        );
    }
    // Task-major order: full volumes (Z) for every product first — the
    // 12-cut reflectivity PVOL must be the merge base — then the
    // supplemental B and A single-cut tasks.
    picks.sort_by_key(|(product_rank, file, _)| (task_rank(file.task), *product_rank));
    let parts: Vec<PlanPart> = picks
        .iter()
        .map(|(_, file, dir_url)| PlanPart {
            url: join_url(dir_url, &file.name),
        })
        .collect();
    let joined = parts
        .iter()
        .map(|part| part.url.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(FramePlan {
        identity: format!(
            "{site_id}_{}_p{}_h{:016x}",
            anchor.format("%Y%m%d%H%M%S"),
            parts.len(),
            fnv1a64(&joined)
        ),
        parts,
        merge: true,
    })
}

/// One CHMI data file: scan task letter, start time, and verbatim name.
#[derive(Clone, Debug, PartialEq)]
struct ChmiFile {
    task: char,
    time: NaiveDateTime,
    name: String,
}

/// Parse `T_<TTAAii>_C_<center>_<YYYYMMDDHHMMSS>.hdf` listing entries. The
/// scan task is the fourth letter of the bulletin header (`PAGZ60` -> `Z`).
fn parse_chmi_files(entries: &[ListingEntry]) -> Vec<ChmiFile> {
    entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .filter_map(|entry| {
            let mut segments = entry.name.split('_');
            if segments.next() != Some("T") {
                return None;
            }
            let bulletin = segments.next()?;
            let task = bulletin.chars().nth(3)?;
            if !task.is_ascii_uppercase() {
                return None;
            }
            let stamp = digit_run(&entry.name, 14)?;
            let time = NaiveDateTime::parse_from_str(stamp, "%Y%m%d%H%M%S").ok()?;
            Some(ChmiFile {
                task,
                time,
                name: entry.name.clone(),
            })
        })
        .collect()
}

/// Newest file per scan task from the trailing freshness window through the
/// small post-Z upload allowance, unordered (the caller sorts the picks).
fn freshest_per_task(files: &[ChmiFile], anchor: NaiveDateTime) -> Vec<ChmiFile> {
    let window_start = anchor - chrono::Duration::minutes(FRESHNESS_WINDOW_MINUTES);
    let window_end = anchor + chrono::Duration::minutes(SUPPLEMENTAL_LEAD_MINUTES);
    let mut newest_per_task: Vec<ChmiFile> = Vec::new();
    for file in files {
        if file.time < window_start || file.time > window_end {
            continue;
        }
        match newest_per_task
            .iter_mut()
            .find(|chosen| chosen.task == file.task)
        {
            Some(chosen) => {
                if file.time > chosen.time {
                    *chosen = file.clone();
                }
            }
            None => newest_per_task.push(file.clone()),
        }
    }
    newest_per_task
}

/// Merge precedence of the scan tasks: the full 12-cut volume (`Z`) is the
/// base, then the 5-minute supplemental cut (`B`), then the 10-minute one
/// (`A`); unknown future tasks go last.
fn task_rank(task: char) -> u8 {
    match task {
        'Z' => 0,
        'B' => 1,
        'A' => 2,
        _ => 3,
    }
}

fn is_safe_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITES_ROOT: &str = include_str!("../../tests/fixtures/chmi_sites_root.html");
    const BRD_PRODUCTS: &str = include_str!("../../tests/fixtures/chmi_brd_products.html");
    const BRD_VOL_Z: &str = include_str!("../../tests/fixtures/chmi_brd_vol_z_files.html");
    const BRD_VOL_V: &str = include_str!("../../tests/fixtures/chmi_brd_vol_v_files.html");

    fn timestamp(value: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M%S").expect("test timestamp")
    }

    #[test]
    fn live_capture_lists_both_stations_and_all_products() {
        let sites: Vec<String> = parse_autoindex(SITES_ROOT)
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(sites, ["brd", "ska"]);

        let products = parse_autoindex(BRD_PRODUCTS);
        for product in &CHMI_PRODUCTS {
            assert!(
                products
                    .iter()
                    .any(|entry| entry.is_dir && entry.name == product.dir),
                "{}",
                product.dir
            );
        }
    }

    #[test]
    fn file_parser_reads_task_letter_and_stamp() {
        let files = parse_chmi_files(&parse_autoindex(BRD_VOL_Z));
        // Trimmed capture: 4 stamps per task family (PAGZ60/PAYA60/PAYB60).
        assert_eq!(files.len(), 12);
        for task in ['Z', 'A', 'B'] {
            assert_eq!(files.iter().filter(|file| file.task == task).count(), 4);
        }
        assert!(files.contains(&ChmiFile {
            task: 'B',
            time: timestamp("20260612064025"),
            name: "T_PAYB60_C_OKPR_20260612064025.hdf".to_owned(),
        }));
    }

    #[test]
    fn anchor_and_window_pick_the_newest_file_of_each_task() {
        let vol_z = parse_chmi_files(&parse_autoindex(BRD_VOL_Z));
        let anchor = vol_z
            .iter()
            .filter(|file| file.task == 'Z')
            .map(|file| file.time)
            .max()
            .expect("full-volume anchor");
        assert_eq!(anchor, timestamp("20260612063911"));

        let mut picks = freshest_per_task(&vol_z, anchor);
        picks.sort_by_key(|file| task_rank(file.task));
        let summary: Vec<(char, NaiveDateTime)> =
            picks.iter().map(|file| (file.task, file.time)).collect();
        assert_eq!(
            summary,
            vec![
                ('Z', timestamp("20260612063911")),
                ('B', timestamp("20260612064025")),
                ('A', timestamp("20260612063948")),
            ]
        );

        // vol_v task stamps pair with vol_z per task (PAHB == PAYB stamps).
        let vol_v = parse_chmi_files(&parse_autoindex(BRD_VOL_V));
        let mut v_picks = freshest_per_task(&vol_v, anchor);
        v_picks.sort_by_key(|file| task_rank(file.task));
        assert_eq!(
            v_picks.iter().map(|file| file.time).collect::<Vec<_>>(),
            picks.iter().map(|file| file.time).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stale_tasks_fall_out_of_the_freshness_window() {
        let files = vec![
            ChmiFile {
                task: 'Z',
                time: timestamp("20260612064000"),
                name: "z.hdf".to_owned(),
            },
            ChmiFile {
                task: 'A',
                time: timestamp("20260612063000"),
                name: "a-fresh.hdf".to_owned(),
            },
            ChmiFile {
                task: 'B',
                time: timestamp("20260612052000"),
                name: "b-stale.hdf".to_owned(),
            },
        ];
        let picks = freshest_per_task(&files, timestamp("20260612064000"));
        assert_eq!(picks.len(), 2);
        assert!(picks.iter().any(|file| file.name == "z.hdf"));
        assert!(picks.iter().any(|file| file.name == "a-fresh.hdf"));
    }

    #[test]
    fn supplemental_file_from_the_next_cycle_cannot_advance_current_frame() {
        let files = vec![
            ChmiFile {
                task: 'Z',
                time: timestamp("20260612064000"),
                name: "z-current.hdf".to_owned(),
            },
            ChmiFile {
                task: 'B',
                time: timestamp("20260612064100"),
                name: "b-current.hdf".to_owned(),
            },
            ChmiFile {
                task: 'B',
                time: timestamp("20260612064600"),
                name: "b-next.hdf".to_owned(),
            },
        ];
        let picks = freshest_per_task(&files, timestamp("20260612064000"));
        assert!(picks.iter().any(|file| file.name == "b-current.hdf"));
        assert!(!picks.iter().any(|file| file.name == "b-next.hdf"));
    }

    #[test]
    fn task_order_puts_the_full_volume_first() {
        assert!(task_rank('Z') < task_rank('B'));
        assert!(task_rank('B') < task_rank('A'));
        assert!(task_rank('A') < task_rank('Q'));
    }

    /// The provider must advertise the real loop it now has (fails on the
    /// old single-frame ChmiProvider).
    #[test]
    fn provider_advertises_a_real_recent_loop() {
        let provider = ChmiProvider::new();
        assert!(provider.recent_source().is_some());
        assert!(provider.supports_recent());
    }

    /// Anchors walk task-Z cycles NEWEST FIRST and require the full-volume
    /// timestamp to be present in both required products.
    #[test]
    fn frame_anchors_walk_full_volume_cycles_newest_first() {
        let listings = fixture_listings();
        let latest_anchor = timestamp("20260612063911");

        assert_eq!(chmi_frame_anchors(&listings, 1), vec![latest_anchor]);
        assert_eq!(
            chmi_frame_anchors(&listings, 3),
            vec![
                timestamp("20260612063911"),
                timestamp("20260612063412"),
                timestamp("20260612062914"),
            ]
        );
        assert_eq!(
            chmi_frame_anchors(&listings, 99),
            vec![
                timestamp("20260612063911"),
                timestamp("20260612063412"),
                timestamp("20260612062914"),
                timestamp("20260612062411"),
            ]
        );
        assert!(chmi_frame_anchors(&[], 3).is_empty());
    }

    #[test]
    fn newest_full_volume_missing_from_velocity_falls_back_to_prior_cycle() {
        let mut listings = fixture_listings();
        listings[1]
            .files
            .retain(|file| !(file.task == 'Z' && file.time == timestamp("20260612063911")));
        assert_eq!(
            chmi_frame_anchors(&listings, 1),
            vec![timestamp("20260612063412")]
        );
    }

    fn fixture_listings() -> Vec<ChmiProductListing> {
        vec![
            ChmiProductListing {
                product_rank: 0,
                dir: "vol_z",
                required: true,
                dir_url: "https://opendata.chmi.cz/meteorology/weather/radar/sites/brd/vol_z/hdf5/"
                    .to_owned(),
                files: parse_chmi_files(&parse_autoindex(BRD_VOL_Z)),
            },
            ChmiProductListing {
                product_rank: 1,
                dir: "vol_v",
                required: true,
                dir_url: "https://opendata.chmi.cz/meteorology/weather/radar/sites/brd/vol_v/hdf5/"
                    .to_owned(),
                files: parse_chmi_files(&parse_autoindex(BRD_VOL_V)),
            },
        ]
    }

    /// Older frames assemble from the same listings with the same identity
    /// grammar and task-major part order as the newest one, and stay
    /// identity-stable across repeated assembly.
    #[test]
    fn older_anchors_assemble_full_frames_from_the_same_listings() {
        let listings = fixture_listings();
        let anchors = chmi_frame_anchors(&listings, 2);
        assert_eq!(anchors.len(), 2);

        let newest = assemble_frame("brd", anchors[0], &listings).expect("newest frame");
        assert!(newest.identity.starts_with("brd_20260612063911_p6_h"));

        let older = assemble_frame("brd", anchors[1], &listings).expect("older frame");
        assert!(older.identity.starts_with("brd_20260612063412_p6_h"));
        assert_ne!(older.identity, newest.identity);
        assert!(older.merge);
        // Task-major order, vol_z before vol_v inside each task; the older
        // cycle picks Z 06:34:12, B 06:35:26, A 06:29:48.
        let names: Vec<&str> = older
            .parts
            .iter()
            .map(|part| part.url.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "T_PAGZ60_C_OKPR_20260612063412.hdf",
                "T_PAHZ60_C_OKPR_20260612063412.hdf",
                "T_PAYB60_C_OKPR_20260612063526.hdf",
                "T_PAHB60_C_OKPR_20260612063526.hdf",
                "T_PAYA60_C_OKPR_20260612062948.hdf",
                "T_PAHA60_C_OKPR_20260612062948.hdf",
            ]
        );
        // Same upstream frame -> same plan (dedupe key stability).
        assert_eq!(
            assemble_frame("brd", anchors[1], &listings).expect("older frame again"),
            older
        );
    }
}
