use chrono::{TimeZone, Utc};
use nexrad_io::odim_cartesian::{
    OdimCartesianProjection, OdimCartesianQuantity, PROJ_SPHERE_RADIUS_M,
    decode_odim_h5_cartesian_max,
};

const KDP: &[u8] = include_bytes!("data/imgw_polrad/2026071100150601KDP.max.h5");
const PHIDP: &[u8] = include_bytes!("data/imgw_polrad/2026071100150601PhiDP.max.h5");
const RHOHV: &[u8] = include_bytes!("data/imgw_polrad/2026071100150601RhoHV.max.h5");
const ZDR: &[u8] = include_bytes!("data/imgw_polrad/2026071100150601ZDR.max.h5");

fn finite_range(values: &[f32]) -> (usize, f32, f32) {
    values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(
            (0, f32::INFINITY, f32::NEG_INFINITY),
            |(count, low, high), value| (count + 1, low.min(value), high.max(value)),
        )
}

#[test]
fn imgw_max_decodes_dataset_level_metadata_geometry_and_missing_values() {
    let grid = decode_odim_h5_cartesian_max(KDP).expect("IMGW KDP MAX decodes");

    assert_eq!(grid.odim_version.as_deref(), Some("H5rd 2.3"));
    assert_eq!(grid.dataset, "dataset1");
    assert_eq!(grid.product, "MAX");
    assert_eq!(grid.quantity_code, "KDP");
    assert_eq!(
        grid.quantity,
        OdimCartesianQuantity::SpecificDifferentialPhase
    );
    assert_eq!(grid.units.as_deref(), Some("deg/km"));
    assert_eq!(
        grid.start_time,
        Utc.with_ymd_and_hms(2026, 7, 11, 0, 15, 6).unwrap()
    );
    assert_eq!(grid.end_time, Some(grid.start_time));

    assert_eq!(grid.site.id, "RAM");
    assert_eq!(grid.site.source, "WMO:12514");
    assert!((grid.site.latitude_deg - 50.151_328).abs() < 1.0e-8);
    assert!((grid.site.longitude_deg - 18.725_094).abs() < 1.0e-8);
    assert!((grid.site.height_m.unwrap() - 357.1).abs() < 1.0e-8);

    let OdimCartesianProjection::AzimuthalEquidistantSphere {
        center_latitude_deg,
        center_longitude_deg,
        radius_m,
        projdef,
    } = &grid.projection;
    assert!((*center_latitude_deg - 50.1513).abs() < 1.0e-8);
    assert!((*center_longitude_deg - 18.7251).abs() < 1.0e-8);
    assert_eq!(*radius_m, PROJ_SPHERE_RADIUS_M);
    assert!(projdef.contains("+proj=aeqd"));
    assert!(projdef.contains("+ellps=sphere"));

    assert_eq!((grid.geometry.width, grid.geometry.height), (500, 500));
    assert!((grid.geometry.x_spacing_m - 1_001.953_064_117_074_4).abs() < 1.0e-9);
    assert!((grid.geometry.y_spacing_m - 998.636_495_766_002).abs() < 1.0e-9);
    assert_eq!(grid.geometry.min_height_m, Some(500.0));
    assert_eq!(grid.geometry.max_height_m, Some(18_000.0));
    assert!(
        grid.geometry.corners.upper_left.latitude_deg
            > grid.geometry.corners.lower_left.latitude_deg
    );
    assert_eq!(grid.values.len(), 250_000);
    assert_eq!(grid.encoding.nodata, Some(255.0));
    assert_eq!(grid.encoding.undetect, Some(0.0));

    let (finite, low, high) = finite_range(&grid.values);
    assert_eq!(finite, 1_934);
    assert!((low - -0.719_441_1).abs() < 1.0e-6, "low={low}");
    assert!((high - 0.936_317).abs() < 1.0e-6, "high={high}");
    assert_eq!(
        grid.values.iter().filter(|value| value.is_nan()).count(),
        248_066
    );
    assert!((grid.value_at(231, 93).unwrap() - -0.103_838_73).abs() < 1.0e-6);
    assert!(grid.geometry.cell_center_offset_m(0, 0).unwrap().0 < 0.0);
    assert!(grid.geometry.cell_center_offset_m(0, 0).unwrap().1 > 0.0);
}

#[test]
fn all_released_imgw_dual_pol_max_quantities_decode_with_physical_units() {
    let cases = [
        (
            KDP,
            OdimCartesianQuantity::SpecificDifferentialPhase,
            "deg/km",
            1_934,
            -0.719_441_1,
            0.936_317,
        ),
        (
            PHIDP,
            OdimCartesianQuantity::DifferentialPhase,
            "deg",
            1_934,
            0.0,
            360.0,
        ),
        (
            RHOHV,
            OdimCartesianQuantity::CorrelationCoefficient,
            "1",
            21_131,
            0.003_952_569,
            1.0,
        ),
        (
            ZDR,
            OdimCartesianQuantity::DifferentialReflectivity,
            "dB",
            19_623,
            -8.0,
            12.0,
        ),
    ];

    for (bytes, quantity, units, expected_finite, expected_low, expected_high) in cases {
        let grid = decode_odim_h5_cartesian_max(bytes).expect("IMGW MAX decodes");
        assert_eq!(grid.quantity, quantity);
        assert_eq!(grid.units.as_deref(), Some(units));
        let (finite, low, high) = finite_range(&grid.values);
        assert_eq!(
            finite, expected_finite,
            "{} finite count",
            grid.quantity_code
        );
        assert!(
            (low - expected_low).abs() < 1.0e-5,
            "{} low={low}",
            grid.quantity_code
        );
        assert!(
            (high - expected_high).abs() < 1.0e-5,
            "{} high={high}",
            grid.quantity_code
        );
    }
}

#[test]
fn imgw_dataset_level_what_is_required_not_data_plane_what() {
    let file = nexrad_io::hdf5lite::H5File::open(KDP).expect("fixture HDF5 opens");
    assert!(file.has_object("/dataset1/what"));
    assert!(!file.has_object("/dataset1/data1/what"));
    let grid = decode_odim_h5_cartesian_max(KDP).expect("dataset-level metadata decodes");
    assert_eq!(grid.quantity_code, "KDP");
}

#[test]
fn image_decoder_and_volume_router_remain_separate() {
    let volume_error = nexrad_io::decode_supported_volume_bytes(KDP)
        .expect_err("IMAGE must not route into RadarVolume");
    assert!(
        volume_error.contains("PVOL and SCAN only"),
        "{volume_error}"
    );

    let polar = include_bytes!("data/odim_pvol_synth.h5");
    let image_error =
        decode_odim_h5_cartesian_max(polar).expect_err("PVOL must not route into Cartesian grid");
    assert!(image_error.to_string().contains("is not a Cartesian IMAGE"));
}
