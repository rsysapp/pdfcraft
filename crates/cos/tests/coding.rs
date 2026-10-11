//! Embed build: a stream's `/Filter` and `/DecodeParms` apply whether they are written directly or
//! as indirect objects (ISO 32000-2 §7.3.8.2), as hayro, PDFium and pdf.js apply them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use pdfcraft_cos::{Dict, Document, Object, Stream};

/// A classic-xref file whose object `i + 1` is `bodies[i]`.
fn file(bodies: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes());
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", bodies.len() + 1).as_bytes());
    out
}

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// Object 2 is a stream with `dict`, holding `data`; objects 3 and 4 are `extra`.
fn decoded(dict: &str, data: &[u8], extra: [&str; 2]) -> Result<Vec<u8>, pdfcraft_cos::CosError> {
    let bodies = vec![b"<< /Type /Catalog >>".to_vec(), stream(dict, data), extra[0].as_bytes().to_vec(), extra[1].as_bytes().to_vec()];
    let doc = Document::open(Arc::new(file(&bodies))).unwrap();
    let Object::Stream(stream) = &*doc.try_get(2).unwrap() else { panic!("object 2 is not a stream") };
    stream.decoded_strict()
}

#[test]
fn an_indirect_filter_applies() {
    assert_eq!(decoded("/Filter 3 0 R", b"48656C6C6F>", ["/ASCIIHexDecode", "null"]).unwrap(), b"Hello");
    // An indirect entry of a filter array, too.
    assert_eq!(decoded("/Filter [3 0 R]", b"48656C6C6F>", ["/ASCIIHexDecode", "null"]).unwrap(), b"Hello");
    // Written directly, the same.
    assert_eq!(decoded("/Filter /ASCIIHexDecode", b"48656C6C6F>", ["null", "null"]).unwrap(), b"Hello");
}

#[test]
fn indirect_decode_parameters_apply() {
    // PNG "None" predictor rows of three bytes: each row starts with its filter type, 0.
    let rows = [0u8, b'a', b'b', b'c', 0, b'd', b'e', b'f'];
    let flate = Stream::flate(Dict::new(), &rows).raw;
    let direct = decoded("/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 3 >>", &flate, ["null", "null"]).unwrap();
    assert_eq!(direct, b"abcdef");
    let indirect = decoded("/Filter /FlateDecode /DecodeParms 3 0 R", &flate, ["<< /Predictor 12 /Columns 3 >>", "null"]).unwrap();
    assert_eq!(indirect, b"abcdef");
    let in_array = decoded("/Filter [/FlateDecode] /DecodeParms [3 0 R]", &flate, ["<< /Predictor 12 /Columns 3 >>", "null"]).unwrap();
    assert_eq!(in_array, b"abcdef");
}

#[test]
fn coding_entries_that_are_not_names_or_dictionaries_are_refused_by_strict_decoding() {
    // A reference to a reference is resolved one level only: what is left is not a name.
    assert!(decoded("/Filter 3 0 R", b"48656C6C6F>", ["4 0 R", "/ASCIIHexDecode"]).is_err());
    // A number, a string, a name inside a parameters array.
    assert!(decoded("/Filter 7", b"Hello", ["null", "null"]).is_err());
    assert!(decoded("/Filter [/ASCIIHexDecode (x)]", b"48656C6C6F>", ["null", "null"]).is_err());
    assert!(decoded("/Filter /ASCIIHexDecode /DecodeParms [/x]", b"48656C6C6F>", ["null", "null"]).is_err());
    // Null parameters are allowed, as is an absent filter.
    assert_eq!(decoded("/Filter [/ASCIIHexDecode] /DecodeParms [null]", b"48656C6C6F>", ["null", "null"]).unwrap(), b"Hello");
    assert_eq!(decoded("", b"Hello", ["null", "null"]).unwrap(), b"Hello");
}

#[test]
fn a_crypt_filter_is_refused_by_strict_decoding() {
    // Nothing applies it in a file without encryption, and readers apply crypt filters
    // differently.
    assert!(decoded("/Filter /Crypt /DecodeParms << /Name /Identity >>", b"Hello", ["null", "null"]).is_err());
    assert!(decoded("/Filter [/Crypt /ASCIIHexDecode] /DecodeParms [<< /Name /Identity >> null]", b"48656C6C6F>", ["null", "null"]).is_err());
    assert!(decoded("/Filter 3 0 R", b"Hello", ["/Crypt", "null"]).is_err());
}

#[test]
fn coding_entries_that_refer_back_are_refused_quickly() {
    // The stream's /Filter is the stream itself; an array naming itself; parameters naming the
    // array they are in. Resolution goes one level, and loading refuses the cycle: what is left
    // is not a name or a dictionary.
    let started = std::time::Instant::now();
    assert!(decoded("/Filter 2 0 R", b"Hello", ["null", "null"]).is_err());
    assert!(decoded("/Filter 3 0 R", b"Hello", ["[3 0 R]", "null"]).is_err());
    assert!(decoded("/Filter /ASCIIHexDecode /DecodeParms 3 0 R", b"48656C6C6F>", ["[3 0 R]", "null"]).is_err());
    assert!(decoded("/Filter 3 0 R /DecodeParms 4 0 R", b"Hello", ["4 0 R", "3 0 R"]).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn a_long_chain_and_a_large_expansion_stop_at_their_limits() {
    // Nine filters: refused before any decoding.
    let nine = ["/ASCIIHexDecode"; 9].join(" ");
    let error = decoded(&format!("/Filter [{nine}]"), b"30>", ["null", "null"]).unwrap_err();
    assert!(error.to_string().contains("exceeds the limit"), "{error}");
    // The same filter named nine times through one indirect array: the same.
    let error = decoded("/Filter 3 0 R", b"30>", [&format!("[{nine}]"), "null"]).unwrap_err();
    assert!(error.to_string().contains("exceeds the limit"), "{error}");
    // 4 MiB of zeros, compressed twice to a few kilobytes, under a 1 MiB bound: stopped at
    // the bound, every step counted.
    let zeros = vec![0u8; 4 << 20];
    let once = Stream::flate(Dict::new(), &zeros).raw;
    let twice = Stream::flate(Dict::new(), &once).raw;
    assert!(twice.len() < 64 << 10);
    let bodies = vec![b"<< /Type /Catalog >>".to_vec(), stream("/Filter [/FlateDecode /FlateDecode]", &twice), b"null".to_vec(), b"null".to_vec()];
    let doc = Document::open(Arc::new(file(&bodies))).unwrap();
    let Object::Stream(bomb) = &*doc.try_get(2).unwrap() else { panic!("object 2 is not a stream") };
    let error = bomb.decoded_strict_counted(1 << 20).unwrap_err();
    assert!(error.to_string().contains("exceeds the limit"), "{error}");
    let (data, made) = bomb.decoded_strict_counted(16 << 20).unwrap();
    assert_eq!((data.len(), made), (zeros.len(), zeros.len() + once.len()));
}
