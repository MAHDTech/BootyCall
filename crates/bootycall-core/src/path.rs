//! Path resolution helpers shared across servers.
//!
//! `safe_join` is the single source of truth for turning a client-supplied
//! path (from TFTP RRQ, HTTP URL, iPXE menu, etc.) into a filesystem path
//! rooted under a configured directory. It refuses `..`, absolute inputs,
//! and Windows-style path prefixes, and — where the resolved path exists —
//! verifies via canonicalisation that a symlink cannot escape the root.
//!
//! Callers map `None` to whatever error their protocol expects (TFTP error
//! packet, HTTP 403/404, etc.); this module never panics on hostile input.
use std::path::{Component, Path, PathBuf};

/// Join `requested` onto `root` with hardened checks.
///
/// Returns `Some(root/relative)` iff `requested`:
///
/// - contains no `..` segments,
/// - is not absolute (`/etc/passwd`, `//etc/passwd`, `C:\…`),
/// - lexically stays inside `root`,
/// - and — if the candidate already exists — resolves under the canonical
///   `root` (blocks symlink-escape attacks against pre-planted links).
///
/// A missing candidate is still returned; callers surface that as their
/// protocol's "not found" instead of an error, matching current behaviour.
pub fn safe_join(root: &Path, requested: &str) -> Option<PathBuf> {
    let normalized = requested.replace('\\', "/");
    let requested_path = Path::new(&normalized);

    // Reject anything that would break out of the root: `..`, absolute
    // paths, or Windows prefixes. This closes the current absolute-path
    // bypass — the old resolvers only filtered `ParentDir`.
    for component in requested_path.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    let candidate = root.join(requested_path);

    // Canonicalise the root once. If the root itself can't be canonicalised
    // (misconfigured server), refuse — better to 404 than to serve arbitrary
    // files.
    let canonical_root = root.canonicalize().ok()?;

    // If the candidate exists, canonicalise it and confirm it lives under
    // the canonical root. This blocks symlink escape.
    if let Ok(canonical_candidate) = candidate.canonicalize() {
        if !canonical_candidate.starts_with(&canonical_root) {
            return None;
        }
        return Some(canonical_candidate);
    }

    // Non-existent candidate (the normal 404 case). A purely lexical check
    // would miss a symlinked *parent* directory with a not-yet-existing leaf
    // (`link/newfile` where `link` -> outside the root): the leaf can't be
    // canonicalised, so the escape slips through. Canonicalise the nearest
    // existing ancestor and confirm it still lives under the canonical root.
    // The component filter already blocks `..`/absolute, so the remaining
    // (non-existent) tail is plain `Normal` segments under that ancestor.
    let existing_ancestor = candidate.ancestors().find(|a| a.exists())?;
    let canonical_ancestor = existing_ancestor.canonicalize().ok()?;
    if !canonical_ancestor.starts_with(&canonical_root) {
        return None;
    }
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn root() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    #[test]
    fn rejects_parent_dir() {
        let dir = root();
        assert!(safe_join(dir.path(), "../../etc/passwd").is_none());
    }

    #[test]
    fn rejects_absolute_unix_path() {
        let dir = root();
        assert!(safe_join(dir.path(), "/etc/passwd").is_none());
    }

    #[test]
    fn rejects_double_slash_absolute() {
        let dir = root();
        assert!(safe_join(dir.path(), "//etc/passwd").is_none());
    }

    #[test]
    fn rejects_windows_backslash_parent() {
        let dir = root();
        assert!(safe_join(dir.path(), "\\\\..\\\\..\\\\etc\\\\passwd").is_none());
    }

    #[test]
    fn accepts_normal_relative_path() {
        let dir = root();
        let nested = dir.path().join("boot").join("x64");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("ipxe.efi"), b"stub").unwrap();

        let joined = safe_join(dir.path(), "boot/x64/ipxe.efi").expect("some");
        assert!(joined.ends_with("boot/x64/ipxe.efi"));
    }

    #[test]
    fn accepts_missing_relative_path() {
        // 404 case: file doesn't exist yet; caller decides what to do.
        let dir = root();
        assert!(safe_join(dir.path(), "not-yet/there.bin").is_some());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlink_that_escapes_root() {
        use std::os::unix::fs::symlink;

        let dir = root();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret"), b"leaked").unwrap();

        // Plant a symlink under the root that points outside the root.
        symlink(outside.path().join("secret"), dir.path().join("escape")).unwrap();

        assert!(safe_join(dir.path(), "escape").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlinked_parent_with_missing_leaf() {
        use std::os::unix::fs::symlink;

        let dir = root();
        let outside = TempDir::new().unwrap();

        // `link` -> an existing directory outside the root. The requested
        // leaf does not exist yet, so it can't be canonicalised — the missing
        // -leaf branch must still reject it via the symlinked parent.
        symlink(outside.path(), dir.path().join("link")).unwrap();

        assert!(safe_join(dir.path(), "link/newfile").is_none());
    }
}
