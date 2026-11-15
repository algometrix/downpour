//! Free disk space checks, so a download waits for room instead of failing
//! halfway or filling the disk to the last byte, and the file name rules that
//! keep one download from replacing another's file.

use std::io;
use std::path::{Path, PathBuf};

use crate::record::unfinished_files;

/// Kept free beyond what a download needs: a full disk breaks other programs.
pub const SPACE_RESERVE_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(windows)]
const DISK_FULL_CODES: [i32; 2] = [
    39,  // ERROR_HANDLE_DISK_FULL
    112, // ERROR_DISK_FULL
];
#[cfg(not(windows))]
const DISK_FULL_CODES: [i32; 2] = [
    28,  // ENOSPC
    122, // EDQUOT
];

/// Not enough room in `directory`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not enough disk space in {}: {} needed, {available} bytes free", .directory.display(), describe_needed(*.needed))]
pub struct NoSpace {
    pub directory: PathBuf,
    /// Bytes the download still has to write, when known.
    pub needed: Option<u64>,
    pub available: u64,
}

fn describe_needed(needed: Option<u64>) -> String {
    match needed {
        Some(bytes) => format!("{bytes} bytes"),
        None => "more space".to_owned(),
    }
}

/// Free bytes on the volume holding `path`, which may not exist yet.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let existing = path
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .unwrap_or(path);
    fs4::available_space(existing)
}

/// Fails with [`NoSpace`] unless `needed` bytes plus the reserve fit.
pub fn ensure_space(directory: &Path, needed: u64) -> Result<(), NoSpaceCheck> {
    let available = free_space(directory).map_err(NoSpaceCheck::Unknown)?;
    if available >= needed.saturating_add(SPACE_RESERVE_BYTES) {
        return Ok(());
    }
    Err(NoSpaceCheck::Short(NoSpace {
        directory: directory.to_path_buf(),
        needed: Some(needed),
        available,
    }))
}

#[derive(Debug, thiserror::Error)]
pub enum NoSpaceCheck {
    #[error(transparent)]
    Short(NoSpace),
    #[error("cannot read free disk space: {0}")]
    Unknown(io::Error),
}

/// Whether a write failed because the volume is full.
pub fn is_disk_full(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::StorageFull
        || error.kind() == io::ErrorKind::QuotaExceeded
        || error
            .raw_os_error()
            .is_some_and(|code| DISK_FULL_CODES.contains(&code))
}

/// Whether a download waiting for room in `directory` can continue.
pub fn has_room_for(directory: &Path, needed: Option<u64>) -> bool {
    match free_space(directory) {
        Ok(available) => available >= needed.unwrap_or(0).saturating_add(SPACE_RESERVE_BYTES),
        Err(error) => {
            tracing::warn!(directory = %directory.display(), %error, "cannot read free disk space");
            false
        }
    }
}

/// Whether two existing paths name one file, as on a disk that ignores case.
pub fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether a download may not save its file as `candidate`: a file is there,
/// or another download reserved the name with its part or track files. The
/// files of the download whose name is `own` do not count, so on a disk that
/// ignores case a download can change just the case of its own name.
pub fn is_name_taken(candidate: &Path, own: Option<&Path>) -> bool {
    let file_there = candidate.exists() && !own.is_some_and(|own| is_same_file(candidate, own));
    let own_files = own.map(unfinished_files);
    let reserved = unfinished_files(candidate)
        .iter()
        .enumerate()
        .any(|(index, file)| {
            let is_own = own_files
                .as_ref()
                .is_some_and(|own| is_same_file(file, &own[index]));
            file.exists() && !is_own
        });
    file_there || reserved
}

/// Renames `from` to `to`, failing with `AlreadyExists` instead of replacing
/// a file already at `to`. `std::fs::rename` replaces it silently, and a check
/// beforehand leaves a gap in which another download can finish there.
pub fn rename_without_replacing(from: &Path, to: &Path) -> io::Result<()> {
    rename_without_replacing_native(from, to)
}

#[cfg(windows)]
fn rename_without_replacing_native(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let wide = |path: &Path| -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (from_wide, to_wide) = (wide(from), wide(to));
    // No MOVEFILE_REPLACE_EXISTING: a file at `to` fails the call instead.
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let moved = unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), 0) };
    if moved != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    let already_there = [ERROR_FILE_EXISTS, ERROR_ALREADY_EXISTS]
        .iter()
        .any(|&code| error.raw_os_error() == Some(code as i32));
    if already_there {
        Err(io::Error::new(io::ErrorKind::AlreadyExists, error))
    } else {
        Err(error)
    }
}

/// A hard link is made only where nothing is, so it cannot replace a file;
/// the old name is dropped once the new one exists.
#[cfg(not(windows))]
fn rename_without_replacing_native(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::hard_link(from, to)?;
    std::fs::remove_file(from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rename_never_replaces_a_file_already_there() {
        let dir = std::env::temp_dir().join(format!("downpour-no-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (from, to) = (dir.join("from.bin"), dir.join("to.bin"));
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"keep").unwrap();

        let refused = rename_without_replacing(&from, &to).unwrap_err();

        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&to).unwrap(), b"keep");
        std::fs::remove_file(&to).unwrap();
        rename_without_replacing(&from, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recognizes_disk_full_errors() {
        assert!(is_disk_full(&io::Error::from(io::ErrorKind::StorageFull)));
        assert!(is_disk_full(&io::Error::from_raw_os_error(
            DISK_FULL_CODES[1]
        )));
        assert!(!is_disk_full(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn free_space_works_for_folders_that_do_not_exist_yet() {
        let missing = std::env::temp_dir()
            .join("downpour-no-such-folder")
            .join("deeper");
        assert!(free_space(&missing).unwrap() > 0);
    }

    #[test]
    fn impossible_requirement_reports_no_space() {
        let result = ensure_space(&std::env::temp_dir(), u64::MAX / 2);
        assert!(matches!(
            result,
            Err(NoSpaceCheck::Short(NoSpace {
                needed: Some(_),
                ..
            }))
        ));
    }
}
