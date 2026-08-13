//! Pure product/cut-selection helpers moved verbatim out of `main.rs`
//! (v0.29.4 decomposition, queue item #4): which products a volume/cut can
//! display, picker ordering/visibility, sweep-cut policy filtering, live-tilt
//! completeness gates, and latest-load selection/prefetch math — params in,
//! value out, no `ViewerApp` state. Types and constants stay in `main.rs`;
//! this module reaches them via `crate::`.
//!
//! Every body moved VERBATIM from main.rs — the only edits are `use` lines
//! and the pub(crate) promotions listed in the extraction commit message.

use crate::*;

fn product_order(available: &std::collections::BTreeSet<MomentType>) -> Vec<DisplayProduct> {
    let mut ordered = Vec::new();
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::CorrelationCoefficient,
        MomentType::DifferentialReflectivity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialPhase,
        MomentType::SpecificDifferentialPhase,
    ] {
        if available.contains(&moment) {
            if moment == MomentType::Velocity {
                ordered.push(DisplayProduct::Moment(MomentType::Velocity));
                ordered.push(DisplayProduct::DealiasedVelocity);
                ordered.push(DisplayProduct::StormRelativeVelocity);
                ordered.push(DisplayProduct::StormRelativeDealiasedVelocity);
            } else {
                ordered.push(DisplayProduct::Moment(moment));
            }
        }
    }
    for moment in available {
        if advanced_derived_product_for_moment(moment).is_some() {
            continue;
        }
        let product = DisplayProduct::Moment(moment.clone());
        if !ordered.contains(&product) {
            ordered.push(product);
        }
    }
    ordered
}

fn picker_product_rank(product: &DisplayProduct) -> (u16, &str) {
    let rank = match product {
        DisplayProduct::Moment(MomentType::Reflectivity) => 10,
        DisplayProduct::Moment(MomentType::Velocity) => 20,
        DisplayProduct::DealiasedVelocity => 21,
        DisplayProduct::StormRelativeVelocity => 30,
        DisplayProduct::StormRelativeDealiasedVelocity => 31,
        DisplayProduct::Moment(MomentType::CorrelationCoefficient) => 40,
        DisplayProduct::Moment(MomentType::DifferentialReflectivity) => 50,
        DisplayProduct::Moment(MomentType::SpectrumWidth) => 60,
        DisplayProduct::Moment(MomentType::DifferentialPhase) => 70,
        DisplayProduct::Moment(MomentType::SpecificDifferentialPhase) => 80,
        // Keep the simulated-radar stage, support, and difference fields below
        // the ordinary derived-product block. Their 81..116 ranks are a
        // deliberate scientific progression; sharing the old 100/110 ranks
        // made CREF/ET interleave with that progression by label.
        DisplayProduct::Derived(DerivedProduct::CompositeReflectivity) => 120,
        DisplayProduct::Derived(DerivedProduct::EchoTops) => 130,
        DisplayProduct::Derived(DerivedProduct::Vil) => 140,
        DisplayProduct::Derived(DerivedProduct::VilDensity) => 141,
        DisplayProduct::Derived(DerivedProduct::Mehs) => 150,
        DisplayProduct::Derived(DerivedProduct::Posh) => 151,
        DisplayProduct::Derived(DerivedProduct::Poh) => 152,
        DisplayProduct::Derived(DerivedProduct::Marc) => 160,
        DisplayProduct::Derived(DerivedProduct::GustProxy) => 170,
        DisplayProduct::Derived(DerivedProduct::AzimuthalShear) => 180,
        DisplayProduct::Derived(DerivedProduct::Divergence) => 181,
        DisplayProduct::Moment(MomentType::Unknown(name)) => match name.as_str() {
            "IREF" => 81,
            "IVEL" => 82,
            "ISW" => 83,
            "IZDR" => 84,
            "IRHO" => 85,
            "IKDP" => 86,
            "MREF" => 87,
            "MVEL" => 88,
            "MSW" => 89,
            "MZDR" => 90,
            "MRHO" => 91,
            "MKDP" => 92,
            "MCOV" => 100,
            "TUNB" => 101,
            "MSIG" => 102,
            "DIF_REF" => 110,
            "DIF_VEL" => 111,
            "DIF_SW" => 112,
            "DIF_ZDR" => 113,
            "DIF_RHO" => 114,
            "DIF_PHI" => 115,
            "DIF_KDP" => 116,
            "PHIF" => 200,
            "KDP_SD" => 201,
            "AH" => 210,
            "PIA" => 211,
            "CREF" => 212,
            "ADP" => 220,
            "PIDA" => 221,
            "ZDRC" => 222,
            "RATE_Z" => 230,
            "RATE_KDP" => 231,
            "RATE" => 232,
            "LWC" => 240,
            "HKE" => 241,
            "CDR" => 250,
            "L_RHO" => 251,
            "REF_TEX" => 260,
            "VEL_TEX" => 261,
            "SW_TEX" => 262,
            "ZDR_TEX" => 263,
            "RHO_TEX" => 264,
            "PHI_TEX" => 265,
            "KDP_TEX" => 266,
            "REF_GRAD_R" => 270,
            "VEL_GRAD_R" => 271,
            "MET_QI" => 280,
            "MET_MASK" => 281,
            "TDS_SCORE" => 290,
            "HAIL_SCORE" => 291,
            "TURB" => 292,
            _ => 900,
        },
    };
    (
        rank,
        validation_product_label(product).unwrap_or_else(|| product.label()),
    )
}

/// Human-facing names for synthetic-radar support and validation fields.
/// The stable ids remain the data contract and are included in the labels so
/// exports, screenshots, and scientific discussion stay unambiguous.
pub(crate) fn validation_product_label(product: &DisplayProduct) -> Option<&'static str> {
    let DisplayProduct::Moment(MomentType::Unknown(name)) = product else {
        return None;
    };
    Some(match name.as_str() {
        "IREF" => "Ideal reflectivity (IREF)",
        "IVEL" => "Ideal velocity (IVEL)",
        "ISW" => "Ideal spectrum width (ISW)",
        "IZDR" => "Ideal differential reflectivity (IZDR)",
        "IRHO" => "Ideal correlation coefficient (IRHO)",
        "IKDP" => "Ideal specific differential phase (IKDP)",
        "MREF" => "Measured reflectivity (MREF)",
        "MVEL" => "Measured velocity (MVEL)",
        "MSW" => "Measured spectrum width (MSW)",
        "MZDR" => "Measured differential reflectivity (MZDR)",
        "MRHO" => "Measured correlation coefficient (MRHO)",
        "MKDP" => "Measured specific differential phase (MKDP)",
        "MCOV" => "Model coverage (MCOV)",
        "TUNB" => "Terrain unblocked (TUNB)",
        "MSIG" => "Meteorological signal (MSIG)",
        "DIF_REF" => "Reflectivity difference (sim - obs)",
        "DIF_VEL" => "Velocity difference (sim - obs)",
        "DIF_SW" => "Spectrum-width difference (sim - obs)",
        "DIF_ZDR" => "ZDR difference (sim - obs)",
        "DIF_RHO" => "RHOHV difference (sim - obs)",
        "DIF_PHI" => "PHIDP difference (sim - obs)",
        "DIF_KDP" => "KDP difference (sim - obs)",
        _ => return None,
    })
}

/// Physical display units for the validation fields. Quality moments are
/// fractions, not percentages; this intentionally matches their 0..1 grid
/// encoding and palette domain.
pub(crate) fn validation_product_units(product: &DisplayProduct) -> Option<&'static str> {
    let DisplayProduct::Moment(MomentType::Unknown(name)) = product else {
        return None;
    };
    Some(match name.as_str() {
        "IREF" | "MREF" => "dBZ",
        "IVEL" | "MVEL" | "ISW" | "MSW" => "m/s",
        "IZDR" | "MZDR" => "dB",
        "IRHO" | "MRHO" => "fraction",
        "IKDP" | "MKDP" => "deg/km",
        "MCOV" | "TUNB" | "MSIG" | "DIF_RHO" => "fraction",
        "DIF_REF" => "dBZ",
        "DIF_VEL" | "DIF_SW" => "m/s",
        "DIF_ZDR" => "dB",
        "DIF_PHI" => "deg",
        "DIF_KDP" => "deg/km",
        _ => return None,
    })
}

fn retain_non_advanced_products(products: &mut Vec<DisplayProduct>) {
    products.retain(|product| advanced_derived_product_for_display_product(product).is_none());
}

fn is_hideable_derived_product(product: &DisplayProduct) -> bool {
    matches!(product, DisplayProduct::Derived(_))
        || advanced_derived_product_for_display_product(product).is_some()
}

/// Stable persisted identity for a radar quick-product favorite.
///
/// The variant prefix is intentional: a volume-derived `CREF` and a native
/// (or source-provided) moment named `CREF` are different products even though
/// their compact picker labels are identical. Legacy settings without a
/// prefix are still accepted by [`resolve_radar_product_favorite`].
pub(crate) fn radar_product_favorite_key(product: &DisplayProduct) -> String {
    match product {
        DisplayProduct::Moment(moment) => {
            let name = moment.short_name();
            let name_collides_with_known_moment = matches!(moment, MomentType::Unknown(_))
                && ["REF", "VEL", "SW", "ZDR", "RHO", "PHI", "KDP"]
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(name));
            let name_uses_reserved_escape = matches!(moment, MomentType::Unknown(_))
                && name
                    .get(.."unknown=".len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("unknown="));
            if name_collides_with_known_moment || name_uses_reserved_escape {
                let encoded = name
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02X}"))
                    .collect::<String>();
                format!("moment:unknown={encoded}")
            } else {
                format!("moment:{name}")
            }
        }
        DisplayProduct::DealiasedVelocity => "display:DVEL".to_owned(),
        DisplayProduct::StormRelativeVelocity => "display:SRV".to_owned(),
        DisplayProduct::StormRelativeDealiasedVelocity => "display:DSRV".to_owned(),
        DisplayProduct::Derived(derived) => format!("derived:{}", derived.label()),
    }
}

fn is_typed_radar_product_favorite(value: &str) -> bool {
    let Some((prefix, _)) = value.trim().split_once(':') else {
        return false;
    };
    ["moment", "display", "derived"]
        .iter()
        .any(|known| known.eq_ignore_ascii_case(prefix))
}

/// Resolve a persisted favorite against the products available in the
/// current picker. Typed identities match exactly (case-insensitively for
/// compatibility with settings normalization). A legacy bare label resolves
/// to the first product in the established picker ranking, so old `CREF`
/// settings continue to select volume-derived composite reflectivity.
pub(crate) fn resolve_radar_product_favorite<'a>(
    favorite: &str,
    products: &'a [DisplayProduct],
) -> Option<&'a DisplayProduct> {
    let favorite = favorite.trim();
    if favorite.is_empty() {
        return None;
    }
    if is_typed_radar_product_favorite(favorite) {
        return products
            .iter()
            .find(|product| radar_product_favorite_key(product).eq_ignore_ascii_case(favorite));
    }

    let mut selected = None;
    for product in products
        .iter()
        .filter(|product| product.label().eq_ignore_ascii_case(favorite))
    {
        if selected
            .is_none_or(|current| picker_product_rank(product) < picker_product_rank(current))
        {
            selected = Some(product);
        }
    }
    selected
}

/// Whether a persisted typed or legacy favorite resolves to `product` within
/// the supplied picker product list.
pub(crate) fn radar_product_matches_favorite(
    favorite: &str,
    product: &DisplayProduct,
    products: &[DisplayProduct],
) -> bool {
    resolve_radar_product_favorite(favorite, products).is_some_and(|resolved| resolved == product)
}

/// Compact chip text that only expands when two products share a short label.
pub(crate) fn radar_product_favorite_caption(
    product: &DisplayProduct,
    products: &[DisplayProduct],
) -> String {
    let label = product.label();
    let duplicate_label = products
        .iter()
        .any(|candidate| candidate != product && candidate.label().eq_ignore_ascii_case(label));
    if !duplicate_label {
        return label.to_owned();
    }
    let qualifier = match product {
        DisplayProduct::Derived(derived) => derived.display_name(),
        DisplayProduct::Moment(moment) => advanced_derived_product_for_moment(moment)
            .map(product_engine::DerivedSweepProduct::display_name)
            .unwrap_or("moment"),
        DisplayProduct::DealiasedVelocity => "dealiased",
        DisplayProduct::StormRelativeVelocity => "storm-relative",
        DisplayProduct::StormRelativeDealiasedVelocity => "storm-relative dealiased",
    };
    format!("{label} ({qualifier})")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RadarQuickProductEntry {
    pub caption: String,
    pub product: Option<DisplayProduct>,
}

fn unavailable_radar_product_favorite_caption(favorite: &str) -> String {
    let favorite = favorite.trim();
    let Some((prefix, value)) = favorite.split_once(':') else {
        return favorite.to_ascii_uppercase();
    };
    if !["moment", "display", "derived"]
        .iter()
        .any(|known| known.eq_ignore_ascii_case(prefix))
    {
        return favorite.to_ascii_uppercase();
    }
    if let Some(encoded) = value.get("unknown=".len()..).filter(|_| {
        value
            .get(.."unknown=".len())
            .is_some_and(|marker| marker.eq_ignore_ascii_case("unknown="))
    }) {
        let decoded = (encoded.len().is_multiple_of(2))
            .then(|| {
                encoded
                    .as_bytes()
                    .chunks_exact(2)
                    .map(|pair| {
                        std::str::from_utf8(pair)
                            .ok()
                            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        decoded
            .filter(|label| !label.trim().is_empty())
            .map_or_else(|| "CUSTOM".to_owned(), |label| label.to_ascii_uppercase())
    } else {
        value.to_ascii_uppercase()
    }
}

fn radar_product_favorite_variant_caption(favorite: &str) -> &'static str {
    let Some((prefix, value)) = favorite.trim().split_once(':') else {
        return "legacy";
    };
    if prefix.eq_ignore_ascii_case("derived") {
        "derived"
    } else if prefix.eq_ignore_ascii_case("display") {
        "display"
    } else if prefix.eq_ignore_ascii_case("moment")
        && value
            .get(.."unknown=".len())
            .is_some_and(|marker| marker.eq_ignore_ascii_case("unknown="))
    {
        "custom moment"
    } else {
        "moment"
    }
}

/// Resolve the persisted mini-strip in exact saved order. Missing products
/// stay in the result with `product = None` so a radar with fewer moments does
/// not make the strip reflow or silently discard a user's customization.
pub(crate) fn radar_quick_product_entries(
    favorites: &[String],
    products: &[DisplayProduct],
) -> Vec<RadarQuickProductEntry> {
    let mut entries = favorites
        .iter()
        .map(|favorite| {
            let product = resolve_radar_product_favorite(favorite, products).cloned();
            let caption = product.as_ref().map_or_else(
                || unavailable_radar_product_favorite_caption(favorite),
                |product| radar_product_favorite_caption(product, products),
            );
            RadarQuickProductEntry { caption, product }
        })
        .collect::<Vec<_>>();
    let base_captions = entries
        .iter()
        .map(|entry| entry.caption.clone())
        .collect::<Vec<_>>();
    for index in 0..entries.len() {
        let duplicate = base_captions
            .iter()
            .enumerate()
            .any(|(candidate, caption)| {
                candidate != index && caption.eq_ignore_ascii_case(&base_captions[index])
            });
        if duplicate {
            entries[index].caption = format!(
                "{} ({})",
                entries[index].caption,
                radar_product_favorite_variant_caption(&favorites[index])
            );
        }
    }
    entries
}

fn known_hideable_products() -> Vec<DisplayProduct> {
    let mut products = DerivedProduct::ALL
        .into_iter()
        .map(DisplayProduct::Derived)
        .collect::<Vec<_>>();
    products.extend(
        product_engine::DerivedSweepProduct::ALL
            .iter()
            .copied()
            .filter_map(advanced_derived_display_product),
    );
    products
}

pub(crate) fn is_product_visible_in_picker(
    product: &DisplayProduct,
    show_derived_products: bool,
    favorite_keys: &[String],
) -> bool {
    show_derived_products || !is_hideable_derived_product(product) || {
        let products = known_hideable_products();
        favorite_keys
            .iter()
            .any(|favorite| radar_product_matches_favorite(favorite, product, &products))
    }
}

fn retain_picker_visible_products(
    products: &mut Vec<DisplayProduct>,
    show_derived_products: bool,
    favorite_keys: &[String],
) {
    if !show_derived_products {
        products.retain(|product| {
            is_product_visible_in_picker(product, show_derived_products, favorite_keys)
        });
    }
}

fn favorites_include_advanced_product(favorite_keys: &[String]) -> bool {
    let products = known_hideable_products();
    favorite_keys.iter().any(|favorite| {
        resolve_radar_product_favorite(favorite, &products)
            .is_some_and(|product| advanced_derived_product_for_display_product(product).is_some())
    })
}

fn append_present_advanced_products(
    products: &mut Vec<DisplayProduct>,
    available: &std::collections::BTreeSet<MomentType>,
) {
    for product in product_engine::DerivedSweepProduct::ALL.iter().copied() {
        let Some(display_product) = advanced_derived_display_product(product) else {
            continue;
        };
        if available.contains(&display_product.base_moment())
            && !products.contains(&display_product)
        {
            products.push(display_product);
        }
    }
}

pub(crate) fn global_displayable_products(volume: &RadarVolume) -> Vec<DisplayProduct> {
    let mut available = std::collections::BTreeSet::new();
    for cut_index in 0..volume.cuts.len() {
        available.extend(
            displayable_products(volume, cut_index)
                .into_iter()
                .map(|product| product.base_moment()),
        );
    }
    let mut products = product_order(&available);
    // product_order only knows raw moments; append derived products (CREF/ET/
    // VIL/AzShr/Div) here so they are reachable from the picker + keyboard cycle,
    // mirroring displayable_products. Offered when their source moment exists.
    for d in DerivedProduct::ALL {
        if available.contains(&d.base_moment()) {
            products.push(DisplayProduct::Derived(d));
        }
    }
    append_present_advanced_products(&mut products, &available);
    products
}

pub(crate) fn global_displayable_products_with_advanced(
    volume: &RadarVolume,
    include_advanced_placeholders: bool,
) -> Vec<DisplayProduct> {
    let mut products = global_displayable_products(volume);
    retain_non_advanced_products(&mut products);
    if !include_advanced_placeholders {
        let mut available = std::collections::BTreeSet::new();
        for cut_index in 0..volume.cuts.len() {
            available.extend(
                displayable_products(volume, cut_index)
                    .into_iter()
                    .map(|product| product.base_moment()),
            );
        }
        append_present_advanced_products(&mut products, &available);
        return products;
    }
    for product in product_engine::DerivedSweepProduct::ALL.iter().copied() {
        let Some(display_product) = advanced_derived_display_product(product) else {
            continue;
        };
        if volume_has_displayable_product(volume, &display_product)
            || volume_has_advanced_product_sources(volume, product)
        {
            products.push(display_product);
        }
    }
    products
}

pub(crate) fn global_displayable_products_for_picker(
    volume: &RadarVolume,
    include_advanced_placeholders: bool,
    show_derived_products: bool,
    favorite_keys: &[String],
) -> Vec<DisplayProduct> {
    let include_advanced_placeholders =
        include_advanced_placeholders || favorites_include_advanced_product(favorite_keys);
    let mut products =
        global_displayable_products_with_advanced(volume, include_advanced_placeholders);
    retain_picker_visible_products(&mut products, show_derived_products, favorite_keys);
    products.sort_by(|left, right| picker_product_rank(left).cmp(&picker_product_rank(right)));
    products
}

pub(crate) fn displayable_products(volume: &RadarVolume, cut_index: usize) -> Vec<DisplayProduct> {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return Vec::new();
    };
    let available = cut
        .moments
        .values()
        .filter(|grid| grid.radial_count() >= displayable_radial_threshold(cut.radials.len()))
        .map(|grid| grid.moment.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let mut products = product_order(&available);
    // Derived products are offered wherever their source moment is present
    // (reflectivity volume products on REF cuts; azimuthal shear on velocity
    // cuts).
    for d in DerivedProduct::ALL {
        if available.contains(&d.base_moment()) {
            products.push(DisplayProduct::Derived(d));
        }
    }
    append_present_advanced_products(&mut products, &available);
    products
}

pub(crate) fn cut_start_time_utc(volume: &RadarVolume, cut_index: usize) -> Option<DateTime<Utc>> {
    let cut = volume.cuts.get(cut_index)?;
    cut.radials
        .iter()
        .filter_map(|radial| {
            radial_collection_time_from_volume_time_utc(
                volume.volume_time,
                radial.time_offset_ms,
                volume.metadata.compression.as_deref(),
            )
        })
        .min()
}

fn radial_collection_time_from_volume_time_utc(
    volume_time: DateTime<Utc>,
    time_offset_ms: i32,
    source_encoding: Option<&str>,
) -> Option<DateTime<Utc>> {
    // ODIM declares an absolute sweep start. Its decoder stores the checked
    // difference from the volume anchor because that remains exact across UTC
    // midnight; do not run those values through the legacy NEXRAD/CfRadial
    // dual-encoding heuristic below.
    if source_encoding == Some("odim-h5") {
        return volume_time
            .checked_add_signed(chrono::Duration::milliseconds(i64::from(time_offset_ms)));
    }
    let midnight = volume_time
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive))?;
    let milliseconds = chrono::Duration::milliseconds(time_offset_ms as i64);
    let midnight_candidate = midnight + milliseconds;
    let relative_candidate = volume_time + milliseconds;
    let midnight_delta = (midnight_candidate - volume_time).num_milliseconds().abs();
    let relative_delta = (relative_candidate - volume_time).num_milliseconds().abs();
    Some(if midnight_delta <= relative_delta {
        midnight_candidate
    } else {
        relative_candidate
    })
}

pub(crate) fn displayable_cuts_for_product(
    volume: &RadarVolume,
    product: &DisplayProduct,
) -> Vec<usize> {
    (0..volume.cuts.len())
        .filter(|index| can_materialize_product_on_cut(volume, *index, product))
        .collect()
}

#[cfg(test)]
pub(crate) fn low_sweep_cuts_for_product(
    volume: &RadarVolume,
    product: &DisplayProduct,
    filter: LowSweepLoopFilter,
) -> Vec<usize> {
    sweep_cuts_for_product(volume, product, legacy_sweep_policy(filter))
}

pub(crate) fn sweep_cuts_for_product(
    volume: &RadarVolume,
    product: &DisplayProduct,
    policy: SweepPolicy,
) -> Vec<usize> {
    let policy = policy.normalized();
    if policy.mode == SweepPolicyMode::Off {
        return Vec::new();
    }
    let min_elevation = policy.min_elevation_deg();
    let max_elevation = policy.max_elevation_deg();
    let mut cuts = (0..volume.cuts.len())
        .filter(|index| {
            volume.cuts.get(*index).is_some_and(|cut| {
                let complete = match policy.mode {
                    SweepPolicyMode::Range => {
                        is_complete_live_candidate_tilt_for_site(cut, &volume.site.id)
                    }
                    SweepPolicyMode::Off => false,
                    SweepPolicyMode::AllLow
                    | SweepPolicyMode::BaseOnly
                    | SweepPolicyMode::SameLevel => {
                        is_complete_live_low_level_tilt_for_site(cut, &volume.site.id)
                    }
                };
                let in_range = policy.mode != SweepPolicyMode::Range
                    || (cut.elevation_deg.is_finite()
                        && cut.elevation_deg >= min_elevation
                        && cut.elevation_deg <= max_elevation);
                complete && in_range && can_materialize_product_on_cut(volume, *index, product)
            })
        })
        .collect::<Vec<_>>();
    cuts.sort_by(|left, right| {
        let left_time = cut_start_time_utc(volume, *left)
            .unwrap_or_else(|| volume.volume_time.with_timezone(&Utc));
        let right_time = cut_start_time_utc(volume, *right)
            .unwrap_or_else(|| volume.volume_time.with_timezone(&Utc));
        left_time.cmp(&right_time).then_with(|| left.cmp(right))
    });

    match policy.mode {
        SweepPolicyMode::Off | SweepPolicyMode::AllLow | SweepPolicyMode::Range => {}
        SweepPolicyMode::BaseOnly => {
            if let Some(min_elevation) = cuts
                .iter()
                .filter_map(|index| volume.cuts.get(*index).map(|cut| cut.elevation_deg))
                .filter(|elevation| elevation.is_finite())
                .min_by(|a, b| a.total_cmp(b))
            {
                cuts.retain(|index| {
                    volume.cuts.get(*index).is_some_and(|cut| {
                        (cut.elevation_deg - min_elevation).abs()
                            <= LOW_SWEEP_FILTER_ELEVATION_TOLERANCE_DEG
                    })
                });
            }
        }
        SweepPolicyMode::SameLevel => {
            cuts = same_elevation_low_sweep_cuts(volume, &cuts);
        }
    }
    cuts
}

/// Pick the dominant repeated elevation cluster independently for each
/// volume. This preserves SAILS/MESO-SAILS repeats without assuming that the
/// nominal base angle is identical in every VCP or scan.
fn same_elevation_low_sweep_cuts(volume: &RadarVolume, cuts: &[usize]) -> Vec<usize> {
    let elevations = cuts
        .iter()
        .filter_map(|index| {
            let elevation = volume.cuts.get(*index)?.elevation_deg;
            elevation.is_finite().then_some((*index, elevation))
        })
        .collect::<Vec<_>>();
    if elevations.is_empty() {
        return cuts.to_vec();
    }

    let tolerance = LOW_SWEEP_FILTER_ELEVATION_TOLERANCE_DEG;
    let mut best: Option<(usize, f32, f32)> = None;
    for (_, anchor) in &elevations {
        let mut count = 0usize;
        let mut spread = 0.0f32;
        for (_, elevation) in &elevations {
            let delta = *elevation - *anchor;
            if delta >= -f32::EPSILON && delta <= tolerance {
                count += 1;
                spread += delta;
            }
        }
        let replace = best.is_none_or(|(best_count, best_spread, best_anchor)| {
            count > best_count
                || (count == best_count
                    && (*anchor < best_anchor - f32::EPSILON
                        || ((*anchor - best_anchor).abs() <= f32::EPSILON
                            && spread < best_spread - f32::EPSILON)))
        });
        if replace {
            best = Some((count, spread, *anchor));
        }
    }

    let Some((_, _, anchor)) = best else {
        return cuts.to_vec();
    };
    cuts.iter()
        .copied()
        .filter(|index| {
            volume.cuts.get(*index).is_some_and(|cut| {
                cut.elevation_deg.is_finite()
                    && cut.elevation_deg >= anchor - f32::EPSILON
                    && cut.elevation_deg <= anchor + tolerance
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn low_sweep_cuts_for_history_entry(
    frame: &FrameHistoryEntry,
    product: &DisplayProduct,
    filter: LowSweepLoopFilter,
    disabled_cuts: &BTreeSet<LowSweepCutKey>,
) -> Vec<usize> {
    sweep_cuts_for_history_entry(frame, product, legacy_sweep_policy(filter), disabled_cuts)
}

pub(crate) fn sweep_cuts_for_history_entry(
    frame: &FrameHistoryEntry,
    product: &DisplayProduct,
    policy: SweepPolicy,
    disabled_cuts: &BTreeSet<LowSweepCutKey>,
) -> Vec<usize> {
    sweep_cuts_for_volume_identity(
        frame.volume.as_ref(),
        &frame.identity,
        product,
        policy,
        disabled_cuts,
    )
}

// `next_frame_index_with_sweep_cuts` (the shared stepper frame-index math)
// died in the 4e stage-(iii) unify: both screen-loop steppers now advance
// through `LoopEngine::advance_loop`, whose range-mode walk is the same
// `1..=len` wrapping search (ui_core::loop_engine::advance, differentially
// proven in differential_4e.rs).

pub(crate) fn sweep_cut_at_or_before_in_frame(
    frame: &FrameHistoryEntry,
    product: &DisplayProduct,
    policy: SweepPolicy,
    disabled_cuts: &BTreeSet<LowSweepCutKey>,
    timeline_time: DateTime<Utc>,
) -> Option<usize> {
    sweep_history_cut_at_or_before(
        std::slice::from_ref(frame),
        product,
        policy,
        disabled_cuts,
        timeline_time,
    )
    .map(|(_, cut)| cut)
}

pub(crate) fn sweep_history_cut_at_or_before(
    frames: &[FrameHistoryEntry],
    product: &DisplayProduct,
    policy: SweepPolicy,
    disabled_cuts: &BTreeSet<LowSweepCutKey>,
    timeline_time: DateTime<Utc>,
) -> Option<(usize, usize)> {
    let mut best: Option<(DateTime<Utc>, usize, usize)> = None;
    for (frame_index, frame) in frames.iter().enumerate() {
        for cut in sweep_cuts_for_history_entry(frame, product, policy, disabled_cuts) {
            let cut_time = cut_start_time_utc(frame.volume.as_ref(), cut)
                .unwrap_or(frame.identity.scan_time_utc);
            if cut_time <= timeline_time
                && best
                    .as_ref()
                    .is_none_or(|(best_time, _, _)| cut_time > *best_time)
            {
                best = Some((cut_time, frame_index, cut));
            }
        }
    }
    best.map(|(_, frame_index, cut)| (frame_index, cut))
}

fn sweep_cuts_for_volume_identity(
    volume: &RadarVolume,
    identity: &FrameIdentity,
    product: &DisplayProduct,
    policy: SweepPolicy,
    disabled_cuts: &BTreeSet<LowSweepCutKey>,
) -> Vec<usize> {
    sweep_cuts_for_product(volume, product, policy)
        .into_iter()
        .filter(|cut| !low_sweep_cut_is_disabled(disabled_cuts, identity, *cut))
        .collect()
}

pub(crate) fn low_sweep_cut_label(volume: &RadarVolume, cut_index: usize) -> String {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return format!("#{cut_index:02}");
    };
    format!("#{cut_index:02} {:.2}", cut.elevation_deg)
}

pub(crate) fn low_sweep_cut_hover_text(volume: &RadarVolume, cut_index: usize) -> String {
    let time = cut_start_time_utc(volume, cut_index)
        .map(|time| time.format("%H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "time unknown".to_owned());
    let Some(cut) = volume.cuts.get(cut_index) else {
        return time;
    };
    format!(
        "Cut #{cut_index:02}, {:.2} deg, {} radials, {time}",
        cut.elevation_deg,
        cut.radials.len()
    )
}

pub(crate) fn live_partial_has_complete_low_level_tilt(volume: &RadarVolume) -> bool {
    volume.cuts.iter().enumerate().any(|(index, cut)| {
        is_complete_live_low_level_tilt_for_site(cut, &volume.site.id)
            && !displayable_products(volume, index).is_empty()
    })
}

fn is_complete_live_low_level_tilt_for_site(cut: &ElevationCut, site_id: &str) -> bool {
    let min_radials = if site_id_is_terminal_radar(site_id) {
        LIVE_COMPLETE_TERMINAL_LOW_LEVEL_TILT_MIN_RADIALS
    } else {
        LIVE_COMPLETE_LOW_LEVEL_TILT_MIN_RADIALS
    };
    is_live_low_level_tilt(cut)
        && cut.radials.len() >= min_radials
        && live_tilt_azimuth_coverage_deg(cut) >= LIVE_COMPLETE_TILT_MIN_AZIMUTH_COVERAGE_DEG
}

fn is_complete_live_tilt(cut: &ElevationCut) -> bool {
    cut.radials.len() >= LIVE_COMPLETE_TILT_MIN_RADIALS
        && live_tilt_azimuth_coverage_deg(cut) >= LIVE_COMPLETE_TILT_MIN_AZIMUTH_COVERAGE_DEG
}

fn live_tilt_azimuth_coverage_deg(cut: &ElevationCut) -> f32 {
    let mut azimuths = cut
        .radials
        .iter()
        .map(|radial| radial.azimuth_deg.rem_euclid(360.0))
        .filter(|azimuth| azimuth.is_finite())
        .collect::<Vec<_>>();
    if azimuths.len() < 2 {
        return 0.0;
    }
    azimuths.sort_by(|left, right| left.total_cmp(right));
    azimuths.dedup_by(|left, right| (*left - *right).abs() < 0.05);
    if azimuths.len() < 2 {
        return 0.0;
    }

    let mut max_gap = 0.0_f32;
    for pair in azimuths.windows(2) {
        max_gap = max_gap.max(pair[1] - pair[0]);
    }
    let wrap_gap = azimuths[0] + 360.0 - azimuths[azimuths.len() - 1];
    max_gap = max_gap.max(wrap_gap);
    360.0 - max_gap
}

fn is_live_low_level_tilt(cut: &ElevationCut) -> bool {
    cut.elevation_deg <= LIVE_LOW_LEVEL_MAX_ELEVATION_DEG
}

fn is_allowed_live_low_level_tilt_for_site(
    cut: &ElevationCut,
    site_id: &str,
    allow_incomplete: bool,
) -> bool {
    if allow_incomplete {
        is_live_low_level_tilt(cut)
    } else {
        is_complete_live_low_level_tilt_for_site(cut, site_id)
    }
}

pub(crate) fn product_hotkey_egui_key(name: &str) -> Option<egui::Key> {
    match name.trim().to_ascii_uppercase().as_str() {
        "1" => Some(egui::Key::Num1),
        "2" => Some(egui::Key::Num2),
        "3" => Some(egui::Key::Num3),
        "4" => Some(egui::Key::Num4),
        "5" => Some(egui::Key::Num5),
        "6" => Some(egui::Key::Num6),
        "7" => Some(egui::Key::Num7),
        "8" => Some(egui::Key::Num8),
        "9" => Some(egui::Key::Num9),
        "0" => Some(egui::Key::Num0),
        "A" => Some(egui::Key::A),
        "B" => Some(egui::Key::B),
        "C" => Some(egui::Key::C),
        "D" => Some(egui::Key::D),
        "E" => Some(egui::Key::E),
        "F" => Some(egui::Key::F),
        "G" => Some(egui::Key::G),
        "H" => Some(egui::Key::H),
        "I" => Some(egui::Key::I),
        "J" => Some(egui::Key::J),
        "K" => Some(egui::Key::K),
        "L" => Some(egui::Key::L),
        "M" => Some(egui::Key::M),
        "N" => Some(egui::Key::N),
        "O" => Some(egui::Key::O),
        "P" => Some(egui::Key::P),
        "Q" => Some(egui::Key::Q),
        "R" => Some(egui::Key::R),
        "S" => Some(egui::Key::S),
        "T" => Some(egui::Key::T),
        "U" => Some(egui::Key::U),
        "V" => Some(egui::Key::V),
        "W" => Some(egui::Key::W),
        "X" => Some(egui::Key::X),
        "Y" => Some(egui::Key::Y),
        "Z" => Some(egui::Key::Z),
        _ => None,
    }
}

pub(crate) fn product_hotkey_sort_key(name: &str) -> (u8, u8, String) {
    let normalized = name.trim().to_ascii_uppercase();
    match normalized.as_str() {
        "1" => (0, 1, normalized),
        "2" => (0, 2, normalized),
        "3" => (0, 3, normalized),
        "4" => (0, 4, normalized),
        "5" => (0, 5, normalized),
        "6" => (0, 6, normalized),
        "7" => (0, 7, normalized),
        "8" => (0, 8, normalized),
        "9" => (0, 9, normalized),
        "0" => (0, 10, normalized),
        letter if letter.len() == 1 && letter.as_bytes()[0].is_ascii_uppercase() => {
            (1, letter.as_bytes()[0] - b'A', normalized)
        }
        _ => (2, 0, normalized),
    }
}

#[cfg(test)]
fn stepped_product<'a>(
    products: &'a [DisplayProduct],
    current: &DisplayProduct,
    delta: isize,
) -> Option<&'a DisplayProduct> {
    stepped_slice_value(products, current, delta)
}

pub(crate) fn stepped_cut(cuts: &[usize], current: usize, delta: isize) -> Option<usize> {
    stepped_slice_value(cuts, &current, delta).copied()
}

fn stepped_slice_value<'a, T: PartialEq>(
    values: &'a [T],
    current: &T,
    delta: isize,
) -> Option<&'a T> {
    if values.is_empty() {
        return None;
    }
    let current_index = values
        .iter()
        .position(|value| value == current)
        .unwrap_or(0);
    let next_index = (current_index as isize + delta).rem_euclid(values.len() as isize) as usize;
    values.get(next_index)
}

pub(crate) fn is_displayable_on_cut(
    volume: &RadarVolume,
    cut_index: usize,
    product: &DisplayProduct,
) -> bool {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return false;
    };
    let base_moment = product.base_moment();
    let Some(grid) = cut.moments.get(&base_moment) else {
        return false;
    };
    grid.radial_count() >= displayable_radial_threshold(cut.radials.len())
}

pub(crate) fn can_materialize_product_on_cut(
    volume: &RadarVolume,
    cut_index: usize,
    product: &DisplayProduct,
) -> bool {
    if is_displayable_on_cut(volume, cut_index, product) {
        return true;
    }
    let Some(derived) = advanced_derived_product_for_display_product(product) else {
        return false;
    };
    volume
        .cuts
        .get(cut_index)
        .is_some_and(|cut| cut_has_advanced_product_sources(cut, derived))
}

pub(crate) fn advanced_product_source_cut(
    volume: &RadarVolume,
    current_cut: usize,
    product: &DisplayProduct,
) -> Option<usize> {
    advanced_product_source_cut_with_live_filter(volume, current_cut, product, false)
}

pub(crate) fn advanced_product_source_cut_with_live_filter(
    volume: &RadarVolume,
    current_cut: usize,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> Option<usize> {
    advanced_derived_product_for_display_product(product)?;
    if can_materialize_product_on_live_candidate_cut(
        volume,
        current_cut,
        product,
        require_complete_live_cut,
    ) {
        return Some(current_cut);
    }
    let current_elevation = volume.cuts.get(current_cut).map(|cut| cut.elevation_deg);
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            can_materialize_product_on_live_candidate_cut(
                volume,
                *index,
                product,
                require_complete_live_cut,
            )
        })
        .min_by(|(left_index, left_cut), (right_index, right_cut)| {
            let left_delta = current_elevation
                .map(|elevation| (left_cut.elevation_deg - elevation).abs())
                .unwrap_or(*left_index as f32);
            let right_delta = current_elevation
                .map(|elevation| (right_cut.elevation_deg - elevation).abs())
                .unwrap_or(*right_index as f32);
            left_delta
                .total_cmp(&right_delta)
                .then_with(|| {
                    left_index
                        .abs_diff(current_cut)
                        .cmp(&right_index.abs_diff(current_cut))
                })
                .then_with(|| left_index.cmp(right_index))
        })
        .map(|(index, _)| index)
}

pub(crate) fn displayable_radial_threshold(cut_radials: usize) -> usize {
    MIN_DISPLAYABLE_RADIALS.min((cut_radials / 2).max(1))
}

pub(crate) fn should_keep_texture_for_volume_install(
    previous_volume: Option<&RadarVolume>,
    next_volume: &RadarVolume,
    same_volume: bool,
    retain_same_site_texture: bool,
) -> bool {
    same_volume
        || (retain_same_site_texture
            && previous_volume.is_some_and(|previous| previous.site.id == next_volume.site.id))
}

pub(crate) fn selected_cut_render_data_unchanged(
    previous_volume: Option<&RadarVolume>,
    next_volume: &RadarVolume,
    selected_cut: usize,
    selected_product: &DisplayProduct,
) -> bool {
    let Some(previous_volume) = previous_volume else {
        return false;
    };
    if frame_identity_for_volume(previous_volume) != frame_identity_for_volume(next_volume) {
        return false;
    }
    let Some(previous_cut) = previous_volume.cuts.get(selected_cut) else {
        return false;
    };
    let Some(next_cut) = next_volume.cuts.get(selected_cut) else {
        return false;
    };
    if (previous_cut.elevation_deg - next_cut.elevation_deg).abs() > 0.05 {
        return false;
    }
    let base_moment = selected_product.base_moment();
    let Some(previous_grid) = previous_cut.moments.get(&base_moment) else {
        return false;
    };
    let Some(next_grid) = next_cut.moments.get(&base_moment) else {
        return false;
    };
    previous_cut.radials.len() == next_cut.radials.len()
        && previous_grid.radial_count() == next_grid.radial_count()
        && previous_grid.gate_range == next_grid.gate_range
}

#[cfg(test)]
pub(crate) fn selection_for_installed_volume(
    previous_volume: Option<&RadarVolume>,
    previous_cut: usize,
    previous_product: &DisplayProduct,
    volume: &RadarVolume,
    allow_low_level_auto_advance: bool,
    allow_incomplete_live_chunk_advance: bool,
    require_complete_live_cut: bool,
) -> (usize, DisplayProduct) {
    selection_for_installed_volume_with_low_sweep_min_seconds(
        previous_volume,
        previous_cut,
        previous_product,
        volume,
        VolumeSelectionPolicy {
            allow_low_level_auto_advance,
            allow_incomplete_live_chunk_advance,
            require_complete_live_cut,
            reanchor_low_follow: false,
            low_level_min_seconds: 60,
        },
    )
}

pub(crate) fn selection_for_installed_volume_with_low_sweep_min_seconds(
    previous_volume: Option<&RadarVolume>,
    previous_cut: usize,
    previous_product: &DisplayProduct,
    volume: &RadarVolume,
    policy: VolumeSelectionPolicy,
) -> (usize, DisplayProduct) {
    let same_site = previous_volume.is_some_and(|previous| previous.site.id == volume.site.id);
    if same_site
        && policy.allow_low_level_auto_advance
        && let Some(next_cut) = latest_newer_low_level_cut(
            previous_volume,
            previous_cut,
            previous_product,
            volume,
            policy.allow_incomplete_live_chunk_advance,
            policy.low_level_min_seconds,
        )
    {
        return (next_cut, previous_product.clone());
    }
    let reanchor_on_new_frame = previous_volume.is_none_or(|previous| {
        frame_identity_for_volume(previous) != frame_identity_for_volume(volume)
    });
    if reanchor_on_new_frame
        && policy.allow_low_level_auto_advance
        && policy.reanchor_low_follow
        && let Some(next_cut) = newest_timed_low_level_cut(
            previous_product,
            volume,
            policy.allow_incomplete_live_chunk_advance,
        )
    {
        return (next_cut, previous_product.clone());
    }
    // Product choice is user intent, not site-local state. Keep it across a
    // radar switch (and across the brief volume=None state used while a new
    // site loads) whenever the destination can render it. Only the automatic
    // low-sweep advance above is inherently a same-site operation.
    if can_materialize_product_on_live_candidate_cut(
        volume,
        previous_cut,
        previous_product,
        policy.require_complete_live_cut,
    ) {
        return (previous_cut, previous_product.clone());
    }
    if let Some(cut) = best_cut_for_product_with_live_filter(
        volume,
        previous_cut,
        previous_product,
        policy.require_complete_live_cut,
    ) {
        return (cut, previous_product.clone());
    }

    default_selection_for_volume_with_live_filter(volume, policy.require_complete_live_cut)
}

fn latest_newer_low_level_cut(
    previous_volume: Option<&RadarVolume>,
    previous_cut: usize,
    previous_product: &DisplayProduct,
    volume: &RadarVolume,
    allow_incomplete_live_chunk_advance: bool,
    low_level_min_seconds: i64,
) -> Option<usize> {
    let previous_volume = previous_volume?;
    if frame_identity_for_volume(previous_volume) != frame_identity_for_volume(volume) {
        return None;
    }
    let previous_cut_data = previous_volume.cuts.get(previous_cut)?;
    if !is_allowed_live_low_level_tilt_for_site(
        previous_cut_data,
        &previous_volume.site.id,
        allow_incomplete_live_chunk_advance,
    ) {
        return None;
    }
    let previous_time = cut_start_time_utc(previous_volume, previous_cut)?;

    (0..volume.cuts.len())
        .filter(|cut_index| {
            volume.cuts.get(*cut_index).is_some_and(|cut| {
                is_allowed_live_low_level_tilt_for_site(
                    cut,
                    &volume.site.id,
                    allow_incomplete_live_chunk_advance,
                ) && cut.elevation_deg <= LIVE_LOW_LEVEL_AUTO_ADVANCE_MAX_ELEVATION_DEG
            }) && can_materialize_product_on_cut(volume, *cut_index, previous_product)
        })
        .filter_map(|cut_index| {
            let cut_time = cut_start_time_utc(volume, cut_index)?;
            ((cut_time - previous_time).num_seconds() >= low_level_min_seconds)
                .then_some((cut_index, cut_time))
        })
        .max_by_key(|(_, cut_time)| *cut_time)
        .map(|(cut_index, _)| cut_index)
}

fn newest_timed_low_level_cut(
    product: &DisplayProduct,
    volume: &RadarVolume,
    allow_incomplete_live_chunk_advance: bool,
) -> Option<usize> {
    (0..volume.cuts.len())
        .filter(|cut_index| {
            volume.cuts.get(*cut_index).is_some_and(|cut| {
                is_allowed_live_low_level_tilt_for_site(
                    cut,
                    &volume.site.id,
                    allow_incomplete_live_chunk_advance,
                ) && cut.elevation_deg <= LIVE_LOW_LEVEL_AUTO_ADVANCE_MAX_ELEVATION_DEG
            }) && can_materialize_product_on_cut(volume, *cut_index, product)
        })
        .filter_map(|cut_index| {
            cut_start_time_utc(volume, cut_index).map(|cut_time| (cut_index, cut_time))
        })
        .max_by(|(left_index, left_time), (right_index, right_time)| {
            left_time
                .cmp(right_time)
                .then_with(|| left_index.cmp(right_index))
        })
        .map(|(cut_index, _)| cut_index)
}

pub(crate) fn should_defer_live_partial_selection_for_active_product(
    active_volume: Option<&RadarVolume>,
    selected_cut: usize,
    selected_product: &DisplayProduct,
    candidate: Option<&FrameHistoryEntry>,
    require_selected_cut: bool,
) -> bool {
    let Some(active_volume) = active_volume else {
        return false;
    };
    let Some(candidate) = candidate else {
        return false;
    };
    if candidate.status != FrameStatus::LivePartial
        || active_volume.site.id != candidate.identity.site_id
    {
        return false;
    }

    if require_selected_cut {
        if !can_materialize_product_on_cut(active_volume, selected_cut, selected_product) {
            return false;
        }
        !can_materialize_product_on_live_candidate_cut(
            candidate.volume.as_ref(),
            selected_cut,
            selected_product,
            true,
        )
    } else {
        if !volume_can_materialize_product_with_live_filter(active_volume, selected_product, false)
        {
            return false;
        }
        !volume_can_materialize_product_with_live_filter(
            candidate.volume.as_ref(),
            selected_product,
            true,
        )
    }
}

pub(crate) fn volume_has_displayable_product(
    volume: &RadarVolume,
    product: &DisplayProduct,
) -> bool {
    volume_has_displayable_product_with_live_filter(volume, product, false)
}

fn volume_has_displayable_product_with_live_filter(
    volume: &RadarVolume,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> bool {
    (0..volume.cuts.len()).any(|cut_index| {
        is_displayable_on_live_candidate_cut(volume, cut_index, product, require_complete_live_cut)
    })
}

pub(crate) fn volume_can_materialize_product_with_live_filter(
    volume: &RadarVolume,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> bool {
    (0..volume.cuts.len()).any(|cut_index| {
        can_materialize_product_on_live_candidate_cut(
            volume,
            cut_index,
            product,
            require_complete_live_cut,
        )
    })
}

fn default_selection_for_volume_with_live_filter(
    volume: &RadarVolume,
    require_complete_live_cut: bool,
) -> (usize, DisplayProduct) {
    let reflectivity = DisplayProduct::Moment(MomentType::Reflectivity);
    if is_displayable_on_live_candidate_cut(volume, 0, &reflectivity, require_complete_live_cut) {
        return (0, reflectivity);
    }

    for cut_index in 0..volume.cuts.len() {
        let Some(cut) = volume.cuts.get(cut_index) else {
            continue;
        };
        if require_complete_live_cut
            && !is_complete_live_candidate_tilt_for_site(cut, &volume.site.id)
        {
            continue;
        }
        if let Some(product) = displayable_products(volume, cut_index).first().cloned() {
            return (cut_index, product);
        }
    }

    (0, reflectivity)
}

pub(crate) fn best_cut_for_product_with_live_filter(
    volume: &RadarVolume,
    current_cut: usize,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> Option<usize> {
    if can_materialize_product_on_live_candidate_cut(
        volume,
        current_cut,
        product,
        require_complete_live_cut,
    ) {
        return Some(current_cut);
    }
    let current_elevation = volume.cuts.get(current_cut).map(|cut| cut.elevation_deg);
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            can_materialize_product_on_live_candidate_cut(
                volume,
                *index,
                product,
                require_complete_live_cut,
            )
        })
        .min_by(|(left_index, left_cut), (right_index, right_cut)| {
            let left_delta = current_elevation
                .map(|elevation| (left_cut.elevation_deg - elevation).abs())
                .unwrap_or(*left_index as f32);
            let right_delta = current_elevation
                .map(|elevation| (right_cut.elevation_deg - elevation).abs())
                .unwrap_or(*right_index as f32);
            left_delta
                .total_cmp(&right_delta)
                .then_with(|| {
                    left_index
                        .abs_diff(current_cut)
                        .cmp(&right_index.abs_diff(current_cut))
                })
                .then_with(|| left_index.cmp(right_index))
        })
        .map(|(index, _)| index)
}

fn is_displayable_on_live_candidate_cut(
    volume: &RadarVolume,
    cut_index: usize,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> bool {
    if !is_displayable_on_cut(volume, cut_index, product) {
        return false;
    }
    !require_complete_live_cut
        || volume
            .cuts
            .get(cut_index)
            .is_some_and(|cut| is_complete_live_candidate_tilt_for_site(cut, &volume.site.id))
}

pub(crate) fn can_materialize_product_on_live_candidate_cut(
    volume: &RadarVolume,
    cut_index: usize,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> bool {
    can_materialize_product_on_cut(volume, cut_index, product)
        && (!require_complete_live_cut
            || volume
                .cuts
                .get(cut_index)
                .is_some_and(|cut| is_complete_live_candidate_tilt_for_site(cut, &volume.site.id)))
}

pub(crate) fn is_complete_live_candidate_tilt_for_site(cut: &ElevationCut, site_id: &str) -> bool {
    if is_live_low_level_tilt(cut) {
        is_complete_live_low_level_tilt_for_site(cut, site_id)
    } else {
        is_complete_live_tilt(cut)
    }
}

fn should_clear_display_for_latest_load(
    volume: Option<&RadarVolume>,
    site_id: &str,
    now_utc: DateTime<Utc>,
) -> bool {
    let Some(volume) = volume else {
        return false;
    };
    if volume.site.id != site_id {
        return true;
    }

    now_utc
        .signed_duration_since(volume.volume_time.with_timezone(&Utc))
        .num_seconds()
        > STALE_LATEST_DISPLAY_CLEAR_SECONDS
}

pub(crate) fn should_clear_display_before_latest_load(
    mode: LatestLoadMode,
    volume: Option<&RadarVolume>,
    site_id: &str,
    now_utc: DateTime<Utc>,
) -> bool {
    mode != LatestLoadMode::AutoRefresh
        && should_clear_display_for_latest_load(volume, site_id, now_utc)
}

/// Explicit primary loads (User/Loop) take the view from an active
/// custom-URL poll; background AutoRefresh is not intent and must never
/// stop the poller.
pub(crate) fn latest_load_pauses_poll(mode: LatestLoadMode, poll_active: bool) -> bool {
    poll_active && mode != LatestLoadMode::AutoRefresh
}

pub(crate) fn live_preload_frames_for_mode(
    mode: LatestLoadMode,
    requested: usize,
    history_limit: usize,
) -> usize {
    if mode != LatestLoadMode::User {
        return 0;
    }
    requested
        .min(MAX_LIVE_PRELOAD_FRAME_COUNT)
        .min(history_limit.saturating_sub(1))
}

pub(crate) fn archive_fetch_count_for_latest_load(
    mode: LatestLoadMode,
    live_preload_frames: usize,
    history_limit: usize,
    has_displayed_frame: bool,
) -> Option<usize> {
    let history_limit = history_limit.max(1);
    match mode {
        LatestLoadMode::Loop => Some(history_limit),
        LatestLoadMode::AutoRefresh if has_displayed_frame => None,
        LatestLoadMode::AutoRefresh => Some(1),
        LatestLoadMode::User if live_preload_frames > 0 => Some(live_preload_frames + 1),
        LatestLoadMode::User if !has_displayed_frame => Some(1),
        LatestLoadMode::User => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odim_relative_sweep_offsets_remain_exact_near_midnight() {
        let mut volume =
            crate::tests::test_reflectivity_sails_volume_with_radials(&[(0.5, 237_000)], 360);
        volume.volume_time = Utc.with_ymd_and_hms(2026, 8, 12, 0, 0, 22).unwrap();
        volume.metadata.compression = Some("odim-h5".to_owned());

        assert_eq!(
            cut_start_time_utc(&volume, 0),
            Some(Utc.with_ymd_and_hms(2026, 8, 12, 0, 4, 19).unwrap()),
            "the real BEJAB-style offset must not be mistaken for milliseconds since midnight"
        );

        for radial in &mut volume.cuts[0].radials {
            radial.time_offset_ms = -60_000;
        }
        assert_eq!(
            cut_start_time_utc(&volume, 0),
            Some(Utc.with_ymd_and_hms(2026, 8, 11, 23, 59, 22).unwrap()),
            "negative ODIM offsets must cross midnight without changing dates heuristically"
        );
    }

    fn low_follow_policy(
        allow_incomplete_live_chunk_advance: bool,
        reanchor_low_follow: bool,
    ) -> VolumeSelectionPolicy {
        VolumeSelectionPolicy {
            allow_low_level_auto_advance: true,
            allow_incomplete_live_chunk_advance,
            require_complete_live_cut: false,
            reanchor_low_follow,
            low_level_min_seconds: 60,
        }
    }

    #[test]
    fn low_follow_reanchor_chooses_newest_compatible_timed_cut_not_highest_index() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let mut previous =
            crate::tests::test_reflectivity_sails_volume_with_radials(&[(0.50, 60_000)], 720);
        previous.site.id = "KTLX".to_owned();
        let mut volume = crate::tests::test_reflectivity_sails_volume_with_radials(
            &[
                (0.50, 120_000),
                (0.70, 300_000),
                (0.60, 180_000),
                (1.20, 420_000),
            ],
            720,
        );
        volume.site.id = "KOUN".to_owned();

        assert_eq!(
            selection_for_installed_volume_with_low_sweep_min_seconds(
                Some(&previous),
                0,
                &product,
                &volume,
                low_follow_policy(false, true),
            ),
            (1, product),
            "the newest timed cut at or below 1.10 degrees should win"
        );
    }

    #[test]
    fn low_follow_reanchor_respects_complete_cut_policy() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let volume = crate::tests::test_reflectivity_sails_volume_with_radials(
            &[(0.50, 120_000), (0.70, 300_000)],
            240,
        );

        assert_eq!(
            newest_timed_low_level_cut(&product, &volume, false),
            None,
            "chunk-only cuts must not be followed while incomplete updates are hidden"
        );
        assert_eq!(
            newest_timed_low_level_cut(&product, &volume, true),
            Some(1),
            "the newest chunk may be followed when incomplete updates are visible"
        );
    }

    #[test]
    fn low_follow_reanchor_does_not_bypass_same_frame_minimum_gap() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let previous =
            crate::tests::test_reflectivity_sails_volume_with_radials(&[(0.50, 120_000)], 720);
        let volume = crate::tests::test_reflectivity_sails_volume_with_radials(
            &[(0.50, 120_000), (0.70, 150_000)],
            720,
        );

        assert_eq!(
            selection_for_installed_volume_with_low_sweep_min_seconds(
                Some(&previous),
                0,
                &product,
                &volume,
                low_follow_policy(false, true),
            ),
            (0, product),
            "a stale one-shot flag must not bypass the 60-second same-scan threshold"
        );
    }

    #[test]
    fn low_follow_reanchor_without_timed_candidate_falls_through() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let mut volume = crate::tests::test_reflectivity_sails_volume_with_radials(
            &[(0.50, 120_000), (0.70, 300_000)],
            720,
        );
        for cut in &mut volume.cuts {
            cut.radials.clear();
        }

        assert_eq!(
            selection_for_installed_volume_with_low_sweep_min_seconds(
                None,
                1,
                &product,
                &volume,
                low_follow_policy(true, true),
            ),
            (1, product),
            "missing sweep times should retain the ordinary product/cut selection path"
        );
    }

    #[test]
    fn product_keyboard_step_wraps_display_products() {
        let products = vec![
            DisplayProduct::Moment(MomentType::Reflectivity),
            DisplayProduct::Moment(MomentType::Velocity),
            DisplayProduct::StormRelativeVelocity,
        ];

        assert_eq!(
            stepped_product(
                &products,
                &DisplayProduct::Moment(MomentType::Reflectivity),
                1
            ),
            Some(&DisplayProduct::Moment(MomentType::Velocity))
        );
        assert_eq!(
            stepped_product(&products, &DisplayProduct::StormRelativeVelocity, 1),
            Some(&DisplayProduct::Moment(MomentType::Reflectivity))
        );
        assert_eq!(
            stepped_product(
                &products,
                &DisplayProduct::Moment(MomentType::Reflectivity),
                -1
            ),
            Some(&DisplayProduct::StormRelativeVelocity)
        );
    }

    #[test]
    fn tilt_keyboard_step_wraps_displayable_cuts() {
        let cuts = vec![0, 2, 4];

        assert_eq!(stepped_cut(&cuts, 0, 1), Some(2));
        assert_eq!(stepped_cut(&cuts, 4, 1), Some(0));
        assert_eq!(stepped_cut(&cuts, 0, -1), Some(4));
    }

    #[test]
    fn product_hotkeys_accept_numbers_and_letters() {
        assert_eq!(product_hotkey_egui_key("1"), Some(egui::Key::Num1));
        assert_eq!(product_hotkey_egui_key("0"), Some(egui::Key::Num0));
        assert_eq!(product_hotkey_egui_key("a"), Some(egui::Key::A));
        assert_eq!(product_hotkey_egui_key("Z"), Some(egui::Key::Z));
        assert_eq!(product_hotkey_egui_key("Space"), None);
        assert!(product_hotkey_sort_key("9") < product_hotkey_sort_key("0"));
        assert!(product_hotkey_sort_key("0") < product_hotkey_sort_key("A"));
    }

    #[test]
    fn explicit_loads_pause_url_poll_auto_refresh_does_not() {
        assert!(latest_load_pauses_poll(LatestLoadMode::User, true));
        assert!(latest_load_pauses_poll(LatestLoadMode::Loop, true));
        assert!(!latest_load_pauses_poll(LatestLoadMode::AutoRefresh, true));
        assert!(!latest_load_pauses_poll(LatestLoadMode::User, false));
    }

    #[test]
    fn live_preload_only_applies_to_explicit_latest_loads() {
        assert_eq!(live_preload_frames_for_mode(LatestLoadMode::User, 5, 7), 5);
        assert_eq!(live_preload_frames_for_mode(LatestLoadMode::User, 50, 7), 6);
        assert_eq!(live_preload_frames_for_mode(LatestLoadMode::Loop, 5, 7), 0);
        assert_eq!(
            live_preload_frames_for_mode(LatestLoadMode::AutoRefresh, 5, 7),
            0
        );

        assert_eq!(
            archive_fetch_count_for_latest_load(LatestLoadMode::User, 5, 7, true),
            Some(6)
        );
        assert_eq!(
            archive_fetch_count_for_latest_load(LatestLoadMode::User, 0, 7, true),
            None
        );
        assert_eq!(
            archive_fetch_count_for_latest_load(LatestLoadMode::User, 0, 7, false),
            Some(1)
        );
        assert_eq!(
            archive_fetch_count_for_latest_load(LatestLoadMode::Loop, 0, 7, true),
            Some(7)
        );
        assert_eq!(
            archive_fetch_count_for_latest_load(LatestLoadMode::AutoRefresh, 0, 7, true),
            None
        );
    }

    #[test]
    fn latest_load_clears_different_or_stale_display() {
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 23, 0, 0).unwrap();
        let mut fresh = RadarVolume::new(
            radar_core::RadarSite::new("KTLX"),
            now - chrono::Duration::minutes(5),
        );

        assert!(!should_clear_display_for_latest_load(
            Some(&fresh),
            "KTLX",
            now
        ));
        assert!(should_clear_display_for_latest_load(
            Some(&fresh),
            "KGGW",
            now
        ));

        fresh.volume_time = now - chrono::Duration::minutes(16);
        assert!(should_clear_display_for_latest_load(
            Some(&fresh),
            "KTLX",
            now
        ));
        assert!(!should_clear_display_before_latest_load(
            LatestLoadMode::AutoRefresh,
            Some(&fresh),
            "KTLX",
            now
        ));
        assert!(should_clear_display_before_latest_load(
            LatestLoadMode::User,
            Some(&fresh),
            "KTLX",
            now
        ));
        assert!(!should_clear_display_for_latest_load(None, "KTLX", now));
    }

    #[test]
    fn picker_product_order_follows_human_moment_order() {
        let available = [
            MomentType::Reflectivity,
            MomentType::Velocity,
            MomentType::SpectrumWidth,
            MomentType::DifferentialReflectivity,
            MomentType::CorrelationCoefficient,
            MomentType::DifferentialPhase,
            MomentType::SpecificDifferentialPhase,
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();

        let labels = product_order(&available)
            .into_iter()
            .map(|product| product.label().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            labels,
            vec![
                "REF", "VEL", "DVEL", "SRV", "DSRV", "RHO", "ZDR", "SW", "PHI", "KDP"
            ]
        );
    }

    #[test]
    fn typed_favorites_distinguish_derived_and_moment_products_with_the_same_label() {
        let moment = DisplayProduct::Moment(MomentType::Unknown("CREF".to_owned()));
        let derived = DisplayProduct::Derived(DerivedProduct::CompositeReflectivity);
        // Put the moment first to prove typed resolution does not depend on
        // list order and legacy resolution uses picker rank, not `.find()`.
        let products = vec![moment.clone(), derived.clone()];

        assert_eq!(radar_product_favorite_key(&derived), "derived:CREF");
        assert_eq!(radar_product_favorite_key(&moment), "moment:CREF");
        assert_eq!(
            resolve_radar_product_favorite("derived:CREF", &products),
            Some(&derived)
        );
        assert_eq!(
            resolve_radar_product_favorite("moment:CREF", &products),
            Some(&moment)
        );
        assert_eq!(
            radar_product_favorite_caption(&derived, &products),
            "CREF (Composite reflectivity)"
        );
        assert_eq!(
            radar_product_favorite_caption(&moment, &products),
            "CREF (moment)"
        );
    }

    #[test]
    fn legacy_bare_cref_keeps_historical_composite_resolution() {
        let moment = DisplayProduct::Moment(MomentType::Unknown("CREF".to_owned()));
        let derived = DisplayProduct::Derived(DerivedProduct::CompositeReflectivity);
        let products = vec![moment, derived.clone()];

        assert_eq!(
            resolve_radar_product_favorite("cref", &products),
            Some(&derived)
        );
        assert!(is_product_visible_in_picker(
            &derived,
            false,
            &["CREF".to_owned()]
        ));
    }

    #[test]
    fn typed_moment_keys_round_trip_known_unknown_and_reserved_names() {
        let products = vec![
            DisplayProduct::Moment(MomentType::Reflectivity),
            DisplayProduct::Moment(MomentType::Unknown("CREF".to_owned())),
            DisplayProduct::Moment(MomentType::Unknown("REF".to_owned())),
            DisplayProduct::Moment(MomentType::Unknown("unknown=524546".to_owned())),
        ];

        let keys = products
            .iter()
            .map(radar_product_favorite_key)
            .collect::<Vec<_>>();
        assert_eq!(keys[0], "moment:REF");
        assert_eq!(keys[1], "moment:CREF");
        assert_eq!(keys[2], "moment:unknown=524546");
        assert_eq!(keys[3], "moment:unknown=756E6B6E6F776E3D353234353436");
        for (product, key) in products.iter().zip(&keys) {
            assert_eq!(
                resolve_radar_product_favorite(key, &products),
                Some(product)
            );
        }
    }

    #[test]
    fn display_product_favorite_keys_are_variant_qualified() {
        assert_eq!(
            radar_product_favorite_key(&DisplayProduct::DealiasedVelocity),
            "display:DVEL"
        );
        assert_eq!(
            radar_product_favorite_key(&DisplayProduct::StormRelativeVelocity),
            "display:SRV"
        );
        assert_eq!(
            radar_product_favorite_key(&DisplayProduct::StormRelativeDealiasedVelocity),
            "display:DSRV"
        );
    }

    #[test]
    fn mini_product_entries_keep_saved_order_and_unavailable_slots() {
        let reflectivity = DisplayProduct::Moment(MomentType::Reflectivity);
        let composite = DisplayProduct::Derived(DerivedProduct::CompositeReflectivity);
        let products = vec![reflectivity.clone(), composite.clone()];
        let favorites = vec![
            "derived:CREF".to_owned(),
            "moment:REF".to_owned(),
            "display:SRV".to_owned(),
            "moment:unknown=524546".to_owned(),
        ];

        let entries = radar_quick_product_entries(&favorites, &products);
        assert_eq!(
            entries,
            vec![
                RadarQuickProductEntry {
                    caption: "CREF".to_owned(),
                    product: Some(composite),
                },
                RadarQuickProductEntry {
                    caption: "REF (moment)".to_owned(),
                    product: Some(reflectivity),
                },
                RadarQuickProductEntry {
                    caption: "SRV".to_owned(),
                    product: None,
                },
                RadarQuickProductEntry {
                    caption: "REF (custom moment)".to_owned(),
                    product: None,
                },
            ]
        );
    }

    #[test]
    fn mini_product_entries_qualify_colliding_available_and_unavailable_products() {
        let composite = DisplayProduct::Derived(DerivedProduct::CompositeReflectivity);
        let products = vec![composite.clone()];
        let favorites = vec!["derived:CREF".to_owned(), "moment:CREF".to_owned()];

        assert_eq!(
            radar_quick_product_entries(&favorites, &products),
            vec![
                RadarQuickProductEntry {
                    caption: "CREF (derived)".to_owned(),
                    product: Some(composite),
                },
                RadarQuickProductEntry {
                    caption: "CREF (moment)".to_owned(),
                    product: None,
                },
            ]
        );
    }

    #[test]
    fn validation_products_have_scientific_labels_units_and_first_class_order() {
        let product = |id: &str| DisplayProduct::Moment(MomentType::Unknown(id.to_owned()));
        assert_eq!(
            validation_product_label(&product("IREF")),
            Some("Ideal reflectivity (IREF)")
        );
        assert_eq!(
            validation_product_label(&product("MVEL")),
            Some("Measured velocity (MVEL)")
        );
        assert_eq!(
            validation_product_label(&product("MCOV")),
            Some("Model coverage (MCOV)")
        );
        assert_eq!(
            validation_product_label(&product("DIF_REF")),
            Some("Reflectivity difference (sim - obs)")
        );
        assert_eq!(validation_product_units(&product("MCOV")), Some("fraction"));
        assert_eq!(validation_product_units(&product("IRHO")), Some("fraction"));
        assert_eq!(validation_product_units(&product("MREF")), Some("dBZ"));
        assert_eq!(validation_product_units(&product("DIF_VEL")), Some("m/s"));
        assert_eq!(validation_product_label(&product("OTHER")), None);

        let mut products = [
            product("DIF_KDP"),
            DisplayProduct::Derived(DerivedProduct::CompositeReflectivity),
            product("MVEL"),
            product("IREF"),
            product("MCOV"),
            DisplayProduct::Moment(MomentType::SpecificDifferentialPhase),
            product("DIF_REF"),
            product("MSIG"),
        ];
        products.sort_by(|left, right| picker_product_rank(left).cmp(&picker_product_rank(right)));
        let labels = products
            .iter()
            .map(|product| validation_product_label(product).unwrap_or_else(|| product.label()))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "KDP",
                "Ideal reflectivity (IREF)",
                "Measured velocity (MVEL)",
                "Model coverage (MCOV)",
                "Meteorological signal (MSIG)",
                "Reflectivity difference (sim - obs)",
                "KDP difference (sim - obs)",
                "CREF",
            ]
        );
    }
}
