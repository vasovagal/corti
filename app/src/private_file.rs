//! Crash-safe persistence for small app-owned private configuration documents.
//!
//! Callers serialize before entering this boundary. The helper writes a unique sibling with mode 0600,
//! syncs it, atomically renames it over the destination, and syncs the parent directory. Error messages
//! contain paths and fixed labels only—never document bytes.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};

const PRIVATE_FILE_MODE: u32 = 0o600;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn read_private(path: &Path, label: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
    read_private_inner(path, label, max_bytes, false)
}

/// Read a legacy app-owned document and atomically tighten an old read-only-for-others mode (for example
/// 0644) to 0600 through the already-open descriptor. Symlinks, foreign owners, executable files, and any
/// group/other-writable mode fail closed and are never migrated.
pub(crate) fn read_private_migrating_mode(
    path: &Path,
    label: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    read_private_inner(path, label, max_bytes, true)
}

fn read_private_inner(
    path: &Path,
    label: &str,
    max_bytes: usize,
    migrate_legacy_mode: bool,
) -> Result<Option<Vec<u8>>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("opening {label} {}", path.display()));
        }
    };
    let mut metadata = file
        .metadata()
        .with_context(|| format!("inspecting {label} {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "refusing non-file {label} {}",
        path.display()
    );
    let mode = metadata.permissions().mode() & 0o777;
    if mode != PRIVATE_FILE_MODE {
        let owner = metadata.uid();
        // SAFETY: geteuid has no preconditions and does not expose mutable process state.
        let current_user = unsafe { libc::geteuid() };
        ensure!(
            migrate_legacy_mode
                && owner == current_user
                && mode & 0o022 == 0
                && mode & 0o111 == 0
                && mode & 0o600 == 0o600,
            "refusing {label} {} with permissions {mode:04o}; expected 0600",
            path.display()
        );
        file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
            .with_context(|| format!("migrating private mode for {label} {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing migrated {label} {}", path.display()))?;
        metadata = file
            .metadata()
            .with_context(|| format!("re-inspecting {label} {}", path.display()))?;
        ensure!(
            metadata.permissions().mode() & 0o777 == PRIVATE_FILE_MODE,
            "failed to secure {label} {}",
            path.display()
        );
    }
    ensure!(
        metadata.len() <= max_bytes as u64,
        "refusing oversized {label} {}",
        path.display()
    );

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::take(&mut file, max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    ensure!(
        bytes.len() <= max_bytes,
        "refusing oversized {label} {}",
        path.display()
    );
    Ok(Some(bytes))
}

pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {label} directory {}", parent.display()))?;

    let (temporary, mut file) = create_temporary(path, label)?;
    let mut cleanup = TemporaryFile::new(temporary.clone());
    // `mode` is filtered by the process umask. Set the exact owner-only contract before writing bytes.
    file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
        .with_context(|| format!("securing temporary {label} {}", temporary.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing temporary {label} {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing temporary {label} {}", temporary.display()))?;
    drop(file);

    std::fs::rename(&temporary, path).with_context(|| {
        format!(
            "publishing {label} {} -> {}",
            temporary.display(),
            path.display()
        )
    })?;
    cleanup.disarm();

    File::open(parent)
        .with_context(|| format!("opening {label} directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("syncing {label} directory {}", parent.display()))?;
    Ok(())
}

fn create_temporary(path: &Path, label: &str) -> Result<(PathBuf, File)> {
    for _ in 0..1_024 {
        let temporary = unique_temporary_path(path);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("creating temporary {label} {}", temporary.display())
                });
            }
        }
    }
    bail!(
        "could not allocate a unique temporary {label} beside {}",
        path.display()
    )
}

fn unique_temporary_path(path: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    PathBuf::from(name)
}

struct TemporaryFile {
    path: Option<PathBuf>,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("corti-private-file-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("document")
    }

    #[test]
    fn replacement_is_exactly_mode_0600_and_leaves_no_temporary_file() {
        let path = test_path("mode");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        atomic_write_private(&path, b"replacement", "test document").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            PRIVATE_FILE_MODE
        );
        let names = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![path.file_name().unwrap()]);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn reader_fails_closed_on_overly_broad_permissions() {
        let path = test_path("read-mode");
        std::fs::write(&path, b"private").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        let error = read_private(&path, "test document", 1024)
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected 0600"), "{error}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn legacy_mode_migration_is_descriptor_scoped_and_symlinks_fail_closed() {
        let path = test_path("migration");
        std::fs::write(&path, b"legacy private bytes").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_private_migrating_mode(&path, "legacy document", 1024)
                .unwrap()
                .unwrap(),
            b"legacy private bytes"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            PRIVATE_FILE_MODE
        );

        let link = path.with_file_name("document-link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_private_migrating_mode(&link, "legacy document", 1024).is_err());
        assert!(read_private(&link, "legacy document", 1024).is_err());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
