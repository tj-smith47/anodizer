//! The `Content-Type` an uploaded object is given, decided from its first
//! bytes by the WHATWG MIME-sniffing table.

/// How many leading bytes of a file decide its content type.
pub(crate) const SNIFF_BYTES: usize = 512;

const OCTET_STREAM: &str = "application/octet-stream";
const TEXT_PLAIN: &str = "text/plain; charset=utf-8";
const TEXT_HTML: &str = "text/html; charset=utf-8";

/// Tags that mark a document as HTML when they open it, compared without
/// regard to the case of the data.
const HTML_TAGS: &[&[u8]] = &[
    b"<!DOCTYPE HTML",
    b"<HTML",
    b"<HEAD",
    b"<SCRIPT",
    b"<IFRAME",
    b"<H1",
    b"<DIV",
    b"<FONT",
    b"<TABLE",
    b"<A",
    b"<STYLE",
    b"<TITLE",
    b"<B",
    b"<BODY",
    b"<BR",
    b"<P",
    b"<!--",
];

/// A byte pattern compared under a mask: `data[i] & mask[i] == pattern[i]`
/// for every byte of the pattern.
struct Masked {
    mask: &'static [u8],
    pattern: &'static [u8],
    content_type: &'static str,
}

impl Masked {
    fn matches(&self, data: &[u8]) -> bool {
        data.len() >= self.pattern.len()
            && self
                .pattern
                .iter()
                .zip(self.mask)
                .zip(data)
                .all(|((pattern, mask), byte)| byte & mask == *pattern)
    }
}

/// A pattern whose every byte is compared.
const fn exact(pattern: &'static [u8], content_type: &'static str) -> Signature {
    Signature::Exact(pattern, content_type)
}

enum Signature {
    Exact(&'static [u8], &'static str),
    Masked(Masked),
    Mp4,
}

/// The signatures tried, in this order, once the HTML and XML checks have
/// failed and before the text check. The order is part of the answer: `RIFF`
/// alone opens three of them.
const SIGNATURES: &[Signature] = &[
    exact(b"%PDF-", "application/pdf"),
    exact(b"%!PS-Adobe-", "application/postscript"),
    Signature::Masked(Masked {
        mask: b"\xFF\xFF\x00\x00",
        pattern: b"\xFE\xFF\x00\x00",
        content_type: "text/plain; charset=utf-16be",
    }),
    Signature::Masked(Masked {
        mask: b"\xFF\xFF\x00\x00",
        pattern: b"\xFF\xFE\x00\x00",
        content_type: "text/plain; charset=utf-16le",
    }),
    Signature::Masked(Masked {
        mask: b"\xFF\xFF\xFF\x00",
        pattern: b"\xEF\xBB\xBF\x00",
        content_type: TEXT_PLAIN,
    }),
    exact(b"\x00\x00\x01\x00", "image/x-icon"),
    exact(b"\x00\x00\x02\x00", "image/x-icon"),
    exact(b"BM", "image/bmp"),
    exact(b"GIF87a", "image/gif"),
    exact(b"GIF89a", "image/gif"),
    Signature::Masked(Masked {
        mask: b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF",
        pattern: b"RIFF\x00\x00\x00\x00WEBPVP",
        content_type: "image/webp",
    }),
    exact(b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
    exact(b"\xFF\xD8\xFF", "image/jpeg"),
    Signature::Masked(Masked {
        mask: b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        pattern: b"FORM\x00\x00\x00\x00AIFF",
        content_type: "audio/aiff",
    }),
    exact(b"ID3", "audio/mpeg"),
    exact(b"OggS\x00", "application/ogg"),
    exact(b"MThd\x00\x00\x00\x06", "audio/midi"),
    Signature::Masked(Masked {
        mask: b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        pattern: b"RIFF\x00\x00\x00\x00AVI ",
        content_type: "video/avi",
    }),
    Signature::Masked(Masked {
        mask: b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        pattern: b"RIFF\x00\x00\x00\x00WAVE",
        content_type: "audio/wave",
    }),
    Signature::Mp4,
    exact(b"\x1A\x45\xDF\xA3", "video/webm"),
    Signature::Masked(Masked {
        mask: b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xFF\xFF",
        pattern: b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00LP",
        content_type: "application/vnd.ms-fontobject",
    }),
    exact(b"\x00\x01\x00\x00", "font/ttf"),
    exact(b"OTTO", "font/otf"),
    exact(b"ttcf", "font/collection"),
    exact(b"wOFF", "font/woff"),
    exact(b"wOF2", "font/woff2"),
    exact(b"\x1F\x8B\x08", "application/x-gzip"),
    exact(b"PK\x03\x04", "application/zip"),
    exact(b"Rar!\x1A\x07\x00", "application/x-rar-compressed"),
    exact(b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
    exact(b"\x00\x61\x73\x6D", "application/wasm"),
];

/// The content type of a file whose first bytes are `data`.
///
/// Only the first [`SNIFF_BYTES`] bytes are read. A file matching no
/// signature is `text/plain; charset=utf-8` when those bytes hold no binary
/// control byte — which includes the empty file — and
/// `application/octet-stream` otherwise.
pub(crate) fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(SNIFF_BYTES)];
    let after_whitespace = &data[data
        .iter()
        .position(|b| !matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' '))
        .unwrap_or(data.len())..];

    if HTML_TAGS
        .iter()
        .any(|tag| opens_with_tag(after_whitespace, tag))
    {
        return TEXT_HTML;
    }
    if after_whitespace.starts_with(b"<?xml") {
        return "text/xml; charset=utf-8";
    }
    for signature in SIGNATURES {
        match signature {
            Signature::Exact(pattern, content_type) if data.starts_with(pattern) => {
                return content_type;
            }
            Signature::Masked(masked) if masked.matches(data) => return masked.content_type,
            Signature::Mp4 if is_mp4(data) => return "video/mp4",
            _ => {}
        }
    }
    let binary = after_whitespace
        .iter()
        .any(|b| matches!(b, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F));
    if binary { OCTET_STREAM } else { TEXT_PLAIN }
}

/// Whether `data` opens with `tag` — upper-case letters of the tag matching
/// either case — followed by a space or `>`.
fn opens_with_tag(data: &[u8], tag: &[u8]) -> bool {
    data.len() > tag.len()
        && tag.iter().zip(data).all(|(want, got)| {
            if want.is_ascii_uppercase() {
                got & 0xDF == *want
            } else {
                got == want
            }
        })
        && matches!(data[tag.len()], b' ' | b'>')
}

/// Whether `data` opens with an ISO base media `ftyp` box naming an `mp4`
/// brand. The four bytes after the major brand are its version, not a brand.
fn is_mp4(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    let box_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < box_size || !box_size.is_multiple_of(4) || &data[4..8] != b"ftyp" {
        return false;
    }
    (8..box_size)
        .step_by(4)
        .any(|at| at != 12 && &data[at..at + 3] == b"mp4")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row's expected value is what Go 1.26's `http.DetectContentType`
    /// returns for the same bytes.
    #[test]
    fn the_content_type_matches_the_reference_table() {
        let cases: &[(&str, &[u8], &str)] = &[
            (".tar.gz / .tgz / .apk", b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\xed\xbd\x07", "application/x-gzip"),
            (".zip", b"PK\x03\x04\x14\x00\x00\x00\x08\x00", "application/zip"),
            (".tar.xz", b"\xfd7zXZ\x00\x00\x04\xe6\xd6\xb4F", "application/octet-stream"),
            (".tar.zst", b"(\xb5/\xfd\x04X\x00\x01\x00", "application/octet-stream"),
            (".deb", b"!<arch>\ndebian-binary   1700000000  0     0     100644  4         `\n2.0\ncontrol.tar.xz  1700000000  0     0     100644  1234      `\n\xfd7zXZ\x00", "application/octet-stream"),
            (".rpm", b"\xed\xab\xee\xdb\x03\x00\x00\x00\x00\x01", "application/octet-stream"),
            (".exe", b"MZ\x90\x00\x03\x00\x00\x00\x04\x00", "application/octet-stream"),
            (".msi", b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1\x00\x00", "application/octet-stream"),
            ("ELF binary", b"\x7fELF\x02\x01\x01\x00\x00\x00", "application/octet-stream"),
            ("Mach-O 64-bit binary", b"\xcf\xfa\xed\xfe\x07\x00\x00\x01", "application/octet-stream"),
            ("Mach-O universal binary", b"\xca\xfe\xba\xbe\x00\x00\x00\x02", "application/octet-stream"),
            (".json", b"{\"version\":\"1.0.0\",\"artifacts\":[]}\n", "text/plain; charset=utf-8"),
            ("checksums .txt / .sha256", b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  app.tar.gz\n", "text/plain; charset=utf-8"),
            (".pem", b"-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\n-----END CERTIFICATE-----\n", "text/plain; charset=utf-8"),
            ("base64 .sig", b"MEUCIQDx1yZ0mJ8m3o0K5v7t2w==\n", "text/plain; charset=utf-8"),
            ("armored .asc", b"-----BEGIN PGP SIGNATURE-----\n\niQEzBAABCAAdFiEE\n-----END PGP SIGNATURE-----\n", "text/plain; charset=utf-8"),
            ("binary .sig", b"\x89\x023\x04\x00\x01\x08\x00\x1d\x16", "application/octet-stream"),
            (".pdf", b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n", "application/pdf"),
            ("empty file", b"", "text/plain; charset=utf-8"),
            ("unknown binary", b"\xde\xad\xbe\xef\x00\x01", "application/octet-stream"),
            ("whitespace only", b" \n\t\r\x0c", "text/plain; charset=utf-8"),
            ("text with ESC", b"plain \x1b[1mbold\x1b[0m\n", "text/plain; charset=utf-8"),
            ("text with a vertical tab", b"a\x0bb", "application/octet-stream"),
            ("utf-8 text", b"caf\xc3\xa9 \xe2\x9c\x93\n", "text/plain; charset=utf-8"),
            ("utf-8 BOM", b"\xef\xbb\xbfhello", "text/plain; charset=utf-8"),
            ("utf-16be BOM", b"\xfe\xff\x00h\x00i", "text/plain; charset=utf-16be"),
            ("utf-16le BOM", b"\xff\xfeh\x00i\x00", "text/plain; charset=utf-16le"),
            ("html doctype", b"<!DOCTYPE html>\n<html>", "text/html; charset=utf-8"),
            ("html after whitespace, lower case", b"\n\t <html lang=\"en\">", "text/html; charset=utf-8"),
            ("html comment", b"<!-- note -->", "text/html; charset=utf-8"),
            ("html tag without terminator", b"<htmlx>", "text/plain; charset=utf-8"),
            ("html tag at end of data", b"<html", "text/plain; charset=utf-8"),
            ("anchor tag", b"<a href=\"x\">", "text/html; charset=utf-8"),
            ("not a tag", b"<abc>", "text/plain; charset=utf-8"),
            ("xml", b"<?xml version=\"1.0\"?>\n<package/>", "text/xml; charset=utf-8"),
            ("xml after whitespace", b"  \n<?xml version=\"1.0\"?>", "text/xml; charset=utf-8"),
            ("postscript", b"%!PS-Adobe-3.0\n", "application/postscript"),
            ("ico", b"\x00\x00\x01\x00\x01\x00", "image/x-icon"),
            ("cur", b"\x00\x00\x02\x00\x01\x00", "image/x-icon"),
            ("bmp", b"BM6\x00\x00\x00", "image/bmp"),
            ("gif87a", b"GIF87a\x01\x00", "image/gif"),
            ("gif89a", b"GIF89a\x01\x00", "image/gif"),
            ("webp", b"RIFF$\x00\x00\x00WEBPVP8 ", "image/webp"),
            ("png", b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR", "image/png"),
            ("jpeg", b"\xff\xd8\xff\xe0\x00\x10JFIF", "image/jpeg"),
            ("aiff", b"FORM\x00\x00\x00\x10AIFF", "audio/aiff"),
            ("mp3 with ID3", b"ID3\x03\x00\x00", "audio/mpeg"),
            ("ogg", b"OggS\x00\x02", "application/ogg"),
            ("midi", b"MThd\x00\x00\x00\x06\x00\x01", "audio/midi"),
            ("avi", b"RIFF\x00\x00\x00\x00AVI LIST", "video/avi"),
            ("wave", b"RIFF$\x00\x00\x00WAVEfmt ", "audio/wave"),
            ("riff of another kind", b"RIFF$\x00\x00\x00ABCDEFGH", "application/octet-stream"),
            ("mp4", b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom", "video/mp4"),
            ("mp4 compatible brand", b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isommp41", "video/mp4"),
            ("mp4 box with no mp4 brand", b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isomiso2", "application/octet-stream"),
            ("mp4 box longer than the data", b"\x00\x00\x00 ftypmp42\x00\x00\x00\x00mp42isom", "application/octet-stream"),
            ("mp4 box size not a multiple of four", b"\x00\x00\x00\x12ftypmp42\x00\x00\x00\x00mp42is", "application/octet-stream"),
            ("mp4 major brand only", b"\x00\x00\x00\x0cftypmp42", "video/mp4"),
            ("webm", b"\x1aE\xdf\xa3\x01\x00", "video/webm"),
            ("ttf", b"\x00\x01\x00\x00\x00\x0c", "font/ttf"),
            ("otf", b"OTTO\x00\x0b", "font/otf"),
            ("font collection", b"ttcf\x00\x01", "font/collection"),
            ("woff", b"wOFF\x00\x01", "font/woff"),
            ("woff2", b"wOF2\x00\x01", "font/woff2"),
            ("rar 4", b"Rar!\x1a\x07\x00\xcf", "application/x-rar-compressed"),
            ("rar 5", b"Rar!\x1a\x07\x01\x00", "application/x-rar-compressed"),
            ("wasm", b"\x00asm\x01\x00\x00\x00", "application/wasm"),
            ("gzip magic cut short", b"\x1f\x8b", "application/octet-stream"),
            ("pdf magic cut short", b"%PDF", "text/plain; charset=utf-8"),
        ];
        for (name, data, expected) in cases {
            assert_eq!(detect_content_type(data), *expected, "{name}");
        }
        assert_eq!(cases.len(), 69);
    }

    /// A tar archive opens with its first member's name padded with NULs.
    #[test]
    fn a_tar_archive_is_an_octet_stream() {
        let mut header = b"app-1.0.0/".to_vec();
        header.resize(100, 0);
        header.extend_from_slice(b"0000755\0");
        assert_eq!(detect_content_type(&header), OCTET_STREAM);
    }

    #[test]
    fn only_the_first_512_bytes_decide() {
        let mut data = vec![b'a'; SNIFF_BYTES];
        data.extend_from_slice(b"\x00\x01");
        assert_eq!(detect_content_type(&data), TEXT_PLAIN);
        data[SNIFF_BYTES - 1] = 0;
        assert_eq!(detect_content_type(&data), OCTET_STREAM);
    }

    #[test]
    fn an_embedded_opentype_font_is_found_behind_its_34_ignored_bytes() {
        let mut data = vec![0u8; 34];
        data.extend_from_slice(b"LP\0");
        assert_eq!(detect_content_type(&data), "application/vnd.ms-fontobject");
        data[3] = 0x7F;
        assert_eq!(detect_content_type(&data), "application/vnd.ms-fontobject");
    }
}
