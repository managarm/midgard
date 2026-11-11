//! A small builder for AML, the bytecode of the ACPI definition blocks (the DSDT). Each
//! function encodes one ASL construct; they nest like the ASL that they correspond to.
//! Names and values are static, so invalid ones are bugs and panic.

const ZERO_OP: u8 = 0x00;
const ONE_OP: u8 = 0x01;
const NAME_OP: u8 = 0x08;
const BYTE_PREFIX: u8 = 0x0A;
const WORD_PREFIX: u8 = 0x0B;
const DWORD_PREFIX: u8 = 0x0C;
const QWORD_PREFIX: u8 = 0x0E;
const PACKAGE_OP: u8 = 0x12;
const DUAL_NAME_PREFIX: u8 = 0x2E;
const MULTI_NAME_PREFIX: u8 = 0x2F;
const ROOT_CHAR: u8 = b'\\';
const PARENT_PREFIX_CHAR: u8 = b'^';
const NULL_NAME: u8 = 0x00;

/// An encoded term (an element of a TermList), e.g., a Name.
pub(crate) struct Term(Vec<u8>);

/// An encoded data object (a DataRefObject), e.g., an integer or a package.
pub(crate) struct Data(Vec<u8>);

/// The encoding of a TermList, e.g., the body of a definition block.
pub(crate) fn term_list(terms: &[Term]) -> Vec<u8> {
    terms
        .iter()
        .flat_map(|term| term.0.iter().copied())
        .collect()
}

/// `contents`, preceded by its PkgLength (which counts its own bytes, too).
fn with_pkg_length(contents: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(contents.len() + 4);
    // One byte holds lengths up to 63; with more bytes, the first holds the low nibble
    // and the number of bytes that follow.
    if contents.len() < 63 {
        bytes.push(contents.len() as u8 + 1);
    } else {
        let follow = (1..=3)
            .find(|&follow| contents.len() + 1 + follow < 1 << (4 + 8 * follow))
            .expect("AML package too long");
        let length = contents.len() + 1 + follow;
        bytes.push((follow << 6) as u8 | (length & 0xF) as u8);
        bytes.extend((0..follow).map(|i| (length >> (4 + 8 * i)) as u8));
    }
    bytes.extend_from_slice(contents);
    bytes
}

/// A NameSeg: up to four characters (uppercase letters, digits and underscores, not
/// starting with a digit), padded with underscores.
fn name_seg(segment: &str) -> [u8; 4] {
    let valid = (1..=4).contains(&segment.len())
        && segment
            .bytes()
            .enumerate()
            .all(|(i, c)| c.is_ascii_uppercase() || c == b'_' || (i > 0 && c.is_ascii_digit()));
    assert!(valid, "invalid AML name segment {segment:?}");
    let mut bytes = [b'_'; 4];
    bytes[..segment.len()].copy_from_slice(segment.as_bytes());
    bytes
}

/// A NameString as in ASL: an optional `\` (the root) or `^`s (parents), then name
/// segments separated by dots (e.g., `\_SB.PCI0`).
fn name_string(path: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut rest = path;
    if let Some(relative) = rest.strip_prefix('\\') {
        bytes.push(ROOT_CHAR);
        rest = relative;
    } else {
        while let Some(relative) = rest.strip_prefix('^') {
            bytes.push(PARENT_PREFIX_CHAR);
            rest = relative;
        }
    }
    if rest.is_empty() {
        bytes.push(NULL_NAME);
        return bytes;
    }
    let segments: Vec<&str> = rest.split('.').collect();
    match segments.len() {
        1 => {}
        2 => bytes.push(DUAL_NAME_PREFIX),
        count => {
            let count = u8::try_from(count).expect("AML name path too long");
            bytes.extend([MULTI_NAME_PREFIX, count]);
        }
    }
    for segment in segments {
        bytes.extend(name_seg(segment));
    }
    bytes
}

/// `Name (path, data)`.
pub(crate) fn name(path: &str, data: Data) -> Term {
    let mut bytes = vec![NAME_OP];
    bytes.extend(name_string(path));
    bytes.extend(data.0);
    Term(bytes)
}

/// An integer in its shortest encoding.
pub(crate) fn integer(value: u64) -> Data {
    let (prefix, size) = match value {
        0 => return Data(vec![ZERO_OP]),
        1 => return Data(vec![ONE_OP]),
        2..=0xFF => (BYTE_PREFIX, 1),
        0x100..=0xFFFF => (WORD_PREFIX, 2),
        0x1_0000..=0xFFFF_FFFF => (DWORD_PREFIX, 4),
        _ => (QWORD_PREFIX, 8),
    };
    let mut bytes = vec![prefix];
    bytes.extend_from_slice(&value.to_le_bytes()[..size]);
    Data(bytes)
}

/// `Package () { elements }`.
pub(crate) fn package(elements: &[Data]) -> Data {
    let count = u8::try_from(elements.len()).expect("AML package has too many elements");
    let mut contents = vec![count];
    for element in elements {
        contents.extend_from_slice(&element.0);
    }
    let mut bytes = vec![PACKAGE_OP];
    bytes.extend(with_pkg_length(&contents));
    Data(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_length() {
        // The largest one-byte length, then the smallest two-byte one.
        assert_eq!(with_pkg_length(&[0; 62])[0], 63);
        assert_eq!(with_pkg_length(&[0; 63])[..2], [0x41, 0x04]);
        // The largest two-byte length (0xFFF), then the smallest three-byte one.
        assert_eq!(with_pkg_length(&[0; 0xFFD])[..2], [0x4F, 0xFF]);
        assert_eq!(with_pkg_length(&[0; 0xFFE])[..3], [0x81, 0x00, 0x01]);
    }

    #[test]
    fn name_strings() {
        assert_eq!(name_string("_S5"), b"_S5_");
        assert_eq!(name_string("\\"), b"\\\0");
        assert_eq!(name_string("\\_SB"), b"\\_SB_");
        assert_eq!(name_string("^^COM1"), b"^^COM1");
        assert_eq!(name_string("\\_SB.PCI0"), b"\\\x2E_SB_PCI0");
        assert_eq!(name_string("_SB.PCI0.S08"), b"\x2F\x03_SB_PCI0S08_");
    }

    #[test]
    #[should_panic]
    fn name_segment_too_long() {
        name_string("COM10");
    }

    #[test]
    #[should_panic]
    fn name_segment_starts_with_digit() {
        name_string("1COM");
    }

    #[test]
    fn integers() {
        assert_eq!(integer(0).0, [ZERO_OP]);
        assert_eq!(integer(1).0, [ONE_OP]);
        assert_eq!(integer(0xFF).0, [BYTE_PREFIX, 0xFF]);
        assert_eq!(integer(0x100).0, [WORD_PREFIX, 0x00, 0x01]);
        assert_eq!(integer(0x1_0000).0, [DWORD_PREFIX, 0x00, 0x00, 0x01, 0x00]);
        assert_eq!(integer(1 << 32).0, [QWORD_PREFIX, 0, 0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn named_package() {
        // Name (_S5, Package () { 5, 5, 0, 0 }), as iasl encodes it.
        let elements = [integer(5), integer(5), integer(0), integer(0)];
        assert_eq!(
            term_list(&[name("_S5", package(&elements))]),
            [
                0x08, 0x5F, 0x53, 0x35, 0x5F, 0x12, 0x08, 0x04, 0x0A, 0x05, 0x0A, 0x05, 0x00, 0x00
            ]
        );
    }
}
