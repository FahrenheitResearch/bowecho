//! Site picker (miniderecho-spec §11 M1): a thin, searchable list over
//! `data_source::sites` — nearest-first when a location is known, US-only
//! per the v1 scope (honest scope, not a greyed graveyard; intl lands
//! v1.x through the same catalog). The ranking/filtering is pure and
//! unit-tested; the egui window is a thin shell over it. Deliberately NOT
//! BowEcho's picker, which is welded to `Vec<RadarSite>` indices that
//! v0.29 Phase 3 deletes.

use data_source::sites::{self, SiteRecord, SiteRef};
use eframe::egui;

use crate::geoloc::site_is_us_live;

/// One picker row: a US catalog site plus its distance from the known
/// location (when there is one).
#[derive(Clone, Debug, PartialEq)]
pub struct PickerRow {
    pub record: SiteRecord,
    pub distance_km: Option<f32>,
}

/// Pure search/rank over the compiled-in catalog (network-free, UI-thread
/// safe): US live-scope sites only, case-insensitive substring match on
/// the label (which embeds the id: "KTLX Norman"), nearest-first when a
/// location is known, else label order. Empty query = the whole catalog.
pub fn picker_rows(query: &str, near: Option<(f32, f32)>) -> Vec<PickerRow> {
    let needle = query.trim().to_ascii_lowercase();
    let mut rows: Vec<PickerRow> = sites::all_sites()
        .filter(|record| site_is_us_live(&record.kind))
        .filter(|record| needle.is_empty() || record.label.to_ascii_lowercase().contains(&needle))
        .map(|record| {
            let distance_km = match (near, record.lat_lon) {
                (Some((lat, lon)), Some((site_lat, site_lon))) => {
                    Some(haversine_km(lat, lon, site_lat, site_lon))
                }
                _ => None,
            };
            PickerRow {
                record,
                distance_km,
            }
        })
        .collect();
    rows.sort_by(|a, b| match (a.distance_km, b.distance_km) {
        (Some(da), Some(db)) => da
            .total_cmp(&db)
            .then_with(|| a.record.label.cmp(&b.record.label)),
        _ => a.record.label.cmp(&b.record.label),
    });
    rows
}

/// Great-circle distance, same formulation as `sites::sites_near` so the
/// picker's ordering agrees with the first-run chain (Sinnott 1984,
/// "Virtues of the Haversine", Sky & Telescope 68(2):159).
fn haversine_km(lat_a: f32, lon_a: f32, lat_b: f32, lon_b: f32) -> f32 {
    let earth_radius_km = 6371.0_f32;
    let d_lat = (lat_b - lat_a).to_radians();
    let d_lon = (lon_b - lon_a).to_radians();
    let lat_a = lat_a.to_radians();
    let lat_b = lat_b.to_radians();
    let a = (d_lat / 2.0).sin().powi(2) + lat_a.cos() * lat_b.cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * earth_radius_km * a.sqrt().atan2((1.0 - a).max(0.0).sqrt())
}

/// The picker window state; the SITE bar control toggles it.
#[derive(Default)]
pub struct SitePicker {
    pub open: bool,
    query: String,
}

impl SitePicker {
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
    }

    /// Show the window; returns the picked site, if any. `near` orders the
    /// list nearest-first (geolocation fix when one exists, else the
    /// current site's pad).
    pub fn show(&mut self, ctx: &egui::Context, near: Option<(f32, f32)>) -> Option<SiteRef> {
        if !self.open {
            return None;
        }
        let mut picked = None;
        let mut open = self.open;
        egui::Window::new("Choose site")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search sites…")
                        .desired_width(f32::INFINITY),
                );
                // Focus the search box when the picker opens (F = find).
                if !response.has_focus() && self.query.is_empty() && picked.is_none() {
                    response.request_focus();
                }
                ui.add_space(4.0);
                let rows = picker_rows(&self.query, near);
                if rows.is_empty() {
                    ui.weak("No US sites match");
                }
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        for row in &rows {
                            let text = match row.distance_km {
                                Some(distance) => {
                                    format!("{}    {:.0} km", row.record.label, distance)
                                }
                                None => row.record.label.clone(),
                            };
                            // Touch-sized targets (§UX blueprint).
                            let button = egui::Button::new(text)
                                .min_size(egui::vec2(ui.available_width(), 32.0));
                            let mut response = ui.add(button);
                            if let Some(origin) = &row.record.origin {
                                response = response.on_hover_text(origin.clone());
                            }
                            if response.clicked() {
                                picked = Some(row.record.site.clone());
                            }
                        }
                    });
            });
        self.open = open && picked.is_none();
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_source::sites::SiteKind;

    #[test]
    fn rows_are_us_live_scope_only() {
        let rows = picker_rows("", None);
        assert!(!rows.is_empty());
        assert!(
            rows.iter().all(|row| site_is_us_live(&row.record.kind)
                && !matches!(row.record.kind, SiteKind::Intl { .. })),
            "v1 picker lists only the US live scope"
        );
        // Nothing international leaks through even by name.
        assert!(
            rows.iter()
                .all(|row| !row.record.site.settings_key().starts_with("intl:"))
        );
    }

    #[test]
    fn search_is_case_insensitive_over_id_and_name() {
        for query in ["ktlx", "KTLX", "norman", "NORMAN"] {
            let rows = picker_rows(query, None);
            assert!(
                rows.iter().any(|row| row.record.label == "KTLX Norman"),
                "query {query:?} finds KTLX"
            );
        }
        assert!(picker_rows("zzzznope", None).is_empty());
        // Without a location the order is the label order.
        let rows = picker_rows("", None);
        for pair in rows.windows(2) {
            assert!(pair[0].record.label <= pair[1].record.label);
        }
        assert!(rows.iter().all(|row| row.distance_km.is_none()));
    }

    #[test]
    fn known_location_orders_nearest_first_with_distances() {
        // Norman, OK: KTLX and TOKC lead the list.
        let rows = picker_rows("", Some((35.3, -97.3)));
        for pair in rows.windows(2) {
            assert!(pair[0].distance_km.unwrap() <= pair[1].distance_km.unwrap());
        }
        let leaders: Vec<&str> = rows
            .iter()
            .take(4)
            .map(|row| row.record.label.as_str())
            .collect();
        assert!(leaders.contains(&"KTLX Norman"), "{leaders:?}");
        // The search still applies on top of the ordering.
        let filtered = picker_rows("wichita", Some((35.3, -97.3)));
        assert!(!filtered.is_empty());
        assert!(
            filtered
                .iter()
                .all(|row| row.record.label.to_ascii_lowercase().contains("wichita"))
        );
    }
}
