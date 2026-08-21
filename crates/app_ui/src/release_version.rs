use semver::Version;

/// Some(trimmed tag) iff the release tag is strictly newer than the current
/// version under SemVer precedence; None on equal, older, or invalid input.
pub(crate) fn newer_release_tag(tag_name: &str, current_version: &str) -> Option<String> {
    let remote = parse_release_version(tag_name)?;
    let current = parse_release_version(current_version)?;
    remote
        .cmp_precedence(&current)
        .is_gt()
        .then(|| tag_name.trim().to_owned())
}

/// Parse an official release version with full SemVer precedence. A leading
/// release-tag `v`/`V` is accepted, as are the historical shortened spellings
/// `v2` and `v0.9` (normalized to `2.0.0` and `0.9.0`). Prerelease identifiers
/// are retained and build metadata is parsed but ignored by
/// [`Version::cmp_precedence`], exactly as SemVer requires.
fn parse_release_version(version: &str) -> Option<Version> {
    let trimmed = version.trim();
    let unprefixed = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    if unprefixed.is_empty() {
        return None;
    }

    let suffix_at = unprefixed.find(['-', '+']).unwrap_or(unprefixed.len());
    let (core, suffix) = unprefixed.split_at(suffix_at);
    let mut core_parts = core.split('.').collect::<Vec<_>>();
    if core_parts.is_empty()
        || core_parts.len() > 3
        || core_parts.iter().any(|part| part.is_empty())
    {
        return None;
    }
    while core_parts.len() < 3 {
        core_parts.push("0");
    }

    let normalized = format!("{}{}", core_parts.join("."), suffix);
    Version::parse(&normalized).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_release_version_preserves_semver_and_historical_short_tags() {
        let parsed = |value: &str| parse_release_version(value).map(|version| version.to_string());
        assert_eq!(parsed("v0.8.2").as_deref(), Some("0.8.2"));
        assert_eq!(parsed("V1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parsed("0.8.2").as_deref(), Some("0.8.2"));
        assert_eq!(parsed(" v0.9 ").as_deref(), Some("0.9.0"));
        assert_eq!(parsed("v2").as_deref(), Some("2.0.0"));
        assert_eq!(parsed("v0.34.14-rc.1").as_deref(), Some("0.34.14-rc.1"));
        assert_eq!(
            parsed("v0.34.14-rc.1+ci.7").as_deref(),
            Some("0.34.14-rc.1+ci.7")
        );
        assert_eq!(
            parsed("v0.34.14+build.5").as_deref(),
            Some("0.34.14+build.5")
        );

        for invalid in [
            "",
            "v",
            "latest",
            "v0.8.2.1",
            "v0..2",
            "v00.8.2",
            "v0.8.2-01",
            "v0.8.2-rc_1",
            "vv0.8.2",
        ] {
            assert_eq!(parse_release_version(invalid), None, "accepted {invalid:?}");
        }
    }

    #[test]
    fn newer_release_tag_uses_full_semver_precedence() {
        let some = |tag: &str| Some(tag.to_owned());
        assert_eq!(newer_release_tag("v0.9.0", "0.8.2"), some("v0.9.0"));
        // Numeric compare, not lexicographic: "10" > "2".
        assert_eq!(newer_release_tag("v0.8.10", "0.8.2"), some("v0.8.10"));
        assert_eq!(newer_release_tag("v1.0.0", "0.9.9"), some("v1.0.0"));

        // The RC-to-final transition was the release blocker: final must be
        // offered even though its numeric core equals the running RC.
        assert_eq!(
            newer_release_tag("v0.34.14", "0.34.14-rc.1"),
            some("v0.34.14")
        );
        assert_eq!(
            newer_release_tag("v0.34.14-rc.2", "0.34.14-rc.1"),
            some("v0.34.14-rc.2")
        );
        assert_eq!(
            newer_release_tag("v0.34.14-rc.10", "0.34.14-rc.2"),
            some("v0.34.14-rc.10")
        );
        assert_eq!(
            newer_release_tag("v0.34.14-rc.beta", "0.34.14-rc.2"),
            some("v0.34.14-rc.beta")
        );
        assert_eq!(
            newer_release_tag(" v0.34.14-rc.2 ", "0.34.14-rc.1"),
            some("v0.34.14-rc.2")
        );

        // Same precedence (including build-only differences), older remote,
        // a prerelease behind the current final, and junk all stay silent.
        assert_eq!(newer_release_tag("v0.8.2", "0.8.2"), None);
        assert_eq!(newer_release_tag("v0.8.1", "0.8.2"), None);
        assert_eq!(newer_release_tag("v0.34.14-rc.1", "0.34.14-rc.2"), None);
        assert_eq!(newer_release_tag("v0.34.14-rc.2", "0.34.14"), None);
        assert_eq!(
            newer_release_tag("v0.34.14+build.9", "0.34.14+build.1"),
            None
        );
        assert_eq!(newer_release_tag("latest", "0.8.2"), None);
    }
}
