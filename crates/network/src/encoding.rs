//! `Content-Encoding` decoding — Milestone 32. The conformance corpus found
//! a server (python.org, behind a CDN) that sends a gzip body to a client
//! that never offered to accept one; before this the engine tried to parse
//! the compressed bytes as HTML. Browsers decode `gzip` and `deflate`
//! whatever they asked for, and so does this, advertising
//! `Accept-Encoding: gzip, deflate` (a common, non-identifying value) since
//! it can.
//!
//! Zero-trust: the body is attacker-controlled and a compression bomb is the
//! classic abuse, so decoding stops at `limit` bytes of OUTPUT, however small
//! the input. The gzip header parser is fuzzed (`fuzz_targets/content_decoding.rs`).

/// Why a body could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// An encoding the engine does not implement (and did not advertise).
    Unsupported(String),
    /// The compressed data is malformed or truncated.
    Corrupt(&'static str),
    /// Decoding would exceed the output limit.
    TooLarge,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Unsupported(e) => write!(f, "unsupported content-encoding {e:?}"),
            DecodeError::Corrupt(why) => write!(f, "corrupt compressed body: {why}"),
            DecodeError::TooLarge => write!(f, "decoded body exceeds the size limit"),
        }
    }
}

/// Most encodings stacked in one `Content-Encoding` header that are undone
/// (`gzip, gzip` is legal and pointless; more is abuse).
const MAX_LAYERS: usize = 2;

/// Undoes `content_encoding` (the header's value, if any) on `body`, producing
/// at most `limit` bytes. `identity` or no header returns the body unchanged.
pub fn decode_body(content_encoding: Option<&str>, body: Vec<u8>, limit: usize) -> Result<Vec<u8>, DecodeError> {
    let Some(header) = content_encoding else { return Ok(body) };
    let layers: Vec<String> = header.split(',').map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty() && e != "identity").collect();
    if layers.len() > MAX_LAYERS {
        return Err(DecodeError::Unsupported(header.to_string()));
    }
    let mut data = body;
    // Encodings are listed in the order they were applied: undo in reverse.
    for encoding in layers.iter().rev() {
        data = match encoding.as_str() {
            "gzip" | "x-gzip" => gunzip(&data, limit)?,
            "deflate" => inflate_deflate(&data, limit)?,
            other => return Err(DecodeError::Unsupported(other.to_string())),
        };
    }
    Ok(data)
}

fn inflate_error(e: miniz_oxide::inflate::DecompressError) -> DecodeError {
    if e.status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput {
        DecodeError::TooLarge
    } else {
        DecodeError::Corrupt("deflate stream")
    }
}

/// HTTP `deflate` is meant to be zlib-wrapped; some servers send a raw
/// deflate stream. Try zlib, then raw.
fn inflate_deflate(data: &[u8], limit: usize) -> Result<Vec<u8>, DecodeError> {
    match miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, limit) {
        Ok(out) => Ok(out),
        Err(e) if e.status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput => Err(DecodeError::TooLarge),
        Err(_) => miniz_oxide::inflate::decompress_to_vec_with_limit(data, limit).map_err(inflate_error),
    }
}

/// One gzip member (RFC 1952): a header, a raw deflate stream, an 8-byte
/// trailer (CRC and size — not checked; the transport already did).
fn gunzip(data: &[u8], limit: usize) -> Result<Vec<u8>, DecodeError> {
    const FHCRC: u8 = 0x02;
    const FEXTRA: u8 = 0x04;
    const FNAME: u8 = 0x08;
    const FCOMMENT: u8 = 0x10;
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b {
        return Err(DecodeError::Corrupt("not a gzip stream"));
    }
    if data[2] != 8 {
        return Err(DecodeError::Corrupt("gzip compression method is not deflate"));
    }
    let flags = data[3];
    let mut at = 10usize;
    if flags & FEXTRA != 0 {
        let len = data.get(at..at + 2).ok_or(DecodeError::Corrupt("gzip header"))?;
        at += 2 + u16::from_le_bytes([len[0], len[1]]) as usize;
    }
    for flag in [FNAME, FCOMMENT] {
        if flags & flag != 0 {
            let end = data.get(at..).and_then(|rest| rest.iter().position(|&b| b == 0)).ok_or(DecodeError::Corrupt("gzip header"))?;
            at += end + 1;
        }
    }
    if flags & FHCRC != 0 {
        at += 2;
    }
    if at + 8 > data.len() {
        return Err(DecodeError::Corrupt("gzip header"));
    }
    miniz_oxide::inflate::decompress_to_vec_with_limit(&data[at..data.len() - 8], limit).map_err(inflate_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip(payload: &[u8], flags: u8, extra: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, flags, 0, 0, 0, 0, 0, 3];
        out.extend_from_slice(extra);
        out.extend(miniz_oxide::deflate::compress_to_vec(payload, 6));
        out.extend_from_slice(&[0; 8]); // trailer (CRC/size not checked)
        out
    }

    #[test]
    fn gzip_deflate_and_identity_decode() {
        let html = b"<html><body>hello</body></html>".repeat(50);
        assert_eq!(decode_body(Some("gzip"), gzip(&html, 0, &[]), 1 << 20).unwrap(), html);
        assert_eq!(decode_body(Some("x-gzip"), gzip(&html, 0x08, b"name.html\0"), 1 << 20).unwrap(), html, "FNAME skipped");
        assert_eq!(decode_body(Some("gzip"), gzip(&html, 0x04 | 0x10, b"\x03\x00abcnote\0"), 1 << 20).unwrap(), html, "FEXTRA + FCOMMENT skipped");
        let zlib = miniz_oxide::deflate::compress_to_vec_zlib(&html, 6);
        assert_eq!(decode_body(Some("deflate"), zlib, 1 << 20).unwrap(), html);
        let raw = miniz_oxide::deflate::compress_to_vec(&html, 6);
        assert_eq!(decode_body(Some("Deflate"), raw, 1 << 20).unwrap(), html, "raw deflate accepted too");
        assert_eq!(decode_body(Some("identity"), html.clone(), 1).unwrap(), html);
        assert_eq!(decode_body(None, html.clone(), 1).unwrap(), html);
        let twice = gzip(&gzip(&html, 0, &[]), 0, &[]);
        assert_eq!(decode_body(Some("gzip, gzip"), twice, 1 << 20).unwrap(), html);
    }

    #[test]
    fn a_compression_bomb_stops_at_the_output_limit() {
        let bomb = gzip(&vec![0u8; 64 * 1024 * 1024], 0, &[]);
        assert!(bomb.len() < 128 * 1024, "64 MiB of zeros compresses to {} bytes", bomb.len());
        assert_eq!(decode_body(Some("gzip"), bomb, 1024 * 1024), Err(DecodeError::TooLarge));
    }

    #[test]
    fn hostile_and_unsupported_bodies_are_errors_not_panics() {
        for (encoding, body) in [
            ("gzip", vec![]),
            ("gzip", vec![0x1f, 0x8b]),
            ("gzip", vec![0x1f, 0x8b, 8, 0x04, 0, 0, 0, 0, 0, 3, 0xff, 0xff]),
            ("gzip", vec![0x1f, 0x8b, 8, 0x08, 0, 0, 0, 0, 0, 3, b'n', b'o', b' ', b'e', b'n', b'd']),
            ("gzip", b"<html>not actually gzip</html>".to_vec()),
            ("deflate", vec![0xff; 100]),
        ] {
            assert!(decode_body(Some(encoding), body, 1 << 20).is_err());
        }
        assert_eq!(decode_body(Some("br"), vec![1, 2, 3], 1 << 20), Err(DecodeError::Unsupported("br".into())));
        assert!(decode_body(Some("gzip, gzip, gzip"), vec![], 1 << 20).is_err(), "too many layers");
    }
}
