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
//! timestamp" cannot bind a frame. Instead the newest vol_z file anchors
//! the frame and each product contributes its newest file per task inside
//! a trailing freshness window sized to cover the slowest (10-minute)
//! task. Parts are ordered task-major — full volumes (`Z`) first so the
//! 12-cut reflectivity PVOL is the merge base, then `B`, then `A`. The
//! supplemental cuts either union in as new elevations (0.3°) or collide
//! with a same-elevation full-volume cut of different gate geometry
//! (1.5°), which `radar_core::merge_radar_volumes` reports as
//! `skipped_geometry` rather than mixing geometries.

use chrono::NaiveDateTime;

use super::listing::{ListingEntry, digit_run, fnv1a64, join_url, parse_autoindex};
use super::{FramePlan, IntlProvider, IntlSite, PlanPart};
use crate::{fetch_listing_text, fetch_text};

const CHMI_SITES_ROOT: &str = "https://opendata.chmi.cz/meteorology/weather/radar/sites/";
/// Freshness window: must cover the 10-minute `A` task (plus jitter) but
/// stay short enough that stale tasks drop out instead of showing old air.
const FRESHNESS_WINDOW_MINUTES: i64 = 12;

struct ChmiProduct {
    dir: &'static str,
    required: bool,
}

/// Product directories to assemble, in merge order per task.
const CHMI_PRODUCTS: [ChmiProduct; 5] = [
    ChmiProduct {
        dir: "vol_z",
        required: true,
    },
    ChmiProduct {
        dir: "vol_v",
        required: true,
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

        // Per product: the parsed file list and the directory it came from.
        let mut anchor: Option<NaiveDateTime> = None;
        let mut picks: Vec<(usize, ChmiFile, String)> = Vec::new();
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

            // The newest vol_z file (any task) anchors the frame.
            if anchor.is_none() {
                anchor = files.iter().map(|file| file.time).max();
            }
            let Some(anchor) = anchor else {
                return Err(format!(
                    "CHMI {}: could not resolve a frame anchor from {dir_url}",
                    product.dir
                ));
            };

            let fresh = freshest_per_task(&files, anchor);
            if product.required && fresh.is_empty() {
                return Err(format!(
                    "CHMI {}/{site_id}: no files within {FRESHNESS_WINDOW_MINUTES} minutes \
                     of the frame anchor {anchor} ({} files inspected)",
                    product.dir,
                    files.len()
                ));
            }
            picks.extend(
                fresh
                    .into_iter()
                    .map(|file| (product_rank, file, dir_url.clone())),
            );
        }

        let Some(anchor) = anchor else {
            return Err(format!("CHMI: no products resolved for site '{site_id}'"));
        };
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

/// Newest file per scan task within the freshness window ending at
/// `anchor`, unordered (the caller sorts the combined picks).
fn freshest_per_task(files: &[ChmiFile], anchor: NaiveDateTime) -> Vec<ChmiFile> {
    let window_start = anchor - chrono::Duration::minutes(FRESHNESS_WINDOW_MINUTES);
    let mut newest_per_task: Vec<ChmiFile> = Vec::new();
    for file in files {
        if file.time < window_start || file.time > anchor {
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
        let anchor = vol_z.iter().map(|file| file.time).max().expect("anchor");
        assert_eq!(anchor, timestamp("20260612064025"));

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
    fn task_order_puts_the_full_volume_first() {
        assert!(task_rank('Z') < task_rank('B'));
        assert!(task_rank('B') < task_rank('A'));
        assert!(task_rank('A') < task_rank('Q'));
    }
}
