//! PRI validation per RFC 5424 §6.2.1.
//!
//! ABNF: `PRI = "<" PRIVAL ">"`, `PRIVAL = 1*3DIGIT`
//! PRI value ranges from 0 to 191 (facility 0–23 × 8 + severity 0–7).
//!
//! Returns `Ok(())` if valid, or a descriptive error message.
pub fn validate_pri(msg: &[u8]) -> Result<(), String> {
    if msg.is_empty() {
        return Err("empty message".into());
    }

    if msg[0] != b'<' {
        return Err(format!(
            "message does not start with '<' (got 0x{:02x})",
            msg[0]
        ));
    }

    // Find closing '>'
    let gt_pos = msg.iter().position(|&b| b == b'>');
    match gt_pos {
        // '<>' at minimum is at position 1, PRIVAL needs 1–3 digits → '>' at 2–4
        Some(pos) if (2..=4).contains(&pos) => {
            let prival = &msg[1..pos];
            if prival.iter().all(|b| b.is_ascii_digit())
                && let Ok(n) = std::str::from_utf8(prival).unwrap_or("").parse::<u16>() {
                    if n <= 191 {
                        return Ok(());
                    }
                    return Err(format!("PRI value {} out of range (0–191)", n));
                }
            Err(format!(
                "invalid PRI content: {:?}",
                std::str::from_utf8(prival)
            ))
        }
        Some(pos) => Err(format!("'>' at unexpected position {}", pos)),
        None => Err("no closing '>' in PRI".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pri_valid() {
        assert!(validate_pri(b"<0>msg").is_ok());
        assert!(validate_pri(b"<13>msg").is_ok());
        assert!(validate_pri(b"<134>hello world").is_ok());
        assert!(validate_pri(b"<191>max").is_ok());
    }

    #[test]
    fn test_pri_invalid_range() {
        assert!(validate_pri(b"<192>over range").is_err());
        assert!(validate_pri(b"<999>way over").is_err());
    }

    #[test]
    fn test_pri_malformed() {
        assert!(validate_pri(b"hello").is_err());
        assert!(validate_pri(b"<>msg").is_err());
        assert!(validate_pri(b"<abc>msg").is_err());
        assert!(validate_pri(b"<1234>msg").is_err());
        assert!(validate_pri(b"<13msg").is_err());
        assert!(validate_pri(b"").is_err());
    }
}
