//! Save/load for annotation sets: versioned, geo-anchored JSON documents,
//! one named file per set in the config-dir `annotations` folder
//! (`settings::annotations_dir()`). Because anchors are lon/lat, a saved
//! analysis reloads correctly over any site, zoom, or basemap — the
//! geo-anchored equivalent of GBW Overlay's Save button.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Annotation;

pub(crate) const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct AnnotationSetFile {
    version: u32,
    shapes: Vec<Annotation>,
}

pub(crate) fn to_json(shapes: &[Annotation]) -> String {
    serde_json::to_string_pretty(&AnnotationSetFile {
        version: FORMAT_VERSION,
        shapes: shapes.to_vec(),
    })
    .unwrap_or_else(|_| "{}".to_owned())
}

pub(crate) fn from_json(text: &str) -> Result<Vec<Annotation>, String> {
    serde_json::from_str::<AnnotationSetFile>(text)
        .map(|file| file.shapes)
        .map_err(|error| error.to_string())
}

/// Filesystem-safe set name: keeps alphanumerics, `-` and `_`; whitespace
/// becomes `_`; everything else is dropped. Empty input gets a default.
pub(crate) fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                Some(c)
            } else if c.is_whitespace() {
                Some('_')
            } else {
                None
            }
        })
        .collect();
    if cleaned.is_empty() {
        "annotations".to_owned()
    } else {
        cleaned
    }
}

pub(crate) fn save_named(dir: &Path, name: &str, shapes: &[Annotation]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let path = dir.join(format!("{}.json", sanitize_name(name)));
    std::fs::write(&path, to_json(shapes)).map_err(|error| error.to_string())?;
    Ok(path)
}

/// Saved set names (file stems), sorted, best-effort.
pub(crate) fn list_saved(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|e| e.to_str()) == Some("json"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    names.sort_unstable();
    names
}

pub(crate) fn load_named(dir: &Path, name: &str) -> Result<Vec<Annotation>, String> {
    let path = dir.join(format!("{name}.json"));
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    from_json(&text)
}

/// One of every shape kind, with non-default style fields exercised.
/// Shared test fixture (also painted end-to-end by the draw tests).
#[cfg(test)]
pub(crate) fn every_shape_kind() -> Vec<Annotation> {
    use super::{FrontKind, GeoPoint, IconKind, ShapeStyle, WarnKind, WatchKind};

    fn p(lon: f32, lat: f32) -> GeoPoint {
        GeoPoint { lon, lat }
    }
    let styled = ShapeStyle {
        thickness: 4.5,
        opacity: 0.7,
        color: Some([0, 200, 255]),
    };
    vec![
        Annotation::Crosshair {
            at: p(-97.5, 35.2),
            style: ShapeStyle::default(),
        },
        Annotation::Box {
            a: p(-98.0, 34.0),
            b: p(-97.0, 35.0),
            style: styled,
        },
        Annotation::Arrow {
            tail: p(-98.2, 34.1),
            head: p(-97.4, 34.8),
            style: ShapeStyle::default(),
        },
        Annotation::RangeCircle {
            center: p(-97.8, 34.3),
            edge: p(-97.4, 34.3),
            style: styled,
        },
        Annotation::Freehand {
            points: vec![p(-98.0, 34.0), p(-97.9, 34.1), p(-97.7, 34.05)],
            style: styled,
        },
        Annotation::Text {
            at: p(-97.6, 34.9),
            text: "Hook echo here".to_owned(),
            style: styled,
        },
        Annotation::Front {
            front: FrontKind::Cold,
            points: vec![p(-99.0, 36.0), p(-98.0, 35.5), p(-97.0, 35.6)],
            flip: true,
            pips: false,
            style: ShapeStyle::default(),
        },
        Annotation::Front {
            front: FrontKind::Outflow,
            points: vec![p(-98.5, 34.4), p(-98.0, 34.2)],
            flip: false,
            pips: true,
            style: styled,
        },
        Annotation::FlowArrow {
            points: vec![p(-99.0, 34.0), p(-98.5, 34.6), p(-97.8, 34.4)],
            style: styled,
        },
        Annotation::WatchBox {
            watch: WatchKind::Tor,
            a: p(-99.5, 33.5),
            b: p(-97.5, 35.5),
            hatch: true,
            label: None,
            style: ShapeStyle::default(),
        },
        Annotation::WatchBox {
            watch: WatchKind::Free,
            a: p(-99.0, 33.0),
            b: p(-98.2, 33.8),
            hatch: true,
            label: Some("PDS AREA".to_owned()),
            style: styled,
        },
        Annotation::WarnPolygon {
            warn: WarnKind::Svr,
            points: vec![p(-98.0, 34.0), p(-97.5, 34.4), p(-97.2, 33.9)],
            label: Some("MACROBURST".to_owned()),
            style: styled,
        },
        Annotation::Icon {
            icon: IconKind::Meso,
            at: p(-97.9, 34.6),
            style: ShapeStyle::default(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trips_every_shape_kind() {
        let shapes = every_shape_kind();
        let json = to_json(&shapes);
        let loaded = from_json(&json).expect("round-trip parses");
        assert_eq!(loaded, shapes);
        // The document is versioned for forward evolution.
        assert!(json.contains("\"version\": 1"));
    }

    #[test]
    fn json_kind_tags_are_stable_snake_case() {
        let json = to_json(&every_shape_kind());
        for tag in [
            "crosshair",
            "box",
            "arrow",
            "range_circle",
            "freehand",
            "text",
            "front",
            "flow_arrow",
            "watch_box",
            "warn_polygon",
            "icon",
        ] {
            assert!(
                json.contains(&format!("\"kind\": \"{tag}\"")),
                "missing kind tag {tag}"
            );
        }
        // Geo anchoring: lon/lat fields, no screen coordinates.
        assert!(json.contains("\"lon\""));
        assert!(json.contains("\"lat\""));
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(from_json("not json").is_err());
        assert!(from_json("{\"version\":1}").is_err());
    }

    #[test]
    fn sanitize_name_keeps_filenames_safe() {
        assert_eq!(
            sanitize_name("Moore TOR 2026-06-11"),
            "Moore_TOR_2026-06-11"
        );
        assert_eq!(sanitize_name("../../evil"), "evil");
        assert_eq!(sanitize_name("  "), "annotations");
        assert_eq!(sanitize_name("dérecho"), "dérecho");
    }

    #[test]
    fn save_list_load_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!(
            "bowecho-annotate-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let shapes = every_shape_kind();
        let path = save_named(&dir, "test set", &shapes).expect("save succeeds");
        assert!(path.ends_with("test_set.json"));
        assert_eq!(list_saved(&dir), vec!["test_set".to_owned()]);
        let loaded = load_named(&dir, "test_set").expect("load succeeds");
        assert_eq!(loaded, shapes);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
