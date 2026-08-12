// SPDX-License-Identifier: Apache-2.0

//! Map foundation for ArWen Studio: camera + projection ([`view`]),
//! Lambert forecast domains ([`domain`]), vendored basemap data
//! ([`basemap_data`]/[`basemap_towns`]), place/coordinate search
//! ([`search`]), and pure geometry ([`geo`]). Raster tiles and the AEQD
//! forward/inverse come from BowEcho's `ui_core` (pinned git dep).

pub mod basemap_data;
pub mod basemap_towns;
pub mod domain;
pub mod geo;
pub mod search;
pub mod view;

pub use domain::{
    LambertDomain, NestPlacement, nest_outline_lon_lat, nest_ring_lon_lat, path_grid_to_root,
    root_grid_to_path,
};
pub use search::{
    CoordinateMarker, PlaceSearchResult, parse_coordinate_search_query, place_search_matches,
};
pub use ui_core::geo::{aeqd_forward_km, aeqd_inverse_km};
pub use ui_core::tiles;
pub use view::{GeoBounds, MapView};
