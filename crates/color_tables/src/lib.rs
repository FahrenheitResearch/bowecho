//! Fast color table parsing and sampling for radar renderers.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

const KNOT_TO_MPS: f32 = 0.514_444;
const MPH_TO_MPS: f32 = 0.447_04;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }

    fn lerp(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        Self {
            r: lerp_u8(self.r, other.r, amount),
            g: lerp_u8(self.g, other.g, amount),
            b: lerp_u8(self.b, other.b, amount),
            a: lerp_u8(self.a, other.a, amount),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColorTableFamily {
    Reflectivity,
    Velocity,
    SpectrumWidth,
    CorrelationCoefficient,
    DifferentialReflectivity,
    EchoTops,
    Vil,
    VilDensity,
    HailSize,
    AzimuthalShear,
    DifferentialPhase,
    SpecificDifferentialPhase,
    Generic,
}

impl ColorTableFamily {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reflectivity => "Reflectivity",
            Self::Velocity => "Velocity / SRV",
            Self::SpectrumWidth => "Spectrum Width",
            Self::CorrelationCoefficient => "Correlation Coeff (CC)",
            Self::DifferentialReflectivity => "Differential Refl (ZDR)",
            Self::EchoTops => "Echo Tops",
            Self::Vil => "VIL",
            Self::VilDensity => "VIL Density",
            Self::HailSize => "Hail Size (MEHS)",
            Self::AzimuthalShear => "Azimuthal Shear",
            Self::DifferentialPhase => "Differential Phase (PHI)",
            Self::SpecificDifferentialPhase => "Specific Diff Phase (KDP)",
            Self::Generic => "Other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorStop {
    pub value: f32,
    pub color: Rgba8,
    /// GR .pal two-color entries: the color ramps from `color` to this
    /// across the stop's own interval (None = single-color entry).
    pub end_color: Option<Rgba8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorTable {
    name: String,
    product: Option<String>,
    units: Option<String>,
    range_folded: Rgba8,
    sample_mode: SampleMode,
    stops: Vec<ColorStop>,
    /// Render-time display threshold: values below it draw transparent (data
    /// is untouched — readouts still see it). For diverging products the clamp
    /// is symmetric (|value| < threshold), hiding the noise around zero while
    /// keeping strong inbound/outbound returns.
    display_threshold: Option<f32>,
    threshold_is_symmetric: bool,
}

impl ColorTable {
    pub fn new(name: impl Into<String>, stops: Vec<ColorStop>) -> Result<Self, ColorTableError> {
        Self::from_parts(
            name.into(),
            None,
            None,
            default_range_folded_color(),
            SampleMode::Interpolated,
            stops,
        )
    }

    pub fn new_stepped(
        name: impl Into<String>,
        stops: Vec<ColorStop>,
    ) -> Result<Self, ColorTableError> {
        Self::from_parts(
            name.into(),
            None,
            None,
            default_range_folded_color(),
            SampleMode::Stepped,
            stops,
        )
    }

    pub fn parse(name: impl Into<String>, text: &str) -> Result<Self, ColorTableError> {
        Self::parse_with_default_mode(name, text, SampleMode::Interpolated)
    }

    pub fn parse_with_default_mode(
        name: impl Into<String>,
        text: &str,
        default_sample_mode: SampleMode,
    ) -> Result<Self, ColorTableError> {
        let name = name.into();
        let mut product = None;
        let mut units = None;
        let mut scale = None;
        let mut range_folded = default_range_folded_color();
        let mut sample_mode = default_sample_mode;
        let mut stops = Vec::new();

        for (line_index, original_line) in text.lines().enumerate() {
            let line_number = line_index + 1;
            let line = normalize_line(original_line);
            let line = line.trim();
            if line.is_empty()
                || line.starts_with(';')
                || line.starts_with('#')
                || line.starts_with("$$")
            {
                continue;
            }

            let Some((raw_key, raw_value)) = split_key_value(line) else {
                continue;
            };
            let key = normalize_key(raw_key);
            let value = raw_value.trim();

            match key.as_str() {
                "product" => product = non_empty(value),
                "units" => units = non_empty(value),
                "scale" => scale = parse_positive_f32(value),
                "step" => {
                    // In GR .pal files `Step:` is the LEGEND tick spacing
                    // and never quantizes the display; our internal tables
                    // use it as the quantized-interpolation step.
                    if default_sample_mode != SampleMode::GrPal {
                        sample_mode = parse_positive_f32(value)
                            .map(|step| SampleMode::QuantizedInterpolated { step, origin: 0.0 })
                            .unwrap_or(SampleMode::Stepped);
                    }
                }
                "mode" | "samplemode" | "interpolate" | "interpolation" | "smooth" => {
                    if let Some(parsed_mode) = parse_sample_mode(value) {
                        sample_mode = parsed_mode;
                    }
                }
                "rf" | "rangefolded" | "rangefoldedcolor" => {
                    range_folded = parse_color_only(value, line_number)?;
                }
                "color" | "color4" | "solidcolor" | "solidcolor4" => {
                    stops.push(parse_color_stop(
                        value,
                        key.ends_with('4'),
                        key.starts_with("solid"),
                        line_number,
                    )?);
                }
                _ => {}
            }
        }

        let unit_scale = scale
            .map(|scale| 1.0 / scale)
            .or_else(|| units.as_deref().map(unit_value_to_mps_scale))
            .unwrap_or(1.0);
        if unit_scale != 1.0 {
            for stop in &mut stops {
                stop.value *= unit_scale;
            }
            sample_mode = sample_mode.scale_values(unit_scale);
        }

        Self::from_parts(name, product, units, range_folded, sample_mode, stops)
    }

    /// Parse a user GR2Analyst-style .pal with faithful GR semantics:
    /// solid/gradient intervals per entry, color4 alpha, `Step:` as legend
    /// ticks only.
    pub fn parse_gr_pal(name: impl Into<String>, text: &str) -> Result<Self, ColorTableError> {
        Self::parse_with_default_mode(name, text, SampleMode::GrPal)
    }

    pub fn parse_stepped(name: impl Into<String>, text: &str) -> Result<Self, ColorTableError> {
        Self::parse_with_default_mode(name, text, SampleMode::Stepped)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn product(&self) -> Option<&str> {
        self.product.as_deref()
    }

    pub fn units(&self) -> Option<&str> {
        self.units.as_deref()
    }

    pub fn stops(&self) -> &[ColorStop] {
        &self.stops
    }

    pub fn interpolates(&self) -> bool {
        self.sample_mode == SampleMode::Interpolated
    }

    pub fn sample_mode_label(&self) -> &'static str {
        self.sample_mode.label()
    }

    pub fn step_size(&self) -> Option<f32> {
        self.sample_mode.step_size()
    }

    pub fn sample(&self, value: f32) -> Rgba8 {
        if !value.is_finite() {
            return Rgba8::TRANSPARENT;
        }
        if self.value_below_display_threshold(value) {
            return Rgba8::TRANSPARENT;
        }
        match self.sample_mode {
            SampleMode::Interpolated => self.sample_interpolated(value),
            SampleMode::Stepped => self.sample_stepped(value),
            SampleMode::GrPal => self.sample_gr_pal(value),
            SampleMode::QuantizedInterpolated { step, origin } => {
                if let Some(first_opaque_value) = self.first_opaque_value()
                    && value < first_opaque_value
                {
                    return Rgba8::TRANSPARENT;
                }
                let quantized = quantize_value(value, step, origin);
                self.sample_interpolated(quantized)
            }
        }
    }

    fn sample_interpolated(&self, value: f32) -> Rgba8 {
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let index = self.stops.partition_point(|stop| stop.value < value);
        if index >= self.stops.len() {
            return self
                .stops
                .last()
                .map(|stop| stop.color)
                .unwrap_or(Rgba8::TRANSPARENT);
        }
        let right = self.stops[index];
        if value == right.value {
            return right.color;
        }
        let left = self.stops[index - 1];
        let span = (right.value - left.value).max(f32::EPSILON);
        left.color.lerp(right.color, (value - left.value) / span)
    }

    /// GR .pal interval semantics: linear ramps between `color:` rows, optional
    /// per-row `end_color` ramps, and `SolidColor:` hard cuts. `Step:` never
    /// quantizes the display.
    fn sample_gr_pal(&self, value: f32) -> Rgba8 {
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let index = self.stops.partition_point(|stop| stop.value <= value);
        let stop = self.stops[index.saturating_sub(1)];
        let Some(next_stop) = self.stops.get(index) else {
            return stop.color;
        };
        let end_color = stop.end_color.unwrap_or({
            if stop.color.a == 0 {
                stop.color
            } else {
                next_stop.color
            }
        });
        let interval_end = next_stop.value;
        let span = (interval_end - stop.value).max(f32::EPSILON);
        let t = ((value - stop.value) / span).clamp(0.0, 1.0);
        stop.color.lerp(end_color, t)
    }

    fn sample_stepped(&self, value: f32) -> Rgba8 {
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let index = self.stops.partition_point(|stop| stop.value < value);
        if index >= self.stops.len() {
            return self
                .stops
                .last()
                .map(|stop| stop.color)
                .unwrap_or(Rgba8::TRANSPARENT);
        }
        let right = self.stops[index];
        if value == right.value {
            return right.color;
        }
        self.stops[index - 1].color
    }

    fn first_opaque_value(&self) -> Option<f32> {
        let first = self.stops.first()?;
        (first.color.a == 0).then(|| {
            self.stops
                .iter()
                .find(|stop| stop.color.a > 0)
                .map(|stop| stop.value)
        })?
    }

    pub fn color_for_value(&self, value: f32) -> [u8; 4] {
        self.sample(value).to_array()
    }

    /// A copy of this table with a render-time display threshold. Values
    /// below `threshold` (or within ±threshold when `symmetric`) sample as
    /// transparent — in the viewport, the palette LUT paths, and the
    /// colorbar alike. `None` clears the clamp.
    pub fn with_display_threshold(&self, threshold: Option<f32>, symmetric: bool) -> Self {
        let mut table = self.clone();
        table.display_threshold = threshold;
        table.threshold_is_symmetric = symmetric && threshold.is_some();
        table
    }

    fn value_below_display_threshold(&self, value: f32) -> bool {
        match self.display_threshold {
            Some(threshold) if self.threshold_is_symmetric => value.abs() < threshold,
            Some(threshold) => value < threshold,
            None => false,
        }
    }

    /// Build a precomputed sampler that returns bit-identical colors to
    /// [`ColorTable::sample`] in O(1) per lookup. Use it in per-pixel loops:
    /// it hoists the quantized mode's first-opaque scan out of the hot path
    /// and replaces the per-sample binary search with a bucket index.
    pub fn sampler(&self) -> ColorSampler {
        ColorSampler::new(self)
    }

    pub fn range_folded_color(&self) -> [u8; 4] {
        self.range_folded.to_array()
    }

    pub fn range_folded_rgba(&self) -> Rgba8 {
        self.range_folded
    }

    pub fn signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.name.hash(&mut hasher);
        self.product.hash(&mut hasher);
        self.units.hash(&mut hasher);
        self.range_folded.hash(&mut hasher);
        self.sample_mode.hash(&mut hasher);
        self.stops.len().hash(&mut hasher);
        for stop in &self.stops {
            stop.value.to_bits().hash(&mut hasher);
            stop.color.hash(&mut hasher);
            stop.end_color.hash(&mut hasher);
        }
        self.display_threshold.map(f32::to_bits).hash(&mut hasher);
        self.threshold_is_symmetric.hash(&mut hasher);
        hasher.finish()
    }

    pub fn mirrored_values(&self, name: impl Into<String>) -> Self {
        let stops = self
            .stops
            .iter()
            .map(|stop| ColorStop {
                value: -stop.value,
                color: stop.color,
                end_color: stop.end_color,
            })
            .collect::<Vec<_>>();
        let mut table = Self::from_parts(
            name.into(),
            self.product.clone(),
            self.units.clone(),
            self.range_folded,
            self.sample_mode.mirrored_values(),
            stops,
        )
        .expect("mirrored table preserves valid stops");
        table.display_threshold = self.display_threshold;
        table.threshold_is_symmetric = self.threshold_is_symmetric;
        table
    }

    fn from_parts(
        name: String,
        product: Option<String>,
        units: Option<String>,
        range_folded: Rgba8,
        sample_mode: SampleMode,
        mut stops: Vec<ColorStop>,
    ) -> Result<Self, ColorTableError> {
        stops.retain(|stop| stop.value.is_finite());
        stops.sort_by(|left, right| left.value.total_cmp(&right.value));
        stops.dedup_by(|left, right| {
            if left.value.to_bits() == right.value.to_bits() {
                *left = *right;
                true
            } else {
                false
            }
        });

        if stops.len() < 2 {
            return Err(ColorTableError::NotEnoughStops);
        }

        Ok(Self {
            name,
            product,
            units,
            range_folded,
            sample_mode,
            stops,
            display_threshold: None,
            threshold_is_symmetric: false,
        })
    }
}

/// Precomputed accelerator for [`ColorTable::sample`].
///
/// Produces bit-identical output to the direct path for every input: the same
/// stop list and tail/edge rules are applied, only the segment search is
/// replaced. A uniform bucket grid over the stop value range maps a value to
/// the first stop index that can match it, so the per-sample cost is one
/// multiply, one table load, and (almost always) a single comparison instead
/// of a binary search — plus, for quantized tables, the first-opaque
/// threshold is computed once here instead of rescanning the stops per call.
#[derive(Clone, Debug)]
pub struct ColorSampler {
    sample_mode: SampleMode,
    first_opaque_value: Option<f32>,
    display_threshold: Option<f32>,
    threshold_is_symmetric: bool,
    stops: Vec<ColorStop>,
    range_folded: Rgba8,
    min_value: f32,
    inv_bucket_width: f32,
    bucket_start: Vec<u32>,
}

impl ColorSampler {
    fn new(table: &ColorTable) -> Self {
        let stops = table.stops.clone();
        let min_value = stops.first().map_or(0.0, |stop| stop.value);
        let max_value = stops.last().map_or(0.0, |stop| stop.value);
        let span = max_value - min_value;
        let bucket_count = (stops.len() * 4).clamp(64, 4096);
        let inv_bucket_width = if span > 0.0 {
            bucket_count as f32 / span
        } else {
            0.0
        };

        // bucket_start[b] = first stop index whose own bucket is >= b. The
        // runtime lookup uses the same value->bucket mapping, which is
        // monotone, so the true segment index can never be earlier.
        let mut bucket_start = vec![stops.len() as u32; bucket_count];
        let mut next_bucket = 0usize;
        for (index, stop) in stops.iter().enumerate() {
            let bucket = bucket_for(stop.value, min_value, inv_bucket_width, bucket_count);
            while next_bucket <= bucket {
                bucket_start[next_bucket] = index as u32;
                next_bucket += 1;
            }
        }

        Self {
            sample_mode: table.sample_mode,
            first_opaque_value: table.first_opaque_value(),
            display_threshold: table.display_threshold,
            threshold_is_symmetric: table.threshold_is_symmetric,
            stops,
            range_folded: table.range_folded,
            min_value,
            inv_bucket_width,
            bucket_start,
        }
    }

    pub fn sample(&self, value: f32) -> Rgba8 {
        if !value.is_finite() {
            return Rgba8::TRANSPARENT;
        }
        let below_threshold = match self.display_threshold {
            Some(threshold) if self.threshold_is_symmetric => value.abs() < threshold,
            Some(threshold) => value < threshold,
            None => false,
        };
        if below_threshold {
            return Rgba8::TRANSPARENT;
        }
        match self.sample_mode {
            SampleMode::Interpolated => self.sample_accelerated(value, true),
            SampleMode::Stepped => self.sample_accelerated(value, false),
            SampleMode::GrPal => self.sample_gr_pal_accelerated(value),
            SampleMode::QuantizedInterpolated { step, origin } => {
                if let Some(first_opaque_value) = self.first_opaque_value
                    && value < first_opaque_value
                {
                    return Rgba8::TRANSPARENT;
                }
                self.sample_accelerated(quantize_value(value, step, origin), true)
            }
        }
    }

    pub fn color_for_value(&self, value: f32) -> [u8; 4] {
        self.sample(value).to_array()
    }

    pub fn range_folded_color(&self) -> [u8; 4] {
        self.range_folded.to_array()
    }

    /// GR .pal interval semantics on the bucketed stop index; see
    /// ColorTable::sample_gr_pal.
    fn sample_gr_pal_accelerated(&self, value: f32) -> Rgba8 {
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let bucket = bucket_for(
            value,
            self.min_value,
            self.inv_bucket_width,
            self.bucket_start.len(),
        );
        let mut index = self.bucket_start[bucket] as usize;
        while index < self.stops.len() && self.stops[index].value <= value {
            index += 1;
        }
        let stop = self.stops[index.saturating_sub(1)];
        let Some(next_stop) = self.stops.get(index) else {
            return stop.color;
        };
        let end_color = stop.end_color.unwrap_or({
            if stop.color.a == 0 {
                stop.color
            } else {
                next_stop.color
            }
        });
        let interval_end = next_stop.value;
        let span = (interval_end - stop.value).max(f32::EPSILON);
        let t = ((value - stop.value) / span).clamp(0.0, 1.0);
        stop.color.lerp(end_color, t)
    }

    fn sample_accelerated(&self, value: f32, interpolate: bool) -> Rgba8 {
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let bucket = bucket_for(
            value,
            self.min_value,
            self.inv_bucket_width,
            self.bucket_start.len(),
        );
        let mut index = self.bucket_start[bucket] as usize;
        while index < self.stops.len() && self.stops[index].value < value {
            index += 1;
        }
        if index >= self.stops.len() {
            return self
                .stops
                .last()
                .map(|stop| stop.color)
                .unwrap_or(Rgba8::TRANSPARENT);
        }
        let right = self.stops[index];
        if value == right.value {
            return right.color;
        }
        let left = self.stops[index - 1];
        if interpolate {
            let span = (right.value - left.value).max(f32::EPSILON);
            left.color.lerp(right.color, (value - left.value) / span)
        } else {
            left.color
        }
    }
}

#[inline]
fn bucket_for(value: f32, min_value: f32, inv_bucket_width: f32, bucket_count: usize) -> usize {
    (((value - min_value) * inv_bucket_width) as usize).min(bucket_count - 1)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleMode {
    Interpolated,
    Stepped,
    QuantizedInterpolated {
        step: f32,
        origin: f32,
    },
    /// GR .pal semantics: a stop's interval is SOLID for single-color
    /// entries and a linear ramp for two-color entries; `step:` headers are
    /// legend ticks only (GR never quantizes the display).
    GrPal,
}

impl SampleMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interpolated => "interpolated",
            Self::Stepped => "stepped",
            Self::QuantizedInterpolated { .. } => "quantized stepped",
            Self::GrPal => "GR pal",
        }
    }

    fn step_size(self) -> Option<f32> {
        match self {
            Self::QuantizedInterpolated { step, .. } => Some(step),
            Self::Interpolated | Self::Stepped | Self::GrPal => None,
        }
    }

    fn scale_values(self, scale: f32) -> Self {
        match self {
            Self::QuantizedInterpolated { step, origin } => Self::QuantizedInterpolated {
                step: step * scale,
                origin: origin * scale,
            },
            Self::Interpolated | Self::Stepped | Self::GrPal => self,
        }
    }

    fn mirrored_values(self) -> Self {
        match self {
            Self::QuantizedInterpolated { step, origin } => Self::QuantizedInterpolated {
                step,
                origin: -origin,
            },
            Self::Interpolated | Self::Stepped | Self::GrPal => self,
        }
    }
}

impl Hash for SampleMode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match *self {
            Self::Interpolated => 0_u8.hash(state),
            Self::Stepped => 1_u8.hash(state),
            Self::QuantizedInterpolated { step, origin } => {
                2_u8.hash(state);
                step.to_bits().hash(state);
                origin.to_bits().hash(state);
            }
            Self::GrPal => 3_u8.hash(state),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorTableSet {
    reflectivity: ColorTable,
    velocity: ColorTable,
    spectrum_width: ColorTable,
    correlation_coefficient: ColorTable,
    differential_reflectivity: ColorTable,
    echo_tops: ColorTable,
    vil: ColorTable,
    vil_density: ColorTable,
    hail_size: ColorTable,
    azimuthal_shear: ColorTable,
    differential_phase: ColorTable,
    specific_differential_phase: ColorTable,
    generic: ColorTable,
}

impl ColorTableSet {
    pub fn for_family(&self, family: ColorTableFamily) -> &ColorTable {
        match family {
            ColorTableFamily::Reflectivity => &self.reflectivity,
            ColorTableFamily::Velocity => &self.velocity,
            ColorTableFamily::SpectrumWidth => &self.spectrum_width,
            ColorTableFamily::CorrelationCoefficient => &self.correlation_coefficient,
            ColorTableFamily::DifferentialReflectivity => &self.differential_reflectivity,
            ColorTableFamily::EchoTops => &self.echo_tops,
            ColorTableFamily::Vil => &self.vil,
            ColorTableFamily::VilDensity => &self.vil_density,
            ColorTableFamily::HailSize => &self.hail_size,
            ColorTableFamily::AzimuthalShear => &self.azimuthal_shear,
            ColorTableFamily::DifferentialPhase => &self.differential_phase,
            ColorTableFamily::SpecificDifferentialPhase => &self.specific_differential_phase,
            ColorTableFamily::Generic => &self.generic,
        }
    }

    pub fn set_family(&mut self, family: ColorTableFamily, table: ColorTable) {
        match family {
            ColorTableFamily::Reflectivity => self.reflectivity = table,
            ColorTableFamily::Velocity => self.velocity = table,
            ColorTableFamily::SpectrumWidth => self.spectrum_width = table,
            ColorTableFamily::CorrelationCoefficient => self.correlation_coefficient = table,
            ColorTableFamily::DifferentialReflectivity => self.differential_reflectivity = table,
            ColorTableFamily::EchoTops => self.echo_tops = table,
            ColorTableFamily::Vil => self.vil = table,
            ColorTableFamily::VilDensity => self.vil_density = table,
            ColorTableFamily::HailSize => self.hail_size = table,
            ColorTableFamily::AzimuthalShear => self.azimuthal_shear = table,
            ColorTableFamily::DifferentialPhase => self.differential_phase = table,
            ColorTableFamily::SpecificDifferentialPhase => self.specific_differential_phase = table,
            ColorTableFamily::Generic => self.generic = table,
        }
    }

    pub fn signature_for_family(&self, family: ColorTableFamily) -> u64 {
        self.for_family(family).signature()
    }
}

impl Default for ColorTableSet {
    fn default() -> Self {
        Self {
            reflectivity: builtin_reflectivity_table(),
            velocity: builtin_velocity_table(),
            spectrum_width: builtin_spectrum_width_table(),
            correlation_coefficient: builtin_correlation_coefficient_table(),
            differential_reflectivity: builtin_differential_reflectivity_table(),
            echo_tops: builtin_echo_tops_table(),
            vil: builtin_vil_table(),
            vil_density: builtin_vil_density_table(),
            hail_size: builtin_hail_size_table(),
            azimuthal_shear: builtin_azimuthal_shear_table(),
            differential_phase: builtin_differential_phase_table(),
            specific_differential_phase: builtin_specific_differential_phase_table(),
            generic: builtin_generic_table(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ColorTableError {
    InvalidColor { line: usize, reason: &'static str },
    NotEnoughStops,
}

impl fmt::Display for ColorTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidColor { line, reason } => {
                write!(formatter, "invalid color table line {line}: {reason}")
            }
            Self::NotEnoughStops => write!(formatter, "color table needs at least two color stops"),
        }
    }
}

impl std::error::Error for ColorTableError {}

pub fn builtin_reflectivity_table() -> ColorTable {
    analyst_reflectivity_hd_table()
}

/// Default reflectivity palette: a smooth, perceptually-ordered dBZ ramp
/// following NWS convention (blue→green→yellow→orange→red→magenta→white).
/// Versus the old GR2 default it desaturates the electric low-end blues/greens
/// so light precip reads naturally, keeps lightness rising monotonically into
/// the severe range, and reserves magenta/purple for the 65+ dBZ hail core.
/// Clear-air junk below ~10 dBZ is transparent.
pub fn analyst_reflectivity_hd_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Reflectivity HD", ANALYST_REFLECTIVITY_HD_TABLE)
        .expect("built-in HD reflectivity color table is valid")
}

pub fn builtin_velocity_table() -> ColorTable {
    analyst_hd_velocity_table()
}

/// Default velocity palette: a diverging green-inbound / red-outbound ramp (NWS
/// convention) with saturated high-chroma mid-tones and light extremes. The
/// operational ±Nyquist range renders as vivid, clearly-separated green and
/// red-orange — the old default washed strong velocities out to near-white
/// cream, hiding exactly the derecho RIJ / mesovortex couplets; near-white is
/// reserved for the rarely-reached extreme. Note: this is a conventional
/// diverging ramp tuned for contrast, not a lightness-monotonic perceptually-
/// uniform map; it does avoid a full rainbow hue cycle (cf. Borland & Taylor
/// 2007). For a CVD-safe perceptually-uniform alternative see cmocean `balance`
/// (Thyng et al. 2016) / CET-D (Kovesi 2015) — a future preset.
pub fn analyst_hd_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Velocity HD", ANALYST_HD_VELOCITY_TABLE)
        .expect("built-in HD velocity color table is valid")
}

pub fn tornado_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Tornado VEL", TORNADO_VELOCITY_TABLE)
        .expect("built-in tornado velocity color table is valid")
}

pub fn vortex_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("WxTools Vortex Velo", VORTEX_VELO_TABLE)
        .expect("built-in velocity color table is valid")
}

pub fn builtin_tables_for_family(family: ColorTableFamily) -> Vec<ColorTable> {
    match family {
        ColorTableFamily::Reflectivity => vec![
            builtin_reflectivity_table(),
            gr2_reflectivity_table(),
            analyst_classic_reflectivity_table(),
            nws_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
            tornado_debris_reflectivity_table(),
            clean_light_reflectivity_table(),
        ],
        ColorTableFamily::Velocity => vec![
            builtin_velocity_table(),
            balance_velocity_table(),
            tornado_velocity_table(),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
            sign_check_velocity_table(),
            couplet_pop_velocity_table(),
            gr2_ish_analyst_velocity_table(),
            subtle_srv_velocity_table(),
        ],
        ColorTableFamily::SpectrumWidth => vec![builtin_spectrum_width_table()],
        ColorTableFamily::CorrelationCoefficient => {
            vec![builtin_correlation_coefficient_table(), tornado_cc_table()]
        }
        ColorTableFamily::DifferentialReflectivity => {
            vec![builtin_differential_reflectivity_table()]
        }
        ColorTableFamily::EchoTops => vec![builtin_echo_tops_table()],
        ColorTableFamily::Vil => vec![builtin_vil_table()],
        ColorTableFamily::VilDensity => vec![builtin_vil_density_table()],
        ColorTableFamily::HailSize => vec![builtin_hail_size_table()],
        ColorTableFamily::AzimuthalShear => vec![builtin_azimuthal_shear_table()],
        ColorTableFamily::DifferentialPhase => vec![builtin_differential_phase_table()],
        ColorTableFamily::SpecificDifferentialPhase => {
            vec![builtin_specific_differential_phase_table()]
        }
        ColorTableFamily::Generic => vec![builtin_generic_table()],
    }
}

/// Echo-tops palette. **Values are metres above the radar** (the echo-top grid
/// stores SI height); the labels below mark the familiar kft levels. The
/// conventional rainbow storm-top ramp: blue→cyan→green→yellow→orange→red→
/// magenta→white (a hue progression, not lightness-monotonic).
pub fn builtin_echo_tops_table() -> ColorTable {
    ColorTable::new(
        "Analyst Echo Tops",
        vec![
            stop(1_500.0, 40, 40, 110),    // ~5 kft
            stop(3_000.0, 30, 84, 184),    // ~10
            stop(4_500.0, 0, 150, 200),    // ~15 cyan
            stop(6_100.0, 0, 172, 128),    // ~20
            stop(7_600.0, 36, 182, 58),    // ~25 green
            stop(9_100.0, 150, 200, 40),   // ~30
            stop(10_700.0, 240, 225, 50),  // ~35 yellow
            stop(12_200.0, 245, 150, 30),  // ~40 orange
            stop(13_700.0, 226, 46, 40),   // ~45 red
            stop(15_200.0, 200, 30, 120),  // ~50 magenta
            stop(16_800.0, 170, 84, 204),  // ~55 violet
            stop(18_300.0, 236, 236, 246), // ~60 kft white
        ],
    )
    .expect("built-in echo-tops color table is valid")
}

/// VIL palette (kg m^-2). Blue→cyan→green→yellow→orange→red→magenta→white;
/// the warm/magenta high end (≳ 40–55) flags the large-hail VIL range.
pub fn builtin_vil_table() -> ColorTable {
    ColorTable::new(
        "Analyst VIL",
        vec![
            stop(1.0, 40, 50, 120),
            stop(5.0, 30, 90, 190),
            stop(10.0, 0, 160, 200),
            stop(15.0, 0, 175, 112),
            stop(20.0, 40, 182, 60),
            stop(25.0, 150, 200, 40),
            stop(30.0, 240, 225, 50),
            stop(37.0, 245, 150, 30),
            stop(45.0, 226, 46, 40),
            stop(55.0, 200, 30, 120),
            stop(70.0, 236, 236, 246),
        ],
    )
    .expect("built-in VIL color table is valid")
}

/// VIL Density palette (g m^-3). Blue→green→yellow below the large-hail
/// threshold, then a hard warm break at ~3.5 g/m³ (orange→red→magenta) so the
/// large-hail range (Amburn & Wolf 1997) stands out.
pub fn builtin_vil_density_table() -> ColorTable {
    ColorTable::new(
        "Analyst VIL Density",
        vec![
            stop(0.3, 40, 60, 130),
            stop(1.0, 30, 120, 200),
            stop(1.8, 0, 175, 150),
            stop(2.6, 120, 200, 50),
            stop(3.4, 240, 220, 50),
            stop(3.6, 245, 150, 30),
            stop(4.5, 232, 60, 44),
            stop(5.5, 180, 30, 110),
            stop(7.0, 240, 200, 235),
        ],
    )
    .expect("built-in VIL density color table is valid")
}

/// MEHS palette (mm). Breaks at report thresholds: 19 mm (3/4"), the 25 mm
/// (1") severe criterion, 44 mm (1.75" golf ball) and 50 mm (2") — sub-severe
/// sizes stay cool, severe goes warm, giant hail goes magenta->white.
pub fn builtin_hail_size_table() -> ColorTable {
    ColorTable::new(
        "Analyst MEHS",
        vec![
            stop(5.0, 60, 110, 170),
            stop(15.0, 70, 160, 200),
            stop(19.0, 90, 190, 120),
            stop(25.0, 235, 215, 60),
            stop(38.0, 245, 150, 40),
            stop(44.0, 230, 70, 45),
            stop(50.0, 200, 35, 100),
            stop(70.0, 240, 160, 235),
            stop(100.0, 250, 245, 250),
        ],
    )
    .expect("built-in hail size color table is valid")
}

/// Azimuthal-shear palette (×10^-3 s^-1), diverging about zero: near-zero is
/// dark/neutral, cyclonic-sense shear (positive) warms through orange→red→
/// white (mesocyclone/TVS), anticyclonic-sense (negative) cools through
/// blue→violet. Magnitude brightens so rotation signatures pop out of the
/// mostly-zero field.
pub fn builtin_azimuthal_shear_table() -> ColorTable {
    ColorTable::new(
        "Analyst Az Shear",
        vec![
            stop(-25.0, 150, 90, 220),
            stop(-15.0, 80, 96, 210),
            stop(-8.0, 52, 92, 150),
            stop(-3.0, 40, 48, 66),
            stop(0.0, 30, 32, 36),
            stop(3.0, 70, 56, 38),
            stop(8.0, 160, 110, 30),
            stop(15.0, 234, 120, 28),
            stop(22.0, 248, 60, 44),
            stop(32.0, 252, 220, 150),
        ],
    )
    .expect("built-in azimuthal-shear color table is valid")
}

/// Differential phase (ΦDP, degrees) palette — a monotonic perceptual ramp over
/// the operational 0–180° range (extending toward 360°). ΦDP accumulates with
/// propagation through rain, so a smooth low→high ramp reads the along-beam
/// phase gradient.
pub fn builtin_differential_phase_table() -> ColorTable {
    ColorTable::new(
        "Analyst PHI",
        vec![
            stop(0.0, 40, 44, 78),
            stop(30.0, 36, 96, 180),
            stop(60.0, 0, 158, 170),
            stop(90.0, 70, 184, 70),
            stop(120.0, 210, 206, 50),
            stop(150.0, 240, 150, 32),
            stop(180.0, 226, 60, 44),
            stop(270.0, 170, 40, 110),
            stop(360.0, 232, 200, 230),
        ],
    )
    .expect("built-in differential-phase color table is valid")
}

/// Specific differential phase (KDP, °/km) palette. Diverging about zero:
/// near-zero neutral, the small negative range (backscatter differential phase /
/// noise) cool, and positive KDP — proportional to liquid-water content / heavy
/// rain and big drops — warming green→yellow→orange→red.
pub fn builtin_specific_differential_phase_table() -> ColorTable {
    ColorTable::new(
        "Analyst KDP",
        vec![
            stop(-1.0, 70, 96, 170),
            stop(-0.3, 90, 110, 140),
            stop(0.0, 60, 64, 70),
            stop(0.3, 70, 120, 80),
            stop(0.75, 90, 180, 70),
            stop(1.5, 210, 210, 50),
            stop(2.5, 244, 158, 32),
            stop(4.0, 230, 70, 44),
            stop(7.0, 180, 30, 96),
        ],
    )
    .expect("built-in specific-differential-phase color table is valid")
}

/// Default Correlation Coefficient (ρhv) palette, ~0.2–1.05. Standard rainbow
/// convention: low CC (non-meteorological — debris, clutter, birds, chaff, and
/// the tornadic debris signature) reads as cool blue/teal, dropping to dark at
/// the noise floor; meteorological precip (CC ≳ 0.95) reads warm, peaking near
/// white at ρhv→1. Most of the color resolution is packed into 0.80–1.00 where
/// the diagnostic action is. A debris ball therefore shows as the classic cool
/// "hole" inside warm precip.
pub fn builtin_correlation_coefficient_table() -> ColorTable {
    ColorTable::new(
        "Analyst CC",
        vec![
            stop(0.20, 48, 48, 56),
            stop(0.45, 72, 60, 150),
            stop(0.65, 46, 96, 200),
            stop(0.80, 0, 168, 196),
            stop(0.88, 64, 196, 92),
            stop(0.92, 208, 216, 52),
            stop(0.95, 245, 158, 32),
            stop(0.97, 226, 46, 40),
            stop(0.99, 150, 22, 30),
            stop(1.00, 236, 236, 244),
            stop(1.05, 255, 255, 255),
        ],
    )
    .expect("built-in CC color table is valid")
}

/// CC variant tuned for tornadic-debris hunting: exaggerates the 0.7–0.95 drop
/// so debris signatures (ρhv ~0.5–0.8 co-located with rotation) pop hard.
pub fn tornado_cc_table() -> ColorTable {
    ColorTable::new(
        "Analyst CC Debris",
        vec![
            stop(0.30, 30, 30, 38),
            stop(0.50, 120, 40, 150),
            stop(0.70, 210, 40, 60),
            stop(0.80, 245, 130, 30),
            stop(0.88, 240, 224, 60),
            stop(0.93, 70, 200, 90),
            stop(0.96, 40, 150, 220),
            stop(0.99, 40, 70, 180),
            stop(1.02, 230, 232, 245),
        ],
    )
    .expect("built-in CC debris color table is valid")
}

/// Default Differential Reflectivity (ZDR) palette, ~−2…+8 dB. Diverging about
/// 0 dB (spherical scatterers: small/dry hail, clutter → neutral gray). Cool
/// blues for the uncommon negatives (conical graupel, vertically-aligned ice);
/// warm green→yellow→orange→red→magenta for positive ZDR (oblate raindrops, big
/// drops, ZDR columns marking updrafts). Resolution favors 0…+4 dB where most
/// meteorological signal lives.
pub fn builtin_differential_reflectivity_table() -> ColorTable {
    ColorTable::new(
        "Analyst ZDR",
        vec![
            stop(-4.0, 60, 30, 96),
            stop(-2.0, 56, 70, 168),
            stop(-0.5, 96, 150, 196),
            stop(0.0, 140, 140, 140),
            stop(0.5, 150, 168, 120),
            stop(1.0, 120, 192, 88),
            stop(2.0, 224, 220, 60),
            stop(3.0, 245, 158, 32),
            stop(4.0, 226, 52, 40),
            stop(5.5, 176, 28, 92),
            stop(7.0, 206, 86, 200),
            stop(8.0, 240, 200, 240),
        ],
    )
    .expect("built-in ZDR color table is valid")
}

pub fn analyst_reflectivity_table() -> ColorTable {
    ColorTable::new_stepped(
        "Analyst High Contrast REF",
        vec![
            stop(-10.0, 5, 8, 18),
            stop(0.0, 18, 36, 76),
            stop(7.5, 23, 92, 157),
            stop(15.0, 26, 158, 191),
            stop(22.5, 17, 146, 62),
            stop(30.0, 84, 188, 54),
            stop(37.5, 242, 216, 47),
            stop(45.0, 239, 120, 34),
            stop(52.5, 221, 42, 38),
            stop(60.0, 174, 32, 112),
            stop(67.5, 214, 76, 218),
            stop(75.0, 245, 245, 245),
        ],
    )
    .expect("built-in analyst reflectivity color table is valid")
}

pub fn nws_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("NWS Classic REF", NWS_CLASSIC_REFLECTIVITY_TABLE)
        .expect("built-in nws reflectivity color table is valid")
}

pub fn analyst_classic_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Classic REF", ANALYST_CLASSIC_REFLECTIVITY_TABLE)
        .expect("built-in analyst classic reflectivity color table is valid")
}

pub fn gr2_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("GR2Analyst Classic REF", GR2_REFLECTIVITY_TABLE)
        .expect("built-in GR2 reflectivity color table is valid")
}

pub fn storm_detail_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Storm Detail REF", STORM_DETAIL_REFLECTIVITY_TABLE)
        .expect("built-in storm detail reflectivity color table is valid")
}

pub fn hail_core_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Hail Core REF", HAIL_CORE_REFLECTIVITY_TABLE)
        .expect("built-in hail core reflectivity color table is valid")
}

pub fn low_precip_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Low Precip REF", LOW_PRECIP_REFLECTIVITY_TABLE)
        .expect("built-in low precip reflectivity color table is valid")
}

pub fn dark_scope_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Dark Scope REF", DARK_SCOPE_REFLECTIVITY_TABLE)
        .expect("built-in dark scope reflectivity color table is valid")
}

pub fn tornado_debris_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Tornado Debris REF", TORNADO_DEBRIS_REFLECTIVITY_TABLE)
        .expect("built-in tornado debris reflectivity color table is valid")
}

pub fn clean_light_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Clean Light REF", CLEAN_LIGHT_REFLECTIVITY_TABLE)
        .expect("built-in clean light reflectivity color table is valid")
}

pub fn analyst_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Pro VEL", ANALYST_PRO_VELOCITY_TABLE)
        .expect("built-in analyst velocity color table is valid")
}

pub fn nws_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("NWS Classic VEL", NWS_VELOCITY_TABLE)
        .expect("built-in nws velocity color table is valid")
}

pub fn gr2_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("GR2Analyst Classic VEL", GR2_VELOCITY_TABLE)
        .expect("built-in GR2 velocity color table is valid")
}

pub fn tight_couplet_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Tight Couplet VEL", TIGHT_COUPLET_VELOCITY_TABLE)
        .expect("built-in tight couplet velocity color table is valid")
}

pub fn radarscope_contrast_velocity_table() -> ColorTable {
    ColorTable::parse_stepped(
        "RadarScope Contrast VEL",
        RADARSCOPE_CONTRAST_VELOCITY_TABLE,
    )
    .expect("built-in radarscope contrast velocity color table is valid")
}

pub fn sign_check_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Sign Check VEL", SIGN_CHECK_VELOCITY_TABLE)
        .expect("built-in sign-check velocity color table is valid")
}

pub fn couplet_pop_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Couplet Pop VEL", COUPLET_POP_VELOCITY_TABLE)
        .expect("built-in couplet pop velocity color table is valid")
}

pub fn gr2_ish_analyst_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("GR2-ish Analyst VEL", GR2_ISH_ANALYST_VELOCITY_TABLE)
        .expect("built-in GR2-ish analyst velocity color table is valid")
}

pub fn subtle_srv_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Subtle SRV VEL", SUBTLE_SRV_VELOCITY_TABLE)
        .expect("built-in subtle SRV velocity color table is valid")
}

/// Colorblind-safe, perceptually-uniform diverging velocity palette modeled on
/// cmocean `balance` (Thyng et al. 2016) / CET-D (Kovesi 2015): deep blue
/// (inbound) → light neutral (zero) → deep red (outbound). Unlike the green/red
/// default this uses the blue↔red axis, which is robust to red-green color
/// vision deficiency, and lightness IS monotonic on each arm (dark at the
/// extremes, light at zero) — a genuinely perceptual ramp for accessibility.
pub fn balance_velocity_table() -> ColorTable {
    ColorTable::new(
        "Balance VEL (CVD-safe)",
        vec![
            stop(-70.0, 18, 24, 92),
            stop(-50.0, 28, 70, 160),
            stop(-30.0, 60, 130, 210),
            stop(-15.0, 132, 186, 230),
            stop(-5.0, 200, 220, 240),
            stop(0.0, 244, 244, 246),
            stop(5.0, 242, 214, 204),
            stop(15.0, 234, 164, 150),
            stop(30.0, 220, 100, 90),
            stop(50.0, 180, 44, 50),
            stop(70.0, 110, 14, 30),
        ],
    )
    .expect("built-in balance velocity color table is valid")
}

pub fn nws_split_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("NWS Split VEL", NWS_SPLIT_VELOCITY_TABLE)
        .expect("built-in split velocity color table is valid")
}

pub fn dark_analyst_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Dark Analyst VEL", DARK_ANALYST_VELOCITY_TABLE)
        .expect("built-in dark analyst velocity color table is valid")
}

pub fn builtin_spectrum_width_table() -> ColorTable {
    ColorTable::new(
        "Analyst Spectrum Width",
        vec![
            stop(0.0, 9, 20, 32),
            stop(1.0, 24, 52, 100),
            stop(2.0, 22, 102, 172),
            stop(3.0, 18, 152, 180),
            stop(4.0, 36, 174, 98),
            stop(5.5, 160, 188, 58),
            stop(7.0, 232, 190, 54),
            stop(9.0, 238, 112, 42),
            stop(12.0, 216, 44, 50),
            stop(16.0, 160, 36, 136),
            stop(24.0, 235, 235, 235),
        ],
    )
    .expect("built-in spectrum width color table is valid")
}

pub fn builtin_generic_table() -> ColorTable {
    ColorTable::new(
        "Analyst Generic",
        vec![
            stop(0.0, 34, 40, 64),
            stop(10.0, 34, 82, 130),
            stop(25.0, 34, 132, 172),
            stop(40.0, 58, 166, 140),
            stop(55.0, 116, 180, 92),
            stop(70.0, 218, 188, 74),
            stop(85.0, 224, 114, 56),
            stop(100.0, 210, 64, 68),
        ],
    )
    .expect("built-in generic color table is valid")
}

fn stop(value: f32, r: u8, g: u8, b: u8) -> ColorStop {
    ColorStop {
        value,
        color: Rgba8::opaque(r, g, b),
        end_color: None,
    }
}

fn default_range_folded_color() -> Rgba8 {
    Rgba8::new(126, 80, 196, 245)
}

fn lerp_u8(left: u8, right: u8, amount: f32) -> u8 {
    ((left as f32 + (right as f32 - left as f32) * amount).round()).clamp(0.0, 255.0) as u8
}

fn quantize_value(value: f32, step: f32, origin: f32) -> f32 {
    if !step.is_finite() || step <= 0.0 {
        return value;
    }
    ((value - origin) / step).round() * step + origin
}

fn normalize_line(line: &str) -> String {
    line.replace('\u{a0}', " ")
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    if let Some((key, value)) = line.split_once(':') {
        return Some((key, value));
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    Some((parts.next()?, parts.next()?))
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_color_stop(
    value: &str,
    expects_alpha: bool,
    solid: bool,
    line: usize,
) -> Result<ColorStop, ColorTableError> {
    let numbers = parse_numbers(value);
    let components = if expects_alpha { 4 } else { 3 };
    if numbers.len() < 1 + components {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "expected value plus RGB or RGBA components",
        });
    }
    let read_color = |offset: usize| -> Result<Rgba8, ColorTableError> {
        Ok(Rgba8::new(
            byte_component(numbers[offset], line)?,
            byte_component(numbers[offset + 1], line)?,
            byte_component(numbers[offset + 2], line)?,
            if expects_alpha {
                byte_component(numbers[offset + 3], line)?
            } else {
                255
            },
        ))
    };
    let color = read_color(1)?;
    // `Color:` rows interpolate to the next color unless they provide a
    // second interval-end color. `SolidColor:` rows hold a hard band.
    let end_color = if solid {
        Some(color)
    } else {
        (numbers.len() > 2 * components)
            .then(|| read_color(1 + components))
            .transpose()?
    };
    Ok(ColorStop {
        value: numbers[0],
        color,
        end_color,
    })
}

fn parse_color_only(value: &str, line: usize) -> Result<Rgba8, ColorTableError> {
    let numbers = parse_numbers(value);
    if numbers.len() < 3 {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "expected RGB components",
        });
    }
    Ok(Rgba8::new(
        byte_component(numbers[0], line)?,
        byte_component(numbers[1], line)?,
        byte_component(numbers[2], line)?,
        numbers
            .get(3)
            .map(|value| byte_component(*value, line))
            .transpose()?
            .unwrap_or(245),
    ))
}

fn parse_numbers(value: &str) -> Vec<f32> {
    value
        .split(|character: char| {
            character.is_ascii_whitespace() || character == ',' || character == ';'
        })
        .filter_map(|token| {
            let token = token.trim();
            (!token.is_empty())
                .then(|| token.parse::<f32>().ok())
                .flatten()
        })
        .collect()
}

fn byte_component(value: f32, line: usize) -> Result<u8, ColorTableError> {
    if !(0.0..=255.0).contains(&value) {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "color component must be 0-255",
        });
    }
    Ok(value.round() as u8)
}

fn parse_positive_f32(value: &str) -> Option<f32> {
    let value = parse_numbers(value).first().copied()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

fn parse_sample_mode(value: &str) -> Option<SampleMode> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "false" | "no" | "off" | "0" | "step" | "stepped" | "discrete" | "nearest" => {
            Some(SampleMode::Stepped)
        }
        "true" | "yes" | "on" | "1" | "smooth" | "linear" | "interpolate" | "interpolated" => {
            Some(SampleMode::Interpolated)
        }
        _ => None,
    }
}

fn unit_value_to_mps_scale(units: &str) -> f32 {
    let units = units.trim().to_ascii_lowercase();
    match units.as_str() {
        "kt" | "kts" | "knot" | "knots" => KNOT_TO_MPS,
        "mph" | "mi/h" => MPH_TO_MPS,
        _ => 1.0,
    }
}

const ANALYST_REFLECTIVITY_HD_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -30 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 110 120 150
color: 15 44 110 214
color: 20 36 168 188
color: 25 44 190 96
color: 30 30 150 44
color: 35 166 206 44
color: 40 244 232 56
color: 45 248 186 40
color: 50 238 110 28
color: 55 224 38 38
color: 60 176 22 28
color: 65 240 72 180
color: 70 168 60 200
color: 75 214 158 232
color: 80 255 255 255
"#;

const GR2_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 4 233 231
color: 15 1 159 244
color: 20 3 0 244
color: 25 2 253 2
color: 30 1 197 1
color: 35 0 142 0
color: 40 253 248 2
color: 45 229 188 0
color: 50 253 149 0
color: 55 253 0 0
color: 62.5 212 0 0
color: 67.5 188 0 0
color: 72.5 232 32 206
color: 80 156 70 206
color: 92.5 255 255 255
"#;

const NWS_CLASSIC_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 4 233 231
color: 15 1 159 244
color: 20 3 0 244
color: 25 2 253 2
color: 30 1 197 1
color: 35 0 142 0
color: 40 253 248 2
color: 45 229 188 0
color: 50 253 149 0
color: 55 253 0 0
color: 62.5 212 0 0
color: 67.5 188 0 0
color: 72.5 232 32 206
color: 80 156 70 206
color: 92.5 255 255 255
"#;

const ANALYST_CLASSIC_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 0 204 220
color: 15 0 132 232
color: 20 12 58 226
color: 25 0 222 44
color: 30 0 174 24
color: 35 0 124 12
color: 40 235 226 34
color: 45 238 174 28
color: 50 242 112 22
color: 55 238 28 30
color: 62.5 190 0 18
color: 67.5 150 0 18
color: 72.5 214 42 180
color: 80 150 82 198
color: 92.5 246 246 246
"#;

const STORM_DETAIL_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -10 0 0 0 0
color4: 0 0 0 0 0
color: 5 18 42 86
color: 10 25 92 154
color: 15 31 164 206
color: 20 28 184 114
color: 25 21 132 44
color: 30 88 178 42
color: 35 218 226 45
color: 40 251 180 32
color: 45 254 101 22
color: 50 238 32 28
color: 55 174 0 22
color: 60 214 52 168
color: 65 142 34 214
color: 70 228 228 236
color: 80 255 255 255
"#;

const HAIL_CORE_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 35 98 164
color: 15 33 168 210
color: 20 16 172 78
color: 25 0 120 36
color: 30 82 170 40
color: 35 234 232 36
color: 40 252 168 22
color: 45 252 88 18
color: 50 246 26 28
color: 57.5 176 0 16
color: 65 154 0 28
color: 70 206 32 174
color: 77.5 152 74 204
color: 80 255 255 255
color: 87.5 112 228 255
color: 95 255 255 255
"#;

const LOW_PRECIP_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -15 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 38 116 174
color: 15 42 184 214
color: 20 58 204 132
color: 25 44 154 66
color: 30 84 188 50
color: 35 224 226 64
color: 40 250 178 50
color: 45 244 96 42
color: 50 218 44 52
color: 57.5 160 26 78
color: 65 170 28 128
color: 72.5 202 68 196
color: 80 154 84 204
color: 90 238 238 244
"#;

const DARK_SCOPE_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 38 86 128
color: 15 52 136 170
color: 20 30 158 86
color: 25 18 118 48
color: 30 78 164 44
color: 35 196 206 54
color: 40 232 156 42
color: 45 234 88 34
color: 50 218 38 40
color: 57.5 156 24 30
color: 65 168 30 130
color: 72.5 196 70 204
color: 80 154 82 210
color: 87.5 226 226 232
color: 95 255 255 255
"#;

const TORNADO_DEBRIS_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 30 96 152
color: 15 34 152 196
color: 20 26 190 112
color: 25 0 146 52
color: 30 72 176 42
color: 35 214 220 48
color: 40 246 174 32
color: 45 250 102 26
color: 50 238 32 30
color: 57.5 178 0 24
color: 65 164 0 40
color: 70 206 36 168
color: 77.5 224 94 210
color: 87.5 176 230 255
color: 95 255 255 255
"#;

const CLEAN_LIGHT_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 1
color4: -15 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 30 114 160
color: 17.5 38 164 190
color: 22.5 42 186 110
color: 27.5 22 132 52
color: 32.5 94 176 48
color: 37.5 220 218 58
color: 42.5 242 160 42
color: 47.5 236 90 38
color: 52.5 218 38 44
color: 60 156 22 34
color: 67.5 174 34 132
color: 75 206 72 198
color: 82.5 156 84 206
color: 92.5 238 238 242
"#;

const VORTEX_VELO_TABLE: &str = r#"
units: MPH
step: 20
scale: 2.237
product: BV
color: 0 115 115 115
color: .1 134 113 116
color: 5 130 3 3
color: 30 238 0 0
color: 40 255 87 1
color: 55 255 143 1
color: 70 255 239 2
color: 90 255 252 81
color: 120 255 255 255
color: 130 128 128 128
color: -4.99 70 129 68
color: -5 2 139 2
color: -30 4 239 16
color: -40 4 169 86
color: -55 4 92 162
color: -70 4 5 254
color: -90 4 87 254
color: -110 5 177 255
color: -130 0 255 255
"#;

const ANALYST_HD_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -80 204 236 255
color: -64 150 208 255
color: -50 74 168 255
color: -40 18 120 240
color: -32 0 150 208
color: -26 0 196 168
color: -20 0 214 110
color: -15 24 208 74
color: -10 48 190 70
color: -6 46 150 74
color: -2 78 116 88
color: 0 105 105 105
color: 2 132 94 84
color: 6 180 70 56
color: 10 214 44 40
color: 15 244 34 34
color: 20 255 74 28
color: 26 255 120 0
color: 32 255 160 0
color: 40 255 200 0
color: 50 255 230 90
color: 64 255 244 170
color: 80 255 255 235
"#;

const TORNADO_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 236 255 255
color: -58 126 220 255
color: -48 166 236 255
color: -38 210 250 255
color: -30 246 255 255
color: -24 232 255 250
color: -18 0 156 54
color: -13 18 232 54
color: -9 82 244 104
color: -5 36 136 54
color: -2 84 100 84
color: 0 112 112 112
color: 2 120 86 84
color: 5 154 46 44
color: 9 216 28 28
color: 14 255 34 40
color: 20 242 0 0
color: 24 255 238 218
color: 28 255 255 238
color: 34 255 224 168
color: 42 255 248 220
color: 50 255 255 240
color: 58 255 230 190
color: 64 255 202 130
color: 70 255 240 204
"#;

const GR2_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 255 255
color: -55 0 170 255
color: -42 0 80 255
color: -32 0 180 80
color: -24 0 220 0
color: -16 0 148 0
color: -8 74 132 74
color: -2 96 108 96
color: 0 128 128 128
color: 2 126 94 94
color: 8 156 44 44
color: 16 198 0 0
color: 24 244 0 0
color: 32 255 116 0
color: 42 255 220 0
color: 55 255 255 255
color: 70 172 172 172
"#;

const TIGHT_COUPLET_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 230 255 255
color: -50 54 236 214
color: -36 0 188 122
color: -26 0 114 48
color: -18 0 176 34
color: -12 32 252 46
color: -7 0 176 34
color: -3 36 112 50
color: -1 78 94 78
color: 0 112 112 112
color: 1 112 78 78
color: 3 152 36 36
color: 7 246 22 22
color: 12 255 42 42
color: 18 202 0 0
color: 26 142 0 0
color: 36 110 0 0
color: 50 238 124 132
color: 70 255 255 255
"#;

const RADARSCOPE_CONTRAST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 216 255 255
color: -58 126 220 255
color: -48 166 236 255
color: -38 210 250 255
color: -30 246 255 255
color: -24 232 255 250
color: -22 210 248 226
color: -16 0 224 54
color: -11 42 255 66
color: -7 106 240 116
color: -4 46 134 54
color: -1 98 104 96
color: 0 122 122 122
color: 1 128 96 96
color: 4 156 64 62
color: 7 198 42 42
color: 11 246 28 28
color: 16 255 40 46
color: 22 244 0 24
color: 24 255 238 218
color: 28 255 255 238
color: 36 255 220 172
color: 44 255 250 224
color: 50 255 255 238
color: 56 255 232 190
color: 62 255 204 134
color: 70 255 242 202
"#;

const SIGN_CHECK_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
mode: stepped
rf: 180 80 255 255
color: -100 0 0 255
color: -0.01 0 0 255
color: 0 120 120 120
color: 0.01 255 0 0
color: 100 255 0 0
"#;

const COUPLET_POP_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 238 255 255
color: -58 92 238 216
color: -46 20 206 152
color: -36 0 150 82
color: -28 0 92 42
color: -21 0 172 58
color: -15 0 236 44
color: -10 34 186 48
color: -6 36 122 50
color: -2 78 98 76
color: 0 92 92 92
color: 2 104 72 70
color: 6 132 34 34
color: 10 214 24 24
color: 15 255 34 34
color: 21 236 16 38
color: 28 180 8 34
color: 36 122 6 34
color: 46 196 78 96
color: 58 240 184 190
color: 70 255 255 255
"#;

const GR2_ISH_ANALYST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 252 252
color: -55 0 174 244
color: -42 20 90 238
color: -32 0 176 82
color: -24 0 214 0
color: -16 0 150 0
color: -8 74 132 74
color: -2 96 108 96
color: 0 124 124 124
color: 2 126 94 94
color: 8 160 42 42
color: 16 204 0 0
color: 24 246 0 0
color: 32 255 92 38
color: 42 246 156 128
color: 55 255 222 222
color: 70 172 172 172
"#;

const SUBTLE_SRV_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 184 236 230
color: -55 90 206 190
color: -42 32 168 132
color: -32 12 122 76
color: -24 18 88 52
color: -16 36 140 64
color: -10 62 196 82
color: -5 58 132 70
color: -1 82 98 84
color: 0 94 94 94
color: 1 104 86 84
color: 5 128 58 54
color: 10 188 52 48
color: 16 222 64 58
color: 24 184 42 54
color: 32 138 34 54
color: 42 190 96 114
color: 55 224 184 190
color: 70 242 242 242
"#;

const NWS_SPLIT_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 240 240
color: -55 0 150 240
color: -42 0 62 220
color: -32 0 150 60
color: -24 0 210 0
color: -16 0 136 0
color: -8 76 140 76
color: -2 104 118 104
color: 0 130 130 130
color: 2 142 104 104
color: 8 168 54 54
color: 16 210 0 0
color: 24 248 0 0
color: 32 255 118 0
color: 42 255 226 0
color: 55 255 255 255
color: 70 170 170 170
"#;

const DARK_ANALYST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 210 246 240
color: -55 82 210 196
color: -42 0 164 126
color: -32 0 114 68
color: -24 0 80 44
color: -16 0 142 50
color: -10 20 206 42
color: -5 34 126 46
color: -1 72 88 74
color: 0 94 94 94
color: 1 102 72 72
color: 5 132 34 34
color: 10 208 24 24
color: 16 238 42 42
color: 24 188 18 36
color: 32 128 16 36
color: 42 198 92 112
color: 55 232 202 206
color: 70 250 250 250
"#;

const ANALYST_PRO_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
mode: stepped
color: -70 222 255 255
color: -58 126 220 255
color: -46 170 238 255
color: -36 214 250 255
color: -28 246 255 255
color: -24 232 255 250
color: -21 210 248 226
color: -15 0 226 58
color: -10 42 214 70
color: -6 42 132 54
color: -2 82 98 80
color: 0 110 110 110
color: 2 116 84 84
color: 6 148 42 42
color: 10 204 30 30
color: 15 248 36 42
color: 21 255 78 86
color: 24 255 238 218
color: 28 255 255 238
color: 36 255 222 174
color: 46 255 250 226
color: 58 255 255 238
color: 66 255 210 146
color: 70 255 240 220
"#;

const NWS_VELOCITY_TABLE: &str = r#"
product: BV
units: kt
color: -120 0 255 255
color: -100 0 160 255
color: -80 0 64 255
color: -60 0 160 80
color: -40 0 220 0
color: -20 0 128 0
color: -5 85 145 85
color: 0 128 128 128
color: 5 150 90 90
color: 20 160 0 0
color: 40 230 0 0
color: 60 255 130 0
color: 80 255 230 0
color: 100 255 255 255
color: 120 170 170 170
"#;

#[cfg(test)]
mod gr_pal_tests {
    use super::*;

    /// The community .pal that exposed the GR-semantics gaps (RadarOmega
    /// reflectivity): color4 alpha stop, a two-color gray ramp, interpolated
    /// single-color levels, and a Step: header that must NOT quantize.
    const RADAR_OMEGA: &str = "units: dBZ
step: 10
product: BR

color4: -10 7 59 71 0
color: 0 62 69 71 191 193 197
color: 20 135 229 125
color: 30 48 102 43
color: 35 253 227 0
color: 50 254 26 0 181 0 52
color: 60 163 0 136 254 4 250
color: 70 67 190 254 19 144 242
color: 80 166 176 150 255 231 188
color: 85 255 231 188
";

    #[test]
    fn gr_pal_matches_gr2analyst_semantics() {
        let table = ColorTable::parse_gr_pal("RadarOmega", RADAR_OMEGA).expect("parse");
        // Step: is legend-only — no quantization mode.
        assert_eq!(table.sample_mode_label(), "GR pal");
        // color4 alpha threshold stop: the [-10, 0) interval remains hidden.
        assert_eq!(table.sample(-5.0).a, 0);
        // Two-color ramp 0..20: midpoint is halfway gray.
        let mid = table.sample(10.0);
        assert!((mid.r as i32 - 126).abs() <= 2, "{mid:?}");
        assert!((mid.g as i32 - 131).abs() <= 2, "{mid:?}");
        // Single-color levels interpolate between the full table stops.
        assert_ne!(table.sample(21.0), table.sample(29.0));
        let green = table.sample(25.0);
        assert!((green.r as i32 - 92).abs() <= 2, "{green:?}");
        assert!((green.g as i32 - 166).abs() <= 2, "{green:?}");
        // Two-color red ramp 50..60: midpoint between (254,26,0)-(181,0,52).
        let red = table.sample(55.0);
        assert!((red.r as i32 - 217).abs() <= 3, "{red:?}");
        assert!((red.b as i32 - 26).abs() <= 3, "{red:?}");
        // The sampler agrees with the table.
        let sampler = ColorSampler::new(&table);
        for value in [-5.0f32, 10.0, 25.0, 40.0, 55.0, 72.0, 86.0] {
            assert_eq!(sampler.sample(value), table.sample(value), "at {value}");
        }
    }

    #[test]
    fn gr_pal_solidcolor_keeps_explicit_hard_cut() {
        let table = ColorTable::parse_gr_pal(
            "solid",
            "color: 0 0 0 0\nsolidcolor: 10 100 0 0\ncolor: 20 200 0 0",
        )
        .expect("parse");

        assert_eq!(table.sample(5.0), Rgba8::opaque(50, 0, 0));
        assert_eq!(table.sample(11.0), Rgba8::opaque(100, 0, 0));
        assert_eq!(table.sample(19.0), Rgba8::opaque(100, 0, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nudge_up(value: f32) -> f32 {
        f32::from_bits(if value >= 0.0 {
            value.to_bits() + 1
        } else {
            value.to_bits() - 1
        })
    }

    fn nudge_down(value: f32) -> f32 {
        f32::from_bits(if value > 0.0 {
            value.to_bits() - 1
        } else {
            value.to_bits() + 1
        })
    }

    #[test]
    fn sampler_matches_direct_sampling_exactly() {
        const FAMILIES: [ColorTableFamily; 12] = [
            ColorTableFamily::Reflectivity,
            ColorTableFamily::Velocity,
            ColorTableFamily::SpectrumWidth,
            ColorTableFamily::CorrelationCoefficient,
            ColorTableFamily::DifferentialReflectivity,
            ColorTableFamily::EchoTops,
            ColorTableFamily::Vil,
            ColorTableFamily::VilDensity,
            ColorTableFamily::AzimuthalShear,
            ColorTableFamily::DifferentialPhase,
            ColorTableFamily::SpecificDifferentialPhase,
            ColorTableFamily::Generic,
        ];
        let mut tables: Vec<ColorTable> = FAMILIES
            .iter()
            .flat_map(|family| builtin_tables_for_family(*family))
            .collect();
        // A quantized table with a transparent lead stop exercises the
        // first-opaque clamp; the stepped variant exercises left-stop picks.
        let quantized = ColorTable::parse(
            "quantized with transparent lead",
            r#"
            Product: Velocity
            Units: MPS
            Step: 5
            Color4: -100 0 0 0 0
            Color4: -50 0 50 255 255
            Color: 0 255 255 255
            Color4: 64 255 0 0 255
            "#,
        )
        .unwrap();
        tables.push(quantized);
        tables.push(
            ColorTable::new_stepped(
                "stepped",
                vec![
                    ColorStop {
                        value: -10.0,
                        color: Rgba8 {
                            r: 1,
                            g: 2,
                            b: 3,
                            a: 0,
                        },
                        end_color: None,
                    },
                    ColorStop {
                        value: 0.5,
                        color: Rgba8 {
                            r: 200,
                            g: 100,
                            b: 50,
                            a: 255,
                        },
                        end_color: None,
                    },
                    ColorStop {
                        value: 33.25,
                        color: Rgba8 {
                            r: 9,
                            g: 8,
                            b: 7,
                            a: 128,
                        },
                        end_color: None,
                    },
                ],
            )
            .unwrap(),
        );

        for table in &tables {
            let sampler = table.sampler();
            let stops = table.stops();
            let min = stops.first().unwrap().value;
            let max = stops.last().unwrap().value;
            let span = (max - min).max(1.0);

            let mut probes: Vec<f32> = Vec::new();
            for index in 0..=4000 {
                probes.push(min - 0.1 * span + (index as f32) * (1.2 * span / 4000.0));
            }
            for stop in stops {
                probes.push(stop.value);
                probes.push(nudge_up(stop.value));
                probes.push(nudge_down(stop.value));
            }
            probes.extend([
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::MAX,
                f32::MIN,
                0.0,
                -0.0,
            ]);

            for value in probes {
                assert_eq!(
                    sampler.color_for_value(value),
                    table.color_for_value(value),
                    "table '{}' diverges at value {value}",
                    table.name()
                );
            }
            assert_eq!(sampler.range_folded_color(), table.range_folded_color());
        }
    }

    #[test]
    fn parses_wxtools_velocity_units_and_unsorted_stops() {
        let table = ColorTable::parse(
            "Vortex Velo sample",
            r#"
            units: MPH
            product: BV
            color: 0 115 115 115
            color: 5 130 3 3
            color: -5 2 139 2
            "#,
        )
        .expect("table parses");

        assert_eq!(table.product(), Some("BV"));
        assert_eq!(table.stops()[0].value, -5.0 * MPH_TO_MPS);
        assert_eq!(table.sample(0.0), Rgba8::opaque(115, 115, 115));
    }

    #[test]
    fn parses_color4_and_range_folded_rows() {
        let table = ColorTable::parse(
            "RadarScope sample",
            r#"
            product: BR
            units: dBZ
            color4: -15 0 0 0 0
            color: 5 29 37 60
            RF: 82 21 86
            "#,
        )
        .expect("table parses");

        assert_eq!(table.sample(-20.0), Rgba8::TRANSPARENT);
        assert_eq!(table.range_folded_rgba(), Rgba8::new(82, 21, 86, 245));
    }

    #[test]
    fn parses_gr_scale_without_double_scaling_units() {
        let table = ColorTable::parse(
            "Scaled velocity",
            r#"
            product: BV
            scale: 2
            color: 10 10 20 30
            color: 20 30 40 50
            "#,
        )
        .expect("table parses");

        assert_eq!(table.stops()[0].value, 5.0);
        assert_eq!(table.stops()[1].value, 10.0);
    }

    #[test]
    fn stepped_tables_hold_bins_between_thresholds() {
        let table = ColorTable::parse(
            "Stepped velocity",
            r#"
            mode: stepped
            color: 0 0 0 0
            color: 10 255 255 255
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(0, 0, 0));
        assert_eq!(table.sample(10.0), Rgba8::opaque(255, 255, 255));
    }

    #[test]
    fn step_rows_make_pal_style_tables_quantized_ramps() {
        let table = ColorTable::parse(
            "RadarScope sample",
            r#"
            product: BR
            units: dBZ
            step: 5
            color4: -5 0 0 0 0
            color: 5 0 0 100
            color: 15 0 0 200
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample_mode_label(), "quantized stepped");
        assert_eq!(table.step_size(), Some(5.0));
        assert_eq!(table.sample(0.0), Rgba8::TRANSPARENT);
        assert_eq!(table.sample(7.4), Rgba8::opaque(0, 0, 100));
        assert_eq!(table.sample(11.0), Rgba8::opaque(0, 0, 150));
        assert_eq!(table.sample(12.4), Rgba8::opaque(0, 0, 150));
        assert_eq!(table.sample(12.6), Rgba8::opaque(0, 0, 200));
    }

    #[test]
    fn quantized_step_converts_with_velocity_units() {
        let table = ColorTable::parse(
            "Velocity sample",
            r#"
            units: MPH
            step: 10
            color: 0 80 80 80
            color: 20 240 0 0
            "#,
        )
        .expect("table parses");

        let step = table.step_size().expect("numeric step preserved");
        assert!((step - 10.0 * MPH_TO_MPS).abs() < 0.001);
    }

    #[test]
    fn parse_stepped_defaults_to_bins_without_mode_line() {
        let table = ColorTable::parse_stepped(
            "NWS sample",
            r#"
            units: dBZ
            color: 0 0 0 0
            color: 10 255 255 255
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(0, 0, 0));
    }

    #[test]
    fn explicit_interpolated_mode_overrides_stepped_default() {
        let table = ColorTable::parse_stepped(
            "Smooth sample",
            r#"
            mode: interpolated
            color: 0 0 0 0
            color: 10 100 100 100
            "#,
        )
        .expect("table parses");

        assert!(table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(50, 50, 50));
    }

    #[test]
    fn default_reflectivity_preset_filters_low_dbz_and_stretches_high_end() {
        let table = builtin_reflectivity_table();

        assert_eq!(table.name(), "Analyst Reflectivity HD");
        assert!(!table.interpolates());
        assert_eq!(table.sample_mode_label(), "quantized stepped");
        assert_eq!(table.step_size(), Some(1.0));
        // clear-air junk below ~10 dBZ is filtered out
        assert_eq!(table.sample(5.0), Rgba8::TRANSPARENT);
        assert_ne!(table.sample(10.0), Rgba8::TRANSPARENT);
        // The display ladder should use the full REF palette, not collapse
        // 10..15 dBZ into one 5 dBZ bucket.
        assert_ne!(table.sample(10.0), table.sample(11.0));
        // purple/magenta reserved for the 65+ dBZ hail core, not light precip
        for stop in table.stops() {
            let [r, g, b, a] = stop.color.to_array();
            let purple = a > 0 && r > 120 && b > 120 && g < 120;
            assert!(
                !purple || stop.value >= 65.0,
                "purple too early at {} dBZ",
                stop.value
            );
        }
    }

    #[test]
    fn dual_pol_families_have_dedicated_defaults() {
        let set = ColorTableSet::default();
        assert_eq!(
            set.for_family(ColorTableFamily::CorrelationCoefficient)
                .name(),
            "Analyst CC"
        );
        assert_eq!(
            set.for_family(ColorTableFamily::DifferentialReflectivity)
                .name(),
            "Analyst ZDR"
        );
    }

    #[test]
    fn cc_table_resolves_meteorological_range() {
        // The old generic fallback flattened all CC (0.2-1.05) into one dark
        // color; the dedicated table must vary meaningfully across the band
        // where interpretation happens.
        let cc = builtin_correlation_coefficient_table();
        let low = cc.sample(0.70); // non-met / debris
        let mid = cc.sample(0.93); // melting / mixed
        let high = cc.sample(0.998); // uniform precip
        assert_ne!(low, mid);
        assert_ne!(mid, high);
        assert_ne!(low, high);
    }

    #[test]
    fn zdr_table_diverges_about_zero() {
        let zdr = builtin_differential_reflectivity_table();
        let [neg_r, _, neg_b, _] = zdr.sample(-2.0).to_array();
        let [zr, zg, zb, _] = zdr.sample(0.0).to_array();
        let [pos_r, _, pos_b, _] = zdr.sample(3.0).to_array();
        assert!(neg_b > neg_r, "negative ZDR should be cool (blue-dominant)");
        assert!(
            (zr as i16 - zg as i16).abs() <= 12 && (zg as i16 - zb as i16).abs() <= 12,
            "0 dB should be ~neutral gray"
        );
        assert!(pos_r > pos_b, "positive ZDR should be warm (red-dominant)");
    }

    #[test]
    fn builtin_radar_presets_default_to_stepped_sampling() {
        for table in [
            builtin_reflectivity_table(),
            analyst_reflectivity_table(),
            nws_reflectivity_table(),
            builtin_velocity_table(),
            vortex_velocity_table(),
            nws_velocity_table(),
        ] {
            assert!(
                !table.interpolates(),
                "{} should use stepped radar bins",
                table.name()
            );
        }
    }

    #[test]
    fn analyst_velocity_preset_is_stepped_for_gate_readability() {
        let table = analyst_velocity_table();

        assert!(!table.interpolates());
    }

    #[test]
    fn default_velocity_table_is_perceptual_diverging() {
        let table = builtin_velocity_table();

        assert_eq!(table.name(), "Analyst Velocity HD");
        assert!(!table.interpolates());

        // Zero isodop is neutral gray.
        let [zero_r, zero_g, zero_b, zero_a] = table.sample(0.0).to_array();
        assert_eq!(zero_a, 255);
        assert!((zero_r as i16 - zero_g as i16).abs() <= 8);
        assert!((zero_g as i16 - zero_b as i16).abs() <= 8);

        // Inbound is cool (green / blue), outbound is warm (red / orange) at
        // every magnitude so the two are always distinguishable.
        let [in_r, in_g, in_b, _] = table.sample(-20.0).to_array();
        assert!(
            in_g > 150 && in_r < 90,
            "inbound should be green, got {in_r},{in_g},{in_b}"
        );
        let [far_r, _, far_b, _] = table.sample(-50.0).to_array();
        assert!(
            far_b > 200 && far_r < 160,
            "strong inbound should be blue, got {far_r},_,{far_b}"
        );

        let [out_r, out_g, out_b, _] = table.sample(20.0).to_array();
        assert!(
            out_r > 220 && out_b < 90,
            "outbound should be red, got {out_r},{out_g},{out_b}"
        );
    }

    #[test]
    fn display_threshold_clamps_table_sampler_and_signature() {
        let table = builtin_reflectivity_table();
        let clamped = table.with_display_threshold(Some(20.0), false);
        // Below threshold -> transparent; at/above unchanged.
        assert_eq!(clamped.color_for_value(10.0)[3], 0);
        assert_eq!(clamped.color_for_value(35.0), table.color_for_value(35.0));
        // Sampler stays bit-identical to the table.
        let sampler = clamped.sampler();
        for value in [-10.0_f32, 5.0, 19.9, 20.0, 35.0, 60.0] {
            assert_eq!(
                sampler.color_for_value(value),
                clamped.color_for_value(value)
            );
        }
        // Symmetric clamp for diverging products keeps both strong sides.
        let velocity = analyst_hd_velocity_table().with_display_threshold(Some(5.0), true);
        assert_eq!(velocity.color_for_value(2.0)[3], 0);
        assert_eq!(velocity.color_for_value(-2.0)[3], 0);
        assert!(velocity.color_for_value(20.0)[3] > 0);
        assert!(velocity.color_for_value(-20.0)[3] > 0);
        // The clamp participates in the signature (render keys invalidate).
        assert_ne!(table.signature(), clamped.signature());
        assert_ne!(
            clamped.signature(),
            table.with_display_threshold(Some(25.0), false).signature()
        );
    }

    #[test]
    fn balance_velocity_is_cvd_safe_and_lightness_monotonic() {
        let t = balance_velocity_table();
        let lum = |c: [u8; 4]| 0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32;
        let inbound = t.color_for_value(-50.0);
        let zero = t.color_for_value(0.0);
        let outbound = t.color_for_value(50.0);
        // blue↔red axis (CVD-safe), green channel low at the extremes
        assert!(
            inbound[2] > inbound[0] && inbound[1] < 120,
            "inbound should be blue: {inbound:?}"
        );
        assert!(
            outbound[0] > outbound[2] && outbound[1] < 120,
            "outbound should be red: {outbound:?}"
        );
        // light/neutral centre
        assert!(
            zero.iter().take(3).all(|&c| c > 200),
            "zero should be light: {zero:?}"
        );
        // lightness monotonic on each arm: darker toward the extremes
        assert!(lum(t.color_for_value(-70.0)) < lum(t.color_for_value(-30.0)));
        assert!(lum(t.color_for_value(-30.0)) < lum(zero));
        assert!(lum(t.color_for_value(70.0)) < lum(t.color_for_value(30.0)));
        assert!(lum(t.color_for_value(30.0)) < lum(zero));
    }

    #[test]
    fn default_velocity_preset_keeps_strong_cores_saturated() {
        // Regression guard for the old "wash to near-white cream" bug: the
        // operational ±Nyquist range must stay vivid and NOT collapse to a
        // pale, near-white color (which hid derecho RIJ / couplet cores).
        let table = builtin_velocity_table();

        let inbound = table.sample(-20.0).to_array();
        assert!(
            !(inbound[0] > 190 && inbound[1] > 190 && inbound[2] > 190),
            "strong inbound washed out to near-white: {inbound:?}"
        );
        assert!(
            inbound[1] > 150 && inbound[0] < 90,
            "inbound not a saturated green: {inbound:?}"
        );

        let outbound = table.sample(20.0).to_array();
        assert!(
            !(outbound[0] > 200 && outbound[1] > 190 && outbound[2] > 170),
            "strong outbound washed out to cream: {outbound:?}"
        );
        assert!(
            outbound[0] > 220 && outbound[2] < 90,
            "outbound not a saturated red: {outbound:?}"
        );
    }

    #[test]
    fn signatures_change_when_colors_change() {
        let left =
            ColorTable::parse("a", "color: 0 0 0 0\ncolor: 1 255 255 255").expect("table parses");
        let right =
            ColorTable::parse("a", "color: 0 0 0 0\ncolor: 1 255 255 254").expect("table parses");

        assert_ne!(left.signature(), right.signature());
    }

    #[test]
    fn signatures_change_when_gr_interval_end_colors_change() {
        let left =
            ColorTable::parse_gr_pal("a", "color: 0 0 0 0 100 100 100\ncolor: 10 255 255 255")
                .expect("table parses");
        let right =
            ColorTable::parse_gr_pal("a", "color: 0 0 0 0 101 100 100\ncolor: 10 255 255 255")
                .expect("table parses");

        assert_ne!(left.signature(), right.signature());
    }

    #[test]
    fn built_in_presets_offer_multiple_ref_and_velocity_choices() {
        let reflectivity = builtin_tables_for_family(ColorTableFamily::Reflectivity)
            .into_iter()
            .map(|table| table.name().to_owned())
            .collect::<Vec<_>>();
        let velocity = builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .map(|table| table.name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            reflectivity,
            vec![
                "Analyst Reflectivity HD",
                "GR2Analyst Classic REF",
                "Analyst Classic REF",
                "NWS Classic REF",
                "Dark Scope REF",
                "Analyst Hail Core REF",
                "Analyst Low Precip REF",
                "Tornado Debris REF",
                "Clean Light REF",
            ]
        );
        assert_eq!(
            velocity,
            vec![
                "Analyst Velocity HD",
                "Balance VEL (CVD-safe)",
                "Analyst Tornado VEL",
                "Analyst Pro VEL",
                "RadarScope Contrast VEL",
                "Sign Check VEL",
                "Couplet Pop VEL",
                "GR2-ish Analyst VEL",
                "Subtle SRV VEL",
            ]
        );
    }

    #[test]
    fn accepted_reflectivity_presets_filter_junk_and_delay_purple() {
        for table in [
            gr2_reflectivity_table(),
            nws_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
        ] {
            assert_eq!(table.sample_mode_label(), "quantized stepped");
            assert_eq!(table.step_size(), Some(1.0), "{} step size", table.name());
            assert_eq!(table.sample(5.0), Rgba8::TRANSPARENT);
            assert_ne!(
                table.sample(10.0),
                Rgba8::TRANSPARENT,
                "{} should show 10 dBZ and higher",
                table.name()
            );
            assert_ne!(
                table.sample(10.0),
                table.sample(11.0),
                "{} should preserve one-dBZ REF detail",
                table.name()
            );
            for stop in table.stops() {
                let [red, green, blue, alpha] = stop.color.to_array();
                let purple_or_magenta = alpha > 0 && red > 120 && blue > 120 && green < 120;
                assert!(
                    !purple_or_magenta || stop.value >= 65.0,
                    "{} brings purple too early at {:.1} dBZ: {red},{green},{blue}",
                    table.name(),
                    stop.value
                );
            }
        }
    }

    #[test]
    fn accepted_reflectivity_presets_keep_high_dbz_purple() {
        for table in [
            gr2_reflectivity_table(),
            nws_reflectivity_table(),
            analyst_classic_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
        ] {
            assert!(
                table.stops().iter().any(|stop| {
                    let [red, green, blue, alpha] = stop.color.to_array();
                    alpha > 0 && stop.value >= 65.0 && red > 140 && blue > 120 && green < 120
                }),
                "{} should keep a high-dBZ purple/magenta bin",
                table.name()
            );
        }
    }

    #[test]
    fn accepted_velocity_presets_stay_available() {
        for table in [
            builtin_velocity_table(),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
            sign_check_velocity_table(),
        ] {
            assert!(!table.interpolates());
        }
    }

    #[test]
    fn sign_check_velocity_table_exposes_raw_velocity_polarity() {
        let table = sign_check_velocity_table();

        assert_eq!(table.name(), "Sign Check VEL");
        assert_eq!(table.sample_mode_label(), "stepped");
        assert_eq!(table.sample(-1.0), Rgba8::opaque(0, 0, 255));
        assert_eq!(table.sample(0.0), Rgba8::opaque(120, 120, 120));
        assert_eq!(table.sample(1.0), Rgba8::opaque(255, 0, 0));
        assert_eq!(table.range_folded_rgba(), Rgba8::opaque(180, 80, 255));
    }

    #[test]
    fn mirrored_velocity_table_samples_opposite_polarity_colors() {
        let table = sign_check_velocity_table();
        let mirrored = table.mirrored_values("Mirrored Sign Check VEL");

        assert_eq!(mirrored.sample(1.0), table.sample(-1.0));
        assert_eq!(mirrored.sample(-1.0), table.sample(1.0));
        assert_eq!(mirrored.sample(0.0), table.sample(0.0));
        assert_eq!(mirrored.range_folded_rgba(), table.range_folded_rgba());
    }

    #[test]
    fn review_candidate_palettes_are_stepped() {
        for table in [
            analyst_classic_reflectivity_table(),
            tornado_debris_reflectivity_table(),
            clean_light_reflectivity_table(),
            couplet_pop_velocity_table(),
            gr2_ish_analyst_velocity_table(),
            subtle_srv_velocity_table(),
        ] {
            assert!(!table.interpolates(), "{} should be stepped", table.name());
        }
    }
}
