//! LTP metadata and complete-frame wire encoding.

use bytes::Bytes;
use prost::Message;
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

#[cfg(test)]
mod tests {
    use super::*;

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
