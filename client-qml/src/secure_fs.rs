//! Files that hold secrets (login token, agent token, server log): private to
//! the user from the moment they exist. On Windows the profile ACL already
//! does this.

use std::io::Write;
use std::path::Path;

/// Create `dir` (and parents) and make it 0700 on unix.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict_dir(dir);
    Ok(())
}

/// Make an existing directory 0700 (unix); best effort.
pub fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Make an existing file 0600 (unix); best effort.
pub fn restrict_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Open (create/truncate) a file that is 0600 from the start.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let f = o.open(path)?;
    // A file that already existed keeps its old mode.
    restrict_file(path);
    Ok(f)
}

/// Replace `path` with `data`: written to a 0600 temp file in the same
/// directory, then renamed, so the content is never readable by others and a
/// crash never leaves half a file.
pub fn write_private(path: &Path, data: impl AsRef<[u8]>) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = dir {
        create_private_dir(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    // The counter keeps two saves of one file in this process apart.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{}.{}.{}.tmp", name, std::process::id(), n));
    let result = (|| {
        let mut f = create_private_file(&tmp)?;
        f.write_all(data.as_ref())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn write_private_is_0600_in_0700_dir() {
        let base = std::env::temp_dir().join(format!("lunaris-secfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let file = base.join("sub").join("cfg.json");
        write_private(&file, b"{}").unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(file.parent().unwrap()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Overwriting a loose file tightens it.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&file, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"{\"a\":1}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
