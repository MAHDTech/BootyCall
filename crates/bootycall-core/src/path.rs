//! Path resolution helpers shared across servers.
//!
//! `safe_join` is the single source of truth for turning a client-supplied
//! path (from TFTP RRQ, HTTP URL, iPXE menu, etc.) into a filesystem path
//! rooted under a configured directory. It refuses `..` and absolute
//! inputs, and verifies via canonicalisation that the resolved path is
//! contained under the root *at the time of the check*.
//!
//! # Guarantees and caveats
//!
//! - **Containment is check-time only.** `safe_join` validates and then
//!   returns a path which the caller opens later. A symlink swapped into an
//!   intermediate directory between the check and the open can still escape
//!   the root — a classic time-of-check to time-of-use race. Exploiting it
//!   requires local write access to the served tree. Callers that need to
//!   close this window must open the file relative to an already-validated
//!   directory descriptor without following symlinks (dir-fd-relative opens
//!   with `O_NOFOLLOW`) instead of re-resolving the returned path.
//! - **Drive letters are not special on Unix.** Backslashes are normalised
//!   to `/`, and `C:` then parses as an ordinary path segment, so an input
//!   such as `C:\boot\x` resolves to `root/C:/boot/x` — contained, never an
//!   escape. Only on Windows do drive-letter inputs parse as
//!   `Component::Prefix` and get rejected outright.
//!
//! Callers map `None` to whatever error their protocol expects (TFTP error
//! packet, HTTP 403/404, etc.); this module never panics on hostile input.
use std::path::{Component, Path, PathBuf};

/// Join `requested` onto `root` with hardened checks.
///
/// Returns `Some(path)` iff `requested`:
///
/// - contains no `..` segments,
/// - is not absolute (`/etc/passwd`, `//etc/passwd`),
/// - lexically stays inside `root`,
/// - and resolves under the canonical `root` at check time: an existing
///   candidate is canonicalised in full; a missing one is canonicalised up
///   to its nearest existing ancestor (blocking symlink escape via
///   pre-planted links, including a symlinked parent of a missing leaf).
///
/// Both branches return the same path form: the canonicalised existing
/// prefix with any missing tail segments appended lexically. A missing
/// candidate is therefore still returned; callers surface that as their
/// protocol's "not found" instead of an error, matching current behaviour.
///
/// Containment is guaranteed only at check time — see the module docs for
/// the return-then-open race and for drive-letter handling on Unix.
///
/// # Security / TOCTOU
///
/// This function is subject to a Time-of-Check to Time-of-Use (TOCTOU) race window.
/// If a local attacker can modify the directories in the path after this validation
/// but before the file is opened, they could swap in a symlink pointing outside the
/// root. Ensure that the directory tree being served cannot be written to by untrusted
/// users, or open the file using directory-relative descriptor-based APIs (`O_NOFOLLOW`).
pub fn safe_join(root: &Path, requested: &str) -> Option<PathBuf> {
    let normalized = requested.replace('\\', "/");
    let requested_path = Path::new(&normalized);

    // Reject anything that would break out of the root: `..`, absolute
    // paths, or (on Windows only) drive-letter prefixes. This closes the
    // absolute-path bypass — the old resolvers only filtered `ParentDir`.
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
    // the canonical root. This blocks symlink escape at check time.
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
    // Return the same path form as the existing-candidate branch: the
    // canonical ancestor plus the (missing, purely lexical) tail.
    // `strip_prefix` cannot fail — `existing_ancestor` was produced by
    // `candidate.ancestors()` — but propagate `None` rather than panic.
    let missing_tail = candidate.strip_prefix(existing_ancestor).ok()?;
    Some(canonical_ancestor.join(missing_tail))
}

/// Open a file at `requested` relative to `root` with hardened checks to prevent directory traversal and symlink escape.
///
/// On Unix, this resolves the path first using `safe_join`, and then traverses and opens each component of the relative path
/// using `openat` with `O_NOFOLLOW` to ensure that no intermediate directories or the target file itself are symlinks.
/// On non-Unix platforms, this delegates to `safe_join` followed by standard `std::fs::File::open`.
pub fn safe_open(root: &Path, requested: &str) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use nix::fcntl::{OFlag, open, openat};
        use nix::sys::stat::Mode;

        // 1. Resolve path using safe_join
        let resolved = safe_join(root, requested).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Access violation (path resolution failed)",
            )
        })?;

        // 2. Canonicalise the root directory to open it securely.
        let canonical_root = root.canonicalize()?;

        // 3. Strip prefix to get the relative path to traverse
        let rel_path = resolved.strip_prefix(&canonical_root).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Access violation (path not under root)",
            )
        })?;

        // 4. Open the canonical root first to get a directory descriptor
        let mut current_fd = open(
            &canonical_root,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;

        // 5. Traverse each component of the relative path
        let components: Vec<_> = rel_path.components().collect();

        for (i, component) in components.iter().enumerate() {
            let name = match component {
                std::path::Component::Normal(s) => s.to_string_lossy(),
                std::path::Component::CurDir => continue,
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "Access violation (invalid component in path)",
                    ));
                }
            };

            let is_last = i == components.len() - 1;

            let flags = if is_last {
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
            } else {
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC
            };

            current_fd = openat(&current_fd, name.as_ref(), flags, Mode::empty())
                .map_err(std::io::Error::from)?;
        }

        Ok(std::fs::File::from(current_fd))
    }

    #[cfg(not(unix))]
    {
        let resolved = safe_join(root, requested).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Access violation (path resolution failed)",
            )
        })?;
        std::fs::File::open(resolved)
    }
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
    fn existing_candidate_returns_canonical_path() {
        let dir = root();
        fs::write(dir.path().join("file.bin"), b"stub").unwrap();

        let canonical_root = dir.path().canonicalize().unwrap();
        let joined = safe_join(dir.path(), "file.bin").expect("some");
        assert_eq!(joined, canonical_root.join("file.bin"));
    }

    #[test]
    fn missing_candidate_returns_same_canonical_form() {
        // Both branches must yield the same path form: the canonicalised
        // existing prefix (here, the root) plus the missing lexical tail.
        let dir = root();

        let canonical_root = dir.path().canonicalize().unwrap();
        let joined = safe_join(dir.path(), "not-yet/there.bin").expect("some");
        assert_eq!(joined, canonical_root.join("not-yet/there.bin"));
    }

    #[test]
    #[cfg(unix)]
    fn missing_leaf_under_internal_symlink_resolves_to_real_dir() {
        use std::os::unix::fs::symlink;

        let dir = root();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        // `alias` -> `real`, both inside the root: allowed, and the
        // returned path is expressed via the canonical (real) directory.
        symlink(&real, dir.path().join("alias")).unwrap();

        let canonical_root = dir.path().canonicalize().unwrap();
        let joined = safe_join(dir.path(), "alias/missing.bin").expect("some");
        assert_eq!(joined, canonical_root.join("real/missing.bin"));
    }

    #[test]
    #[cfg(unix)]
    fn treats_drive_letter_as_normal_segment_on_unix() {
        // On Unix `C:` is an ordinary file name, not a path prefix: the
        // input is normalised and contained under the root, not rejected.
        let dir = root();

        let canonical_root = dir.path().canonicalize().unwrap();
        let joined = safe_join(dir.path(), "C:\\boot\\file.bin").expect("some");
        assert_eq!(joined, canonical_root.join("C:/boot/file.bin"));
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

    #[test]
    #[cfg(unix)]
    fn safe_open_rejects_symlink_that_escapes_root() {
        use std::os::unix::fs::symlink;

        let dir = root();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret"), b"leaked").unwrap();

        // Plant a symlink under the root that points outside the root.
        symlink(outside.path().join("secret"), dir.path().join("escape")).unwrap();

        assert!(safe_open(dir.path(), "escape").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn safe_open_rejects_parent_symlink() {
        use std::os::unix::fs::symlink;

        let dir = root();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret"), b"leaked").unwrap();

        // Plant a symlink to outside directory.
        symlink(outside.path(), dir.path().join("link")).unwrap();

        assert!(safe_open(dir.path(), "link/secret").is_err());
    }

    #[test]
    fn safe_open_accepts_normal_file() {
        let dir = root();
        fs::write(dir.path().join("test.txt"), b"hello").unwrap();
        let mut file = safe_open(dir.path(), "test.txt").expect("should open");

        use std::io::Read;
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "hello");
    }
}
