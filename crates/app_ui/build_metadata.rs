/// Whether Cargo's `CARGO_PKG_VERSION_PRE` component describes a prerelease.
///
/// Cargo supplies the empty string for a stable package and the exact SemVer
/// prerelease component (for example `rc.1`) otherwise. Keeping this pure lets
/// the Windows resource behavior be tested without invoking `rc.exe`.
pub(crate) fn cargo_version_is_prerelease(prerelease: &str) -> bool {
    !prerelease.trim().is_empty()
}
