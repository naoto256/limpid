//! LTP metadata, complete-frame wire encoding, and node-key preflight.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use prost::Message;
use ring::signature::Ed25519KeyPair;
use thiserror::Error;

pub(crate) const FRAME_MAGIC: &[u8; 4] = b"LTP\0";
pub(crate) const FRAME_VERSION: u8 = 1;
pub(crate) const META_LEN_SIZE: usize = size_of::<u32>();
pub(crate) const PAYLOAD_LEN_SIZE: usize = size_of::<u32>();
pub(crate) const FRAME_PREFIX_SIZE: usize = FRAME_MAGIC.len() + 1 + META_LEN_SIZE;
pub(crate) const MAX_META_LEN: usize = 64 * 1024;
pub(crate) const MAX_PAYLOAD_LEN: usize = 16 * 1024 * 1024;

#[derive(Clone, PartialEq, Message)]
pub(crate) struct LtpMeta {
    #[prost(bytes = "vec", tag = "1")]
    pub(crate) key: Vec<u8>,
    #[prost(message, repeated, tag = "2")]
    pub(crate) stamps: Vec<HopStamp>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct HopStamp {
    #[prost(string, tag = "1")]
    pub(crate) node_id: String,
    #[prost(fixed64, tag = "2")]
    pub(crate) arrival_unix_nano: u64,
    #[prost(fixed64, tag = "3")]
    pub(crate) departure_unix_nano: u64,
}

#[derive(Debug, Error)]
pub(crate) enum FrameError {
    #[error("invalid LTP frame magic")]
    BadMagic,
    #[error("unsupported LTP frame version {0}")]
    Version(u8),
    #[error("LTP metadata length {0} exceeds the limit")]
    MetaTooLarge(usize),
    #[error("LTP payload length {0} exceeds the limit")]
    PayloadTooLarge(usize),
    #[error("truncated LTP frame")]
    Truncated,
    #[error("trailing bytes after LTP frame")]
    Trailing,
    #[error("invalid LTP metadata: {0}")]
    MetaDecode(#[source] prost::DecodeError),
}

pub(crate) fn encode_frame(meta: &LtpMeta, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let meta_len = meta.encoded_len();
    if meta_len > MAX_META_LEN {
        return Err(FrameError::MetaTooLarge(meta_len));
    }
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge(payload.len()));
    }

    let capacity = FRAME_PREFIX_SIZE + meta_len + PAYLOAD_LEN_SIZE + payload.len();
    let mut frame = Vec::with_capacity(capacity);
    frame.extend_from_slice(FRAME_MAGIC);
    frame.push(FRAME_VERSION);
    frame.extend_from_slice(&(meta_len as u32).to_be_bytes());
    meta.encode_raw(&mut frame);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub(crate) fn decode_frame(frame: &Bytes) -> Result<(LtpMeta, Bytes), FrameError> {
    if frame.len() < FRAME_MAGIC.len() {
        return Err(FrameError::Truncated);
    }
    if &frame[..FRAME_MAGIC.len()] != FRAME_MAGIC {
        return Err(FrameError::BadMagic);
    }
    if frame.len() < FRAME_MAGIC.len() + 1 {
        return Err(FrameError::Truncated);
    }
    let version = frame[FRAME_MAGIC.len()];
    if version != FRAME_VERSION {
        return Err(FrameError::Version(version));
    }
    if frame.len() < FRAME_PREFIX_SIZE {
        return Err(FrameError::Truncated);
    }

    let meta_len = u32::from_be_bytes(
        frame[FRAME_MAGIC.len() + 1..FRAME_PREFIX_SIZE]
            .try_into()
            .expect("the metadata length field has a fixed width"),
    ) as usize;
    if meta_len > MAX_META_LEN {
        return Err(FrameError::MetaTooLarge(meta_len));
    }

    let meta_start = FRAME_PREFIX_SIZE;
    let meta_end = meta_start + meta_len;
    let payload_len_end = meta_end + PAYLOAD_LEN_SIZE;
    if frame.len() < payload_len_end {
        return Err(FrameError::Truncated);
    }
    let payload_len = u32::from_be_bytes(
        frame[meta_end..payload_len_end]
            .try_into()
            .expect("the payload length field has a fixed width"),
    ) as usize;
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge(payload_len));
    }

    let payload_end = payload_len_end + payload_len;
    if frame.len() < payload_end {
        return Err(FrameError::Truncated);
    }
    if frame.len() > payload_end {
        return Err(FrameError::Trailing);
    }

    let meta = LtpMeta::decode(&frame[meta_start..meta_end]).map_err(FrameError::MetaDecode)?;
    Ok((meta, frame.slice(payload_len_end..payload_end)))
}

/// Verifies an operator-declared node key before runtime tasks start.
///
/// The path is opened without following a final symlink. Every later
/// operation (metadata and read) uses that same descriptor, so a path
/// replacement cannot redirect validation to a different inode.
pub(crate) fn preflight_node_key(path: &Path) -> Result<()> {
    preflight_node_key_with_open_hook(path, || {})
}

const MAX_NODE_KEY_FILE_LEN: usize = 64 * 1024;

fn read_node_key_bounded<R: Read>(reader: &mut R, path: &Path) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    reader
        .take((MAX_NODE_KEY_FILE_LEN + 1) as u64)
        .read_to_end(&mut encoded)
        .with_context(|| format!("node_key '{}': read failed", path.display()))?;
    if encoded.len() > MAX_NODE_KEY_FILE_LEN {
        bail!(
            "node_key '{}': file exceeds {} bytes",
            path.display(),
            MAX_NODE_KEY_FILE_LEN
        );
    }
    Ok(encoded)
}

fn preflight_node_key_with_open_hook<F>(path: &Path, after_open: F) -> Result<()>
where
    F: FnOnce(),
{
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = options
        .open(path)
        .with_context(|| format!("node_key '{}': secure open failed", path.display()))?;

    after_open();

    let metadata = file
        .metadata()
        .with_context(|| format!("node_key '{}': fstat failed", path.display()))?;
    if !metadata.is_file() {
        bail!("node_key '{}': not a regular file", path.display());
    }

    let euid = unsafe { libc::geteuid() };
    if !node_key_owner_matches(metadata.uid(), euid) {
        bail!(
            "node_key '{}': owner uid {} does not match daemon euid {}",
            path.display(),
            metadata.uid(),
            euid
        );
    }

    let mode = metadata.permissions().mode() & 0o7777;
    if mode != 0o400 && mode != 0o600 {
        bail!(
            "node_key '{}': mode 0o{:o} must be exactly 0o400 or 0o600",
            path.display(),
            mode
        );
    }

    let encoded = read_node_key_bounded(&mut file, path)?;
    let document = pem::parse(&encoded)
        .map_err(|_| anyhow::anyhow!("node_key '{}': invalid PRIVATE KEY PEM", path.display()))?;
    if document.tag() != "PRIVATE KEY" {
        bail!("node_key '{}': expected PRIVATE KEY PEM", path.display());
    }
    Ed25519KeyPair::from_pkcs8_maybe_unchecked(document.contents()).map_err(|_| {
        anyhow::anyhow!(
            "node_key '{}': invalid Ed25519 PKCS#8 private key",
            path.display()
        )
    })?;
    Ok(())
}

fn node_key_owner_matches(owner_uid: u32, daemon_euid: u32) -> bool {
    owner_uid == daemon_euid
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::mpsc;
    use std::time::Duration;

    use ring::rand::SystemRandom;

    struct CountingReader<R> {
        inner: R,
        bytes_read: usize,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.inner.read(buffer)?;
            self.bytes_read += count;
            Ok(count)
        }
    }

    fn sample_meta() -> LtpMeta {
        LtpMeta {
            key: vec![0x01, 0x02],
            stamps: vec![HopStamp {
                node_id: "n".to_owned(),
                arrival_unix_nano: 0x0102_0304_0506_0708,
                departure_unix_nano: 0x1112_1314_1516_1718,
            }],
        }
    }

    fn frame_with_raw_meta(meta: &[u8], payload: &[u8]) -> Bytes {
        let mut frame = Vec::new();
        frame.extend_from_slice(FRAME_MAGIC);
        frame.push(FRAME_VERSION);
        frame.extend_from_slice(&(meta.len() as u32).to_be_bytes());
        frame.extend_from_slice(meta);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        Bytes::from(frame)
    }

    fn write_ed25519_key(path: &Path, mode: u32) {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        fs::write(
            path,
            pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_ref())),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn node_key_preflight_accepts_generated_ed25519_at_exact_modes() {
        for mode in [0o400, 0o600] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("node-{mode:o}.pem"));
            write_ed25519_key(&path, mode);
            preflight_node_key(&path).unwrap();
        }
    }

    #[test]
    fn node_key_preflight_accepts_standard_pkcs8_v1_ed25519() {
        // RFC 8032 section 7.1 test vector 1 seed, wrapped in the
        // RFC 8410 version-1 PrivateKeyInfo structure.
        let mut pkcs8 = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        pkcs8.extend_from_slice(&[
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node-v1.pem");
        fs::write(&path, pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8))).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        preflight_node_key(&path).unwrap();
    }

    #[test]
    fn node_key_preflight_rejects_symlinks_and_other_modes() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("node.pem");
        write_ed25519_key(&key, 0o600);
        let link = dir.path().join("node-link.pem");
        symlink(&key, &link).unwrap();
        assert!(
            format!("{:#}", preflight_node_key(&link).unwrap_err()).contains("secure open failed")
        );

        for mode in [0o440, 0o644, 0o700, 0o4600] {
            fs::set_permissions(&key, fs::Permissions::from_mode(mode)).unwrap();
            let error = preflight_node_key(&key).unwrap_err();
            assert!(
                format!("{error:#}").contains("must be exactly 0o400 or 0o600"),
                "mode {mode:o}: {error:#}"
            );
        }
    }

    #[test]
    fn node_key_preflight_rejects_a_fifo_without_waiting_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node-key.fifo");
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            tx.send(preflight_node_key(&path)).unwrap();
        });
        let error = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("secure open must not block while opening a FIFO")
            .unwrap_err();
        handle.join().unwrap();
        assert!(format!("{error:#}").contains("not a regular file"));
    }

    #[test]
    fn node_key_owner_must_match_the_daemon_even_for_root() {
        assert!(node_key_owner_matches(501, 501));
        assert!(node_key_owner_matches(0, 0));
        assert!(!node_key_owner_matches(0, 501));
        assert!(!node_key_owner_matches(501, 0));
    }

    #[test]
    fn node_key_preflight_rejects_garbage_and_wrong_algorithm_without_leaking_material() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("node.pem");
        let secret = "DO-NOT-LEAK-PRIVATE-MATERIAL";
        fs::write(&key, secret).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        let diagnostic = format!("{:#?}", preflight_node_key(&key).unwrap_err());
        assert!(!diagnostic.contains(secret));
        assert!(diagnostic.contains("invalid PRIVATE KEY PEM"));

        let wrong = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        fs::write(
            &key,
            pem::encode(&pem::Pem::new("PRIVATE KEY", wrong.serialize_der())),
        )
        .unwrap();
        let diagnostic = format!("{:#?}", preflight_node_key(&key).unwrap_err());
        assert!(diagnostic.contains("invalid Ed25519 PKCS#8 private key"));
    }

    #[test]
    fn node_key_preflight_enforces_the_exact_64_kib_file_limit() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("node.pem");
        fs::write(&key, vec![b'x'; MAX_NODE_KEY_FILE_LEN]).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        let exact_limit = format!("{:#}", preflight_node_key(&key).unwrap_err());
        assert!(exact_limit.contains("invalid PRIVATE KEY PEM"));
        assert!(!exact_limit.contains("file exceeds"));

        fs::write(&key, vec![b'x'; MAX_NODE_KEY_FILE_LEN + 1]).unwrap();
        let over_limit = format!("{:#}", preflight_node_key(&key).unwrap_err());
        assert!(over_limit.contains("file exceeds 65536 bytes"));
    }

    #[test]
    fn node_key_bounded_reader_stops_after_limit_plus_one() {
        let input = vec![b'x'; MAX_NODE_KEY_FILE_LEN + 4096];
        let mut reader = CountingReader {
            inner: std::io::Cursor::new(input),
            bytes_read: 0,
        };
        let error = read_node_key_bounded(&mut reader, Path::new("counted.pem")).unwrap_err();

        assert!(format!("{error:#}").contains("file exceeds 65536 bytes"));
        assert_eq!(reader.bytes_read, MAX_NODE_KEY_FILE_LEN + 1);
    }

    #[test]
    fn node_key_preflight_fstats_and_reads_the_open_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("node.pem");
        let moved = dir.path().join("opened-node.pem");
        write_ed25519_key(&key, 0o600);

        preflight_node_key_with_open_hook(&key, || {
            fs::rename(&key, &moved).unwrap();
            fs::write(&key, "replacement that is not a key").unwrap();
            fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        })
        .expect("validation must stay bound to the originally opened key inode");
    }

    #[test]
    fn exact_golden_frame_round_trips() {
        let expected = vec![
            0x4c, 0x54, 0x50, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1b, 0x0a, 0x02, 0x01, 0x02, 0x12,
            0x15, 0x0a, 0x01, 0x6e, 0x11, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x19,
            0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11, 0x00, 0x00, 0x00, 0x03, 0x61, 0x62,
            0x63,
        ];

        let encoded = encode_frame(&sample_meta(), b"abc").unwrap();
        assert_eq!(encoded, expected);

        let (meta, payload) = decode_frame(&Bytes::from(encoded)).unwrap();
        assert_eq!(meta, sample_meta());
        assert_eq!(payload, Bytes::from_static(b"abc"));
    }

    #[test]
    fn empty_metadata_and_payload_round_trip() {
        let meta = LtpMeta {
            key: Vec::new(),
            stamps: Vec::new(),
        };
        let encoded = encode_frame(&meta, &[]).unwrap();
        assert_eq!(
            encoded,
            [
                FRAME_MAGIC.as_slice(),
                &[FRAME_VERSION],
                &0_u32.to_be_bytes(),
                &0_u32.to_be_bytes(),
            ]
            .concat()
        );
        let (decoded, payload) = decode_frame(&Bytes::from(encoded)).unwrap();
        assert_eq!(decoded, meta);
        assert!(payload.is_empty());
    }

    #[test]
    fn every_proper_prefix_is_truncated() {
        let frame = encode_frame(&sample_meta(), b"payload").unwrap();
        for end in 0..frame.len() {
            let error = decode_frame(&Bytes::copy_from_slice(&frame[..end])).unwrap_err();
            assert!(
                matches!(error, FrameError::Truncated),
                "prefix ending at byte {end} returned {error:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_magic_and_reports_found_version() {
        let mut bad_magic = encode_frame(&sample_meta(), b"").unwrap();
        bad_magic[0] ^= 0xff;
        assert!(matches!(
            decode_frame(&Bytes::from(bad_magic)),
            Err(FrameError::BadMagic)
        ));

        let mut bad_version = encode_frame(&sample_meta(), b"").unwrap();
        bad_version[FRAME_MAGIC.len()] = 9;
        assert!(matches!(
            decode_frame(&Bytes::from(bad_version)),
            Err(FrameError::Version(9))
        ));
    }

    #[test]
    fn encode_accepts_limits_and_rejects_limit_plus_one() {
        let exact_meta = LtpMeta {
            key: vec![0; MAX_META_LEN - 4],
            stamps: Vec::new(),
        };
        assert_eq!(exact_meta.encoded_len(), MAX_META_LEN);
        let exact_meta_frame = Bytes::from(encode_frame(&exact_meta, &[]).unwrap());
        assert_eq!(decode_frame(&exact_meta_frame).unwrap().0, exact_meta);

        let large_meta = LtpMeta {
            key: vec![0; MAX_META_LEN - 3],
            stamps: Vec::new(),
        };
        assert_eq!(large_meta.encoded_len(), MAX_META_LEN + 1);
        assert!(matches!(
            encode_frame(&large_meta, &[]),
            Err(FrameError::MetaTooLarge(len)) if len == MAX_META_LEN + 1
        ));

        let empty_meta = LtpMeta {
            key: Vec::new(),
            stamps: Vec::new(),
        };
        let exact_payload_frame =
            Bytes::from(encode_frame(&empty_meta, &vec![0; MAX_PAYLOAD_LEN]).unwrap());
        assert_eq!(
            decode_frame(&exact_payload_frame).unwrap().1.len(),
            MAX_PAYLOAD_LEN
        );
        assert!(matches!(
            encode_frame(&empty_meta, &vec![0; MAX_PAYLOAD_LEN + 1]),
            Err(FrameError::PayloadTooLarge(len)) if len == MAX_PAYLOAD_LEN + 1
        ));
    }

    #[test]
    fn decode_rejects_lengths_above_limits_before_content() {
        let mut meta_frame = Vec::new();
        meta_frame.extend_from_slice(FRAME_MAGIC);
        meta_frame.push(FRAME_VERSION);
        meta_frame.extend_from_slice(&((MAX_META_LEN + 1) as u32).to_be_bytes());
        assert!(matches!(
            decode_frame(&Bytes::from(meta_frame)),
            Err(FrameError::MetaTooLarge(len)) if len == MAX_META_LEN + 1
        ));

        let mut payload_frame = frame_with_raw_meta(&[], &[]).to_vec();
        payload_frame[FRAME_PREFIX_SIZE..FRAME_PREFIX_SIZE + PAYLOAD_LEN_SIZE]
            .copy_from_slice(&((MAX_PAYLOAD_LEN + 1) as u32).to_be_bytes());
        assert!(matches!(
            decode_frame(&Bytes::from(payload_frame)),
            Err(FrameError::PayloadTooLarge(len)) if len == MAX_PAYLOAD_LEN + 1
        ));
    }

    #[test]
    fn lengths_are_big_endian() {
        let encoded = encode_frame(&sample_meta(), b"abc").unwrap();
        assert_eq!(
            &encoded[FRAME_MAGIC.len() + 1..FRAME_PREFIX_SIZE],
            &(sample_meta().encoded_len() as u32).to_be_bytes()
        );
        let payload_len_start = FRAME_PREFIX_SIZE + sample_meta().encoded_len();
        assert_eq!(
            &encoded[payload_len_start..payload_len_start + PAYLOAD_LEN_SIZE],
            &3_u32.to_be_bytes()
        );
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut frame = encode_frame(&sample_meta(), b"payload").unwrap();
        frame.push(0);
        assert!(matches!(
            decode_frame(&Bytes::from(frame)),
            Err(FrameError::Trailing)
        ));
    }

    #[test]
    fn rejects_invalid_protobuf_metadata() {
        let frame = frame_with_raw_meta(&[0x0a, 0x02, 0x01], &[]);
        assert!(matches!(
            decode_frame(&frame),
            Err(FrameError::MetaDecode(_))
        ));
    }

    #[test]
    fn accepts_unknown_protobuf_fields() {
        let frame = frame_with_raw_meta(&[0x78, 0x2a], b"payload");
        let (meta, payload) = decode_frame(&frame).unwrap();
        assert!(meta.key.is_empty());
        assert!(meta.stamps.is_empty());
        assert_eq!(payload, Bytes::from_static(b"payload"));
    }

    #[test]
    fn leaves_semantic_fields_unvalidated() {
        let meta = LtpMeta {
            key: Vec::new(),
            stamps: vec![HopStamp {
                node_id: String::new(),
                arrival_unix_nano: 0,
                departure_unix_nano: 0,
            }],
        };
        let encoded = encode_frame(&meta, b"").unwrap();
        let (decoded, _) = decode_frame(&Bytes::from(encoded)).unwrap();
        assert_eq!(decoded, meta);
    }

    #[test]
    fn decoded_payload_is_a_zero_copy_slice() {
        let frame = Bytes::from(encode_frame(&sample_meta(), b"payload").unwrap());
        let payload_offset = FRAME_PREFIX_SIZE + sample_meta().encoded_len() + PAYLOAD_LEN_SIZE;
        let expected_ptr = frame.as_ptr().wrapping_add(payload_offset);

        let (_, payload) = decode_frame(&frame).unwrap();
        assert_eq!(payload.as_ptr(), expected_ptr);
        assert_eq!(payload, Bytes::from_static(b"payload"));
    }
}
