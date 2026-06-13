//! Native app-shell state shared by future winit/wgpu UI code.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PanelLayout {
    #[default]
    One,
    TwoVertical,
    ThreeStacked,
    FourGrid,
}

impl PanelLayout {
    pub fn panel_count(self) -> usize {
        match self {
            Self::One => 1,
            Self::TwoVertical => 2,
            Self::ThreeStacked => 3,
            Self::FourGrid => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_grid_has_four_panels() {
        assert_eq!(PanelLayout::One.panel_count(), 1);
        assert_eq!(PanelLayout::TwoVertical.panel_count(), 2);
        assert_eq!(PanelLayout::ThreeStacked.panel_count(), 3);
        assert_eq!(PanelLayout::FourGrid.panel_count(), 4);
    }
}
