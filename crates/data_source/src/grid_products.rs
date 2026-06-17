//! Catalog surface for gridded/composite radar and warning products.
//!
//! This is deliberately separate from `international::IntlProvider`.
//! International providers describe site-centered polar volumes that decode
//! into `RadarVolume`; these entries describe time-indexed grids, rasters,
//! nowcasts, QPE products, and warning polygons. The catalog gives the UI and
//! future decoders a typed target without pretending that every European
//! product is a radar site.

/// The product family a gridded source contributes to BowEcho.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridProductKind {
    MaxReflectivity,
    ReflectivityComposite,
    RainRate,
    Accumulation,
    Qpe,
    Nowcast,
    EchoTops,
    HailProbability,
    CellTracking,
    ThreeDimensionalComposite,
    VerticalMaximumIntensity,
    HeavyRainDetection,
    Lightning,
    RadarStatus,
    Warning,
    Discovery,
}

/// Container/transfer format the product is expected to use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridCodec {
    OdimH5Grid,
    CloudOptimizedGeoTiff,
    Hdf5Grid,
    NetcdfGrid,
    Zarr,
    GeoReferencedImage,
    GeoJson,
    EdrJson,
    ApiJson,
    MqttNotification,
}

/// How BowEcho should discover or fetch the product once a decoder exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridAccess {
    AnonymousBucket,
    OpenHttp,
    RestApi,
    EdrApi,
    Mqtt,
    PortalDownload,
}

/// Current implementation state. `Catalogued` is intentionally not
/// user-visible ingest: it means the product is known and typed, and still
/// needs the matching decoder/fetcher before it can be displayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridImplementationStatus {
    Catalogued,
    DecoderNeeded,
}

/// One gridded/composite product available from a provider.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridProduct {
    pub slug: &'static str,
    pub label: &'static str,
    pub kind: GridProductKind,
    pub cadence_minutes: Option<u16>,
    pub resolution_km: Option<f32>,
    pub forecast_hours: Option<u16>,
    pub codecs: &'static [GridCodec],
    pub access: &'static [GridAccess],
    pub status: GridImplementationStatus,
    pub source_hint: &'static str,
}

/// Static implementation of the future `GridProductProvider` path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StaticGridProductProvider {
    pub id: &'static str,
    pub label: &'static str,
    pub region: &'static str,
    pub docs_url: &'static str,
    pub products: &'static [GridProduct],
}

/// A source of time-indexed gridded radar products or companion alert layers.
pub trait GridProductProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn region(&self) -> &'static str;
    fn products(&self) -> &'static [GridProduct];
}

impl GridProductProvider for StaticGridProductProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn region(&self) -> &'static str {
        self.region
    }

    fn products(&self) -> &'static [GridProduct] {
        self.products
    }
}

const OPERA_CODECS: &[GridCodec] = &[GridCodec::OdimH5Grid, GridCodec::CloudOptimizedGeoTiff];
const ORD_API_CODECS: &[GridCodec] = &[
    GridCodec::EdrJson,
    GridCodec::ApiJson,
    GridCodec::MqttNotification,
];
const HDF5_GRID: &[GridCodec] = &[GridCodec::Hdf5Grid];
const KNMI_GRID: &[GridCodec] = &[GridCodec::Hdf5Grid, GridCodec::NetcdfGrid];
const SWISS_GRID: &[GridCodec] = &[GridCodec::Hdf5Grid, GridCodec::NetcdfGrid];
const RADOLAN_CODECS: &[GridCodec] = &[GridCodec::ApiJson, GridCodec::Hdf5Grid];
const ITALY_CODECS: &[GridCodec] = &[GridCodec::GeoReferencedImage, GridCodec::Zarr];
const IMAGE_CODECS: &[GridCodec] = &[GridCodec::GeoReferencedImage];
const METEOALARM_CODECS: &[GridCodec] = &[
    GridCodec::GeoJson,
    GridCodec::EdrJson,
    GridCodec::MqttNotification,
];

const BUCKET_AND_API: &[GridAccess] = &[GridAccess::AnonymousBucket, GridAccess::RestApi];
const API_AND_MQTT: &[GridAccess] = &[GridAccess::RestApi, GridAccess::Mqtt, GridAccess::EdrApi];
const OPEN_BUCKET: &[GridAccess] = &[GridAccess::AnonymousBucket];
const REST_API: &[GridAccess] = &[GridAccess::RestApi];
const OPEN_HTTP: &[GridAccess] = &[GridAccess::OpenHttp];
const PORTAL_DOWNLOAD: &[GridAccess] = &[GridAccess::PortalDownload];

const OPERA_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "opera-cirrus-max-reflectivity",
        label: "Europe OPERA/CIRRUS Max Reflectivity",
        kind: GridProductKind::MaxReflectivity,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: OPERA_CODECS,
        access: BUCKET_AND_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "ORD OPERA composite prefix; gridded, not a polar site",
    },
    GridProduct {
        slug: "opera-rain-rate",
        label: "Europe OPERA Rain Rate",
        kind: GridProductKind::RainRate,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: OPERA_CODECS,
        access: BUCKET_AND_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "ORD OPERA composite instantaneous precipitation product",
    },
    GridProduct {
        slug: "opera-accum-1h",
        label: "Europe OPERA 1h Accumulation",
        kind: GridProductKind::Accumulation,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: OPERA_CODECS,
        access: BUCKET_AND_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "ORD OPERA composite one-hour accumulation product",
    },
    GridProduct {
        slug: "ord-api-mqtt-discovery",
        label: "ORD API/MQTT Discovery",
        kind: GridProductKind::Discovery,
        cadence_minutes: None,
        resolution_km: None,
        forecast_hours: None,
        codecs: ORD_API_CODECS,
        access: API_AND_MQTT,
        status: GridImplementationStatus::Catalogued,
        source_hint: "MeteoGate ORD API and notification service for metadata/archive refresh",
    },
];

const UK_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "uk-metoffice-rain-rate",
    label: "UK Met Office Rain Rate",
    kind: GridProductKind::RainRate,
    cadence_minutes: Some(15),
    resolution_km: None,
    forecast_hours: None,
    codecs: HDF5_GRID,
    access: OPEN_BUCKET,
    status: GridImplementationStatus::DecoderNeeded,
    source_hint: "Met Office AWS radar composite HDF5 rolling archive",
}];

const KNMI_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "knmi-reflectivity-composite",
        label: "Netherlands KNMI Reflectivity Composite",
        kind: GridProductKind::ReflectivityComposite,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI Data Platform radar grid product family",
    },
    GridProduct {
        slug: "knmi-reflectivity-nowcast",
        label: "Netherlands KNMI Reflectivity Nowcast",
        kind: GridProductKind::Nowcast,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: Some(2),
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI 0-2 hour radar nowcast family",
    },
    GridProduct {
        slug: "knmi-radar-gauge-accum",
        label: "Netherlands KNMI Radar/Gauge Accumulation",
        kind: GridProductKind::Qpe,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI radar/gauge quantitative precipitation products",
    },
    GridProduct {
        slug: "knmi-hail-probability",
        label: "Netherlands KNMI Hail Probability",
        kind: GridProductKind::HailProbability,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI radar hail product family",
    },
    GridProduct {
        slug: "knmi-echo-tops",
        label: "Netherlands KNMI Echo Tops",
        kind: GridProductKind::EchoTops,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI echo-top height composite",
    },
    GridProduct {
        slug: "knmi-3d-composite",
        label: "Netherlands KNMI 3D Radar Composite",
        kind: GridProductKind::ThreeDimensionalComposite,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI real-time 3D radar composite",
    },
    GridProduct {
        slug: "knmi-cellwarn",
        label: "Netherlands KNMI CellWarn",
        kind: GridProductKind::CellTracking,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI tracking/risk output",
    },
];

const METEOSWISS_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "meteoswiss-precip",
        label: "Swiss PRECIP",
        kind: GridProductKind::Qpe,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss national precipitation radar estimate",
    },
    GridProduct {
        slug: "meteoswiss-combiprecip",
        label: "Swiss CombiPrecip",
        kind: GridProductKind::Qpe,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss radar plus rain-gauge precipitation estimate",
    },
    GridProduct {
        slug: "meteoswiss-hail",
        label: "Swiss Hail Products",
        kind: GridProductKind::HailProbability,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss radar hail product family",
    },
    GridProduct {
        slug: "meteoswiss-convection",
        label: "Swiss Convection Products",
        kind: GridProductKind::Nowcast,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss convection radar product family",
    },
    GridProduct {
        slug: "meteoswiss-polar-3d",
        label: "Swiss Polar 3D Products",
        kind: GridProductKind::ThreeDimensionalComposite,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss polar 3D radar product family",
    },
];

const DWD_GRID_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "dwd-radolan-qpe",
        label: "Germany RADOLAN QPE",
        kind: GridProductKind::Qpe,
        cadence_minutes: Some(60),
        resolution_km: None,
        forecast_hours: None,
        codecs: RADOLAN_CODECS,
        access: OPEN_HTTP,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "DWD gauge-adjusted radar precipitation analysis",
    },
    GridProduct {
        slug: "dwd-radvor-nowcast",
        label: "Germany RADVOR Nowcast",
        kind: GridProductKind::Nowcast,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: RADOLAN_CODECS,
        access: OPEN_HTTP,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "DWD radar precipitation nowcast product family",
    },
];

const ITALY_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "italy-dpc-vmi",
        label: "Italy DPC VMI",
        kind: GridProductKind::VerticalMaximumIntensity,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "Civil Protection national vertical maximum intensity composite",
    },
    GridProduct {
        slug: "italy-dpc-sri",
        label: "Italy DPC SRI",
        kind: GridProductKind::RainRate,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "Civil Protection surface rainfall intensity composite",
    },
    GridProduct {
        slug: "italy-dpc-srt",
        label: "Italy DPC SRT",
        kind: GridProductKind::Accumulation,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "Civil Protection rainfall totals; 1/3/6/12/24h product family",
    },
    GridProduct {
        slug: "italy-dpc-heavy-rain",
        label: "Italy DPC Heavy Rain Detection",
        kind: GridProductKind::HeavyRainDetection,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "Civil Protection heavy rain detection product family",
    },
    GridProduct {
        slug: "italy-dpc-lightning",
        label: "Italy DPC Lightning",
        kind: GridProductKind::Lightning,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "Civil Protection lightning overlay product",
    },
    GridProduct {
        slug: "italy-dpc-radar-status",
        label: "Italy DPC Radar Status",
        kind: GridProductKind::RadarStatus,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "Civil Protection radar status overlay",
    },
];

const AEMET_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "aemet-lowest-elevation-reflectivity",
    label: "Spain AEMET Lowest-Elevation Reflectivity",
    kind: GridProductKind::ReflectivityComposite,
    cadence_minutes: None,
    resolution_km: None,
    forecast_hours: None,
    codecs: IMAGE_CODECS,
    access: REST_API,
    status: GridImplementationStatus::DecoderNeeded,
    source_hint: "AEMET OpenData/public radar image path",
}];

const IPMA_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "ipma-precipitation-intensity",
    label: "Portugal IPMA Precipitation Intensity",
    kind: GridProductKind::RainRate,
    cadence_minutes: Some(60),
    resolution_km: None,
    forecast_hours: None,
    codecs: IMAGE_CODECS,
    access: OPEN_HTTP,
    status: GridImplementationStatus::DecoderNeeded,
    source_hint: "IPMA radar imagery for mainland, Azores, and Madeira",
}];

const METEOALARM_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "meteoalarm-warnings",
    label: "MeteoAlarm Warnings",
    kind: GridProductKind::Warning,
    cadence_minutes: None,
    resolution_km: None,
    forecast_hours: None,
    codecs: METEOALARM_CODECS,
    access: API_AND_MQTT,
    status: GridImplementationStatus::DecoderNeeded,
    source_hint: "Pan-European warning polygons and metadata",
}];

const GRID_PROVIDERS: &[StaticGridProductProvider] = &[
    StaticGridProductProvider {
        id: "opera",
        label: "OPERA/CIRRUS Europe",
        region: "Europe",
        docs_url: "https://eumetnet.github.io/openradardata-documentation/",
        products: OPERA_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "metoffice-uk",
        label: "Met Office UK Radar",
        region: "United Kingdom",
        docs_url: "https://registry.opendata.aws/met-office-uk-radar-observations/",
        products: UK_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "knmi",
        label: "KNMI Netherlands",
        region: "Netherlands",
        docs_url: "https://dataplatform.knmi.nl/",
        products: KNMI_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "meteoswiss",
        label: "MeteoSwiss",
        region: "Switzerland",
        docs_url: "https://opendatadocs.meteoswiss.ch/",
        products: METEOSWISS_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "dwd-grid",
        label: "DWD Gridded Products",
        region: "Germany",
        docs_url: "https://opendata.dwd.de/weather/radar/",
        products: DWD_GRID_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "italy-dpc",
        label: "Italy DPC / ItaliaMeteo",
        region: "Italy",
        docs_url: "https://www.agenziaitaliameteo.it/",
        products: ITALY_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "aemet",
        label: "AEMET Spain",
        region: "Spain",
        docs_url: "https://www.aemet.es/",
        products: AEMET_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "ipma",
        label: "IPMA Portugal",
        region: "Portugal",
        docs_url: "https://www.ipma.pt/",
        products: IPMA_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "meteoalarm",
        label: "MeteoAlarm",
        region: "Europe",
        docs_url: "https://api.meteoalarm.org/",
        products: METEOALARM_PRODUCTS,
    },
];

/// Built-in gridded/composite product providers, separate from polar sites.
pub fn grid_product_providers() -> &'static [StaticGridProductProvider] {
    GRID_PROVIDERS
}

/// Flatten every catalogued grid product in provider order.
pub fn grid_products()
-> impl Iterator<Item = (&'static StaticGridProductProvider, &'static GridProduct)> {
    GRID_PROVIDERS.iter().flat_map(|provider| {
        provider
            .products
            .iter()
            .map(move |product| (provider, product))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn grid_provider_ids_are_unique_and_nonempty() {
        let providers = grid_product_providers();
        let ids: BTreeSet<_> = providers.iter().map(|provider| provider.id).collect();
        assert_eq!(ids.len(), providers.len());
        for provider in providers {
            assert!(!provider.id.is_empty());
            assert!(!provider.label.is_empty());
            assert!(!provider.products.is_empty(), "{}", provider.id);
        }
    }

    #[test]
    fn roadmap_sources_are_catalogued_separately_from_polar_sites() {
        let slugs: BTreeSet<_> = grid_products().map(|(_, product)| product.slug).collect();
        for required in [
            "opera-cirrus-max-reflectivity",
            "opera-rain-rate",
            "opera-accum-1h",
            "ord-api-mqtt-discovery",
            "uk-metoffice-rain-rate",
            "knmi-reflectivity-nowcast",
            "knmi-hail-probability",
            "knmi-echo-tops",
            "meteoswiss-precip",
            "meteoswiss-combiprecip",
            "dwd-radolan-qpe",
            "italy-dpc-vmi",
            "italy-dpc-sri",
            "italy-dpc-srt",
            "aemet-lowest-elevation-reflectivity",
            "ipma-precipitation-intensity",
            "meteoalarm-warnings",
        ] {
            assert!(slugs.contains(required), "{required}");
        }
    }

    #[test]
    fn opera_products_are_grid_products_not_radar_sites() {
        let opera = grid_product_providers()
            .iter()
            .find(|provider| provider.id == "opera")
            .expect("opera provider");
        assert!(
            opera
                .products
                .iter()
                .all(|product| product.codecs.contains(&GridCodec::OdimH5Grid)
                    || product.kind == GridProductKind::Discovery)
        );
        assert!(
            opera
                .products
                .iter()
                .any(|product| product.access.contains(&GridAccess::Mqtt))
        );
    }
}
