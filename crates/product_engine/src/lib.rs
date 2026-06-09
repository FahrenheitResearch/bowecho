//! Base and derived product registry scaffolding.

use radar_core::{MomentType, ProductId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductDescriptor {
    pub id: ProductId,
    pub display_name: &'static str,
    pub source: ProductSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductSource {
    BaseMoment(MomentType),
    Derived,
}

pub fn base_products() -> Vec<ProductDescriptor> {
    [
        (MomentType::Reflectivity, "Base Reflectivity"),
        (MomentType::Velocity, "Base Velocity"),
        (MomentType::SpectrumWidth, "Spectrum Width"),
        (
            MomentType::DifferentialReflectivity,
            "Differential Reflectivity",
        ),
        (
            MomentType::CorrelationCoefficient,
            "Correlation Coefficient",
        ),
        (MomentType::DifferentialPhase, "Differential Phase"),
    ]
    .into_iter()
    .map(|(moment, display_name)| ProductDescriptor {
        id: ProductId::from(moment.clone()),
        display_name,
        source: ProductSource::BaseMoment(moment),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_includes_reflectivity() {
        assert!(base_products().iter().any(|product| product.id.0 == "REF"));
    }
}
