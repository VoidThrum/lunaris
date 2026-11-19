//! Version comparison: semver, a leading `v` allowed (release tags are `vX.Y.Z`).

/// `v0.6.4` / `0.6.4` → 0.6.4; None when it is not a version.
pub fn parse(tag: &str) -> Option<semver::Version> {
    let t = tag.trim();
    let t = t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t);
    semver::Version::parse(t).ok()
}

/// Whether the release `candidate` is an update for `current`: a stable
/// version (no `-pre` part) strictly newer. Never a downgrade, and nothing
/// when either side cannot be read.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse(current), parse(candidate)) {
        (Some(c), Some(n)) => n.pre.is_empty() && n > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_only_when_strictly_greater() {
        assert!(is_newer("0.6.3", "v0.6.4"));
        assert!(is_newer("0.6.3", "0.7.0"));
        assert!(is_newer("0.6.3", "v1.0.0"));
        assert!(is_newer("0.6.9", "0.6.10"));
        assert!(!is_newer("0.6.3", "v0.6.3"));
        assert!(!is_newer("0.6.3", "v0.6.2"), "never a downgrade");
        assert!(!is_newer("0.10.0", "0.9.9"));
    }

    #[test]
    fn prereleases_and_garbage_are_not_offered() {
        assert!(!is_newer("0.6.3", "v0.7.0-rc.1"));
        assert!(!is_newer("0.6.3", "latest"));
        assert!(!is_newer("0.6.3", ""));
        assert!(!is_newer("garbage", "v0.7.0"));
        // A prerelease build is offered the stable release of the same version.
        assert!(is_newer("0.7.0-dev.3", "v0.7.0"));
    }
}
