//! Owner-only directories and files for data the app keeps on disk for a while (query result
//! spill files, data compare rows).
//!
//! Other local users must not be able to read that data, and nothing another user planted (a
//! directory, a symbolic link) may be reused or followed. On Unix, new directories get mode
//! `0o700` and new files `0o600`, and an existing directory is used only when it is a real
//! directory owned by the effective user with no group or other permission bits. On other
//! platforms the per-user temporary and cache directories already keep other users out, so the
//! functions only create (and refuse anything that is not a plain directory).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

/// Creates `dir` and its missing parents (each new one `0o700` on Unix), then checks that `dir`
/// is private ([`check_private_dir`]).
pub fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)?;
    check_private_dir(dir)
}

/// Fails unless `dir` is a directory itself, not a symbolic link to one. On Unix it must also
/// belong to the effective user and grant nothing to the group or to others.
pub fn check_private_dir(dir: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(not_private(dir, "is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != rustix::process::geteuid().as_raw() {
            return Err(not_private(dir, "belongs to another user"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(not_private(dir, "is open to other users"));
        }
    }
    Ok(())
}

/// Owner-only directory permissions (`0o700`), e.g. for `tempfile::Builder::permissions`.
#[cfg(unix)]
pub fn private_dir_permissions() -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(0o700)
}

/// Creates a new file at `path` that only the current user can read and write (`0o600` on
/// Unix), open for writing. Fails when anything already exists at `path`, a symbolic link
/// included, so it never follows or truncates someone else's file.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn not_private(dir: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} {why}; refusing to keep private data there",
            dir.display()
        ),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_directories_and_files_are_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("b");
        create_private_dir_all(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&tmp.path().join("a")), 0o700);
        // An existing private directory is accepted again.
        create_private_dir_all(&dir).unwrap();

        let file = dir.join("f");
        create_private_file(&file).unwrap().write_all(b"x").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(
            create_private_file(&file).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn open_or_linked_directories_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let open = tmp.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let err = create_private_dir_all(&open).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        let target = tmp.path().join("target");
        create_private_dir_all(&target).unwrap();
        let link = tmp.path().join("link");
        symlink(&target, &link).unwrap();
        let err = create_private_dir_all(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(check_private_dir(&target).is_ok());
    }

    #[test]
    fn a_planted_link_is_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        let link = tmp.path().join("link");
        symlink(&victim, &link).unwrap();
        assert_eq!(
            create_private_file(&link).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        // A dangling link is not followed either.
        let dangling = tmp.path().join("dangling");
        symlink(tmp.path().join("missing"), &dangling).unwrap();
        assert!(create_private_file(&dangling).is_err());
        assert!(!tmp.path().join("missing").exists());
    }
}
