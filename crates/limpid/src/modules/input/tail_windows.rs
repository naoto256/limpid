//! File identity for Windows tail rotation detection. Keep the volume and full
//! 128-bit file ID together; a path, size or timestamp is not a file identity.

use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ID_INFO, FILE_READ_ATTRIBUTES, FileIdInfo, GetFileInformationByHandleEx,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity {
    volume: u64,
    id: [u8; 16],
}

pub(super) fn identity(path: &Path) -> io::Result<FileIdentity> {
    // Attribute-only access is enough; retain the standard read/write/delete
    // sharing so the identity probe does not obstruct log rotation.
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .open(path)?;
    let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: the owned file handle and correctly sized/aligned output buffer
    // remain live through the synchronous call. Read output only on success.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(FileIdentity {
        volume: info.VolumeSerialNumber,
        id: info.FileId.Identifier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_follows_the_open_file_through_rename_and_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("イベント.log");
        let rotated = dir.path().join("rotated.log");
        std::fs::write(&path, b"old").unwrap();
        // Keep the old object alive: do not assume IDs are never reused after
        // deleting a file, or rely on an inode allocator's intermediate state.
        let original = std::fs::File::open(&path).unwrap();
        let before = identity(&path).unwrap();
        std::fs::rename(&path, &rotated).unwrap();
        std::fs::write(&path, b"new").unwrap();
        assert_eq!(identity(&rotated).unwrap(), before);
        assert_ne!(identity(&path).unwrap(), before);
        drop(original);
    }

    #[test]
    fn truncating_a_file_preserves_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.log");
        std::fs::write(&path, b"original").unwrap();
        let before = identity(&path).unwrap();
        std::fs::write(&path, b"short").unwrap();
        assert_eq!(identity(&path).unwrap(), before);
    }

    #[test]
    fn missing_file_is_an_error_and_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.log");
        assert_eq!(identity(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!path.exists());
    }
}
