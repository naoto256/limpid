//! Windows boundary for opening an existing file output without following the
//! final reparse point. The caller owns creation, framing, flushing and ACKs.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub(super) fn sanitize_path_component(value: &str) -> String {
    // ':' selects an NTFS alternate data stream. Treat it as a boundary in
    // interpolated values, just like slash, without changing literal paths.
    value.replace(['/', '\\', ':'], "_")
}

pub(super) fn open_existing(path: &Path) -> io::Result<File> {
    // Open the final reparse point itself, then inspect the opened handle.
    // A separate path metadata check would race a replacement of that path.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = OpenOptions::new()
        .append(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    verify_regular(file)
}

pub(super) fn create_new(path: &Path) -> io::Result<File> {
    // create_new refuses an existing final component (including a symlink).
    // Validate the handle here too: DOS device names have special open rules.
    verify_regular(
        OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(path)?,
    )
}

fn verify_regular(file: File) -> io::Result<File> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output file must be a regular file, not a device, directory or reparse point",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn interpolation_cannot_select_an_alternate_data_stream() {
        assert_eq!(sanitize_path_component("host:stream"), "host_stream");
        assert_eq!(sanitize_path_component("C:\\logs/host"), "C__logs_host");
        assert_eq!(
            sanitize_path_component("ホスト.example.com"),
            "ホスト.example.com"
        );
    }

    #[test]
    fn appends_exact_bytes_to_unicode_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("イベント.log");
        std::fs::write(&path, b"before\n").unwrap();
        let mut file = open_existing(&path).unwrap();
        file.write_all(b"\xff\x00after\n").unwrap();
        file.flush().unwrap();
        drop(file);
        assert_eq!(std::fs::read(path).unwrap(), b"before\n\xff\x00after\n");
    }

    #[test]
    fn missing_file_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.log");
        assert_eq!(
            open_existing(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!path.exists());
    }

    #[test]
    fn refuses_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_existing(dir.path()).is_err());
    }

    #[test]
    fn refuses_character_device_before_writing() {
        assert!(open_existing(Path::new("NUL")).is_err());
        assert!(create_new(Path::new("NUL")).is_err());
    }

    #[test]
    fn creates_new_file_and_refuses_to_replace_existing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.log");
        create_new(&path).unwrap().write_all(b"original\n").unwrap();
        assert_eq!(
            create_new(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(path).unwrap(), b"original\n");
    }

    #[test]
    fn refuses_final_symlink_without_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.log");
        let link = dir.path().join("link.log");
        std::fs::write(&target, b"unchanged\n").unwrap();
        std::os::windows::fs::symlink_file(&target, &link)
            .expect("native symlink test requires Developer Mode or SeCreateSymbolicLinkPrivilege");
        assert!(open_existing(&link).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"unchanged\n");
    }

    #[test]
    fn rotation_keeps_open_handle_on_original_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active.log");
        let rotated = dir.path().join("rotated.log");
        std::fs::write(&path, b"original\n").unwrap();
        let mut original = open_existing(&path).unwrap();
        std::fs::rename(&path, &rotated).unwrap();
        std::fs::write(&path, b"replacement\n").unwrap();
        original.write_all(b"old handle\n").unwrap();
        drop(original);
        open_existing(&path)
            .unwrap()
            .write_all(b"new handle\n")
            .unwrap();
        assert_eq!(std::fs::read(rotated).unwrap(), b"original\nold handle\n");
        assert_eq!(std::fs::read(path).unwrap(), b"replacement\nnew handle\n");
    }
}
