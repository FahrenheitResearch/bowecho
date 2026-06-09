//! Cache policy primitives for downloaded files and decoded volumes.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub files_on_disk: usize,
    pub volumes_in_memory: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            files_on_disk: 24,
            volumes_in_memory: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keeps_multiple_volumes() {
        assert!(RetentionPolicy::default().volumes_in_memory > 1);
    }
}
