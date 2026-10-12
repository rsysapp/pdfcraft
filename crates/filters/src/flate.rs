//! FlateDecode (ISO 32000-2 §7.4.4): zlib (RFC 1950) wrapping deflate (RFC 1951).
//!
//! Real-world quirks handled:
//! - the Adler-32 trailer is not required (some producers omit or corrupt it), because the
//!   deflate stream is inflated raw after the 2-byte zlib header;
//! - streams without a valid zlib header are inflated as raw deflate;
//! - data after the end of the deflate stream is ignored.

use std::io::Write;

use flate2::{Compression, Decompress, FlushDecompress, Status};

use crate::{Failure, Step};

const NAME: &str = "FlateDecode";

/// Checks the RFC 1950 header: CM = 8, CINFO ≤ 7 and the FCHECK multiple-of-31 rule.
fn zlib_header(data: &[u8]) -> Option<(u8, u8)> {
    let (&cmf, &flg) = (data.first()?, data.get(1)?);
    let ok = cmf & 0x0f == 8 && cmf >> 4 <= 7 && (u16::from(cmf) << 8 | u16::from(flg)) % 31 == 0;
    ok.then_some((cmf, flg))
}

pub(crate) fn decode(data: &[u8], max: usize) -> Step {
    match zlib_header(data) {
        Some((_, flg)) if flg & 0x20 != 0 => Err(Failure::corrupt(NAME, "zlib preset dictionary is not allowed in PDF", Vec::new())),
        Some(_) => match inflate_raw(&data[2..], max) {
            // A header that only looked valid: retry the whole input as raw deflate.
            Err(f) if f.partial.is_empty() && matches!(f.error, crate::FilterError::Corrupt { .. }) => inflate_raw(data, max).map_err(|_| f),
            r => r,
        },
        None => inflate_raw(data, max),
    }
}

/// Embed build: where the zlib (or raw deflate) data in `data` ends: past the deflate stream
/// and, after a zlib header, its Adler-32 trailer (as far as it is there). `None` when the data
/// does not inflate to its end within `max` bytes of output. Also the bytes it inflated to.
pub(crate) fn end(data: &[u8], max: usize) -> Option<(usize, usize)> {
    let raw_end = |input: &[u8]| -> Option<(usize, usize)> {
        let mut d = Decompress::new(false);
        // Inflated a block at a time and dropped: only how far the input goes is wanted.
        let mut block = vec![0u8; 64 << 10];
        loop {
            let (consumed, made) = (d.total_in(), d.total_out());
            let status = d.decompress(input.get(usize::try_from(consumed).ok()?..)?, &mut block, FlushDecompress::None).ok()?;
            if d.total_out() > max as u64 {
                return None;
            }
            if status == Status::StreamEnd {
                return Some((usize::try_from(d.total_in()).ok()?, usize::try_from(d.total_out()).ok()?));
            }
            if d.total_in() == consumed && d.total_out() == made {
                return None;
            }
        }
    };
    match zlib_header(data) {
        Some((_, flg)) if flg & 0x20 != 0 => None,
        Some(_) => match raw_end(data.get(2..)?) {
            Some((deflate, made)) => Some((data.len().min(deflate.saturating_add(2).saturating_add(4)), made)),
            None => raw_end(data),
        },
        None => raw_end(data),
    }
}

fn inflate_raw(input: &[u8], max: usize) -> Step {
    let mut d = Decompress::new(false);
    let mut out: Vec<u8> = Vec::new();
    // One byte beyond the limit lets us tell "exactly max" from "more than max".
    let cap = max.saturating_add(1);
    loop {
        if out.len() == out.capacity() {
            if out.len() >= cap {
                return Err(Failure::limit(max));
            }
            let grow = out.capacity().max(input.len().saturating_mul(2)).max(4096).min(cap - out.len());
            out.reserve_exact(grow);
        }
        let consumed = d.total_in() as usize;
        let produced = out.len();
        let res = d.decompress_vec(&input[consumed.min(input.len())..], &mut out, FlushDecompress::None);
        if out.len() > max {
            return Err(Failure::limit(max));
        }
        match res {
            Err(e) => return Err(Failure::corrupt(NAME, e.to_string(), out)),
            Ok(Status::StreamEnd) => return Ok(out),
            Ok(Status::Ok | Status::BufError) => {
                let progressed = d.total_in() as usize != consumed || out.len() != produced;
                let room_left = out.len() < out.capacity();
                if room_left && (d.total_in() as usize >= input.len() || !progressed) {
                    return Err(Failure::corrupt(NAME, "unexpected end of deflate data", out));
                }
            }
        }
    }
}

/// zlib at level 6.
// The documented never-crash exception: writing into a `Vec` cannot fail.
#[allow(clippy::expect_used)]
pub(crate) fn encode(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 64), Compression::new(6));
    e.write_all(data).expect("in-memory write");
    e.finish().expect("in-memory write")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FilterError;

    fn dec(d: &[u8]) -> Result<Vec<u8>, FilterError> {
        decode(d, 1 << 24).map_err(|f| f.error)
    }

    #[test]
    fn where_flate_data_ends() {
        // Embed build: the data after the zlib stream (its Adler-32 included) is found.
        let mut data = encode(b"hello hello hello");
        let len = data.len();
        assert_eq!(end(&data, 1 << 20), Some((len, 17)));
        data.extend_from_slice(b"SECRET");
        assert_eq!(end(&data, 1 << 20).map(|(at, _)| at), Some(len));
        // Bounded by what it may make, and data that does not end is no end.
        assert_eq!(end(&encode(&[0; 4096]), 100), None);
        assert_eq!(end(&data[..len / 2], 1 << 20), None);
    }

    #[test]
    fn known_zlib_vector() {
        // zlib("a"): header 78 9C, fixed-Huffman block, Adler-32 0x00620062.
        assert_eq!(dec(&[0x78, 0x9c, 0x4b, 0x04, 0x00, 0x00, 0x62, 0x00, 0x62]).unwrap(), b"a");
    }

    #[test]
    fn hand_built_stored_block() {
        // RFC 1951 §3.2.4 stored block: BFINAL=1 BTYPE=00, LEN=5, NLEN=!5, "hello",
        // then Adler-32("hello") = 0x062C0215.
        let v = [0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o', 0x06, 0x2c, 0x02, 0x15];
        assert_eq!(dec(&v).unwrap(), b"hello");
        // Missing Adler-32 is tolerated.
        assert_eq!(dec(&v[..12]).unwrap(), b"hello");
        // Wrong Adler-32 is tolerated (as in other readers).
        let mut bad = v;
        bad[15] ^= 0xff;
        assert_eq!(dec(&bad).unwrap(), b"hello");
        // Trailing garbage after the stream is ignored.
        let mut trailing = v.to_vec();
        trailing.extend_from_slice(b"\r\nendstream");
        assert_eq!(dec(&trailing).unwrap(), b"hello");
    }

    #[test]
    fn raw_deflate_fallback() {
        let data = b"raw deflate without any zlib header, repeated repeated repeated".repeat(4);
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), Compression::new(6));
        e.write_all(&data).unwrap();
        let raw = e.finish().unwrap();
        assert!(zlib_header(&raw).is_none());
        assert_eq!(dec(&raw).unwrap(), data);
    }

    #[test]
    fn round_trip_and_missing_checksum() {
        let data: Vec<u8> = (0..50_000u32).map(|i| (i * i % 251) as u8).collect();
        let z = encode(&data);
        assert_eq!(&z[..2], &[0x78, 0x9c]);
        assert_eq!(dec(&z).unwrap(), data);
        assert_eq!(dec(&z[..z.len() - 4]).unwrap(), data);
        assert_eq!(dec(&z[..z.len() - 2]).unwrap(), data);
    }

    #[test]
    fn empty_input() {
        assert_eq!(dec(&encode(b"")).unwrap(), b"");
        assert!(matches!(dec(b""), Err(FilterError::Corrupt { .. })));
    }

    #[test]
    fn corrupt_and_truncated() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 13) as u8).collect();
        let z = encode(&data);
        match decode(&z[..z.len() / 2], 1 << 24) {
            Err(f) => {
                assert!(matches!(f.error, FilterError::Corrupt { .. }));
                assert_eq!(&data[..f.partial.len()], &f.partial[..]);
            }
            Ok(_) => panic!("truncated stream decoded"),
        }
        // BTYPE = 11 is reserved (RFC 1951 §3.2.3).
        assert!(matches!(dec(&[0x78, 0x9c, 0x07, 0, 0, 0]), Err(FilterError::Corrupt { .. })));
        // Preset dictionary.
        assert!(matches!(dec(&[0x78, 0x20, 0, 0, 0, 0]), Err(FilterError::Corrupt { .. })));
    }

    #[test]
    fn bomb_is_bounded() {
        let z = encode(&vec![0u8; 16 << 20]);
        assert!(z.len() < 32 * 1024);
        assert!(matches!(dec_limit(&z, 1 << 20), Err(FilterError::LimitExceeded(n)) if n == 1 << 20));
        assert_eq!(dec_limit(&z, 16 << 20).unwrap().len(), 16 << 20);
        assert!(matches!(dec_limit(&z, (16 << 20) - 1), Err(FilterError::LimitExceeded(_))));
        assert!(matches!(dec_limit(&z, 0), Err(FilterError::LimitExceeded(0))));
    }

    fn dec_limit(d: &[u8], max: usize) -> Result<Vec<u8>, FilterError> {
        decode(d, max).map_err(|f| f.error)
    }
}
