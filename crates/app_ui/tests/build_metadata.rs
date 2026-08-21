#[path = "../build_metadata.rs"]
mod build_metadata;

#[test]
fn windows_prerelease_flag_follows_cargo_semver_component() {
    assert!(!build_metadata::cargo_version_is_prerelease(""));
    assert!(!build_metadata::cargo_version_is_prerelease("   "));
    assert!(build_metadata::cargo_version_is_prerelease("rc.1"));
    assert!(build_metadata::cargo_version_is_prerelease("alpha.2"));
    assert!(build_metadata::cargo_version_is_prerelease("beta"));
}
