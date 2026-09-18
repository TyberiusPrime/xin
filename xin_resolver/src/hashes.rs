//! Hash types and encoding (design.md A9, C1, B12).
//!
//! Both hash kinds are 32-byte blake3. Displayed as padding-less RFC 4648
//! base32, lowercased, plus one uppercase version/type marker *appended*
//! (a suffix keeps filesystem sharding on the leading characters useful).
//! The marker's position and letter carry the information — its case never
//! does, since some filesystems are case-insensitive. Input markers come
//! before output markers in the alphabet; later hash versions get later
//! letters: 'A' = input v1, 'B' = output v1, 'C'/'D' reserved for v2.

use std::fmt;

pub const HASH_LEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputHash(pub [u8; HASH_LEN]);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputHash(pub [u8; HASH_LEN]);

/// The one hash primitive the resolver uses (C1).
pub fn hash_bytes(data: &[u8]) -> [u8; HASH_LEN] {
    *blake3::hash(data).as_bytes()
}

impl InputHash {
    pub fn of(data: &[u8]) -> Self {
        InputHash(hash_bytes(data))
    }

    /// Inverse of `Display` (52 base32 chars + the 'A' marker).
    pub fn parse(s: &str) -> Option<InputHash> {
        parse_hash(s, INPUT_V1_MARKER).map(InputHash)
    }
}

impl OutputHash {
    pub fn of(data: &[u8]) -> Self {
        OutputHash(hash_bytes(data))
    }

    /// Inverse of `Display` (52 base32 chars + the 'B' marker).
    pub fn parse(s: &str) -> Option<OutputHash> {
        parse_hash(s, OUTPUT_V1_MARKER).map(OutputHash)
    }
}

/// C1: the input-hash preimage — `output-hash:name` lines sorted byte-wise
/// by name, a `--` separator, then the opaque special-input bytes handed
/// over by the DAG definition layer. `inputs` must already be sorted.
pub fn input_hash_of(inputs: &[(&str, OutputHash)], special: &[u8]) -> InputHash {
    debug_assert!(
        inputs.windows(2).all(|w| w[0].0 < w[1].0),
        "inputs must be name-sorted and unique"
    );
    let mut pre = Vec::with_capacity(inputs.len() * 64 + special.len() + 3);
    for (name, oh) in inputs {
        pre.extend_from_slice(oh.to_string().as_bytes());
        pre.push(b':');
        pre.extend_from_slice(name.as_bytes());
        pre.push(b'\n');
    }
    pre.extend_from_slice(b"--\n");
    pre.extend_from_slice(special);
    InputHash(hash_bytes(&pre))
}

const INPUT_V1_MARKER: char = 'A';
const OUTPUT_V1_MARKER: char = 'B';

const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn decode_base32(s: &str) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    for c in s.bytes() {
        let v = BASE32.iter().position(|&b| b == c)? as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // the padding bits of the final character must be zero
    if bits > 0 && acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

fn parse_hash(s: &str, marker: char) -> Option<[u8; HASH_LEN]> {
    if s.len() != 53 || !s.ends_with(marker) {
        return None;
    }
    decode_base32(&s[..52])?.try_into().ok()
}

fn push_base32(bytes: &[u8], out: &mut String) {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((acc << (5 - bits)) & 31) as usize] as char);
    }
}

fn fmt_hash(bytes: &[u8], marker: char, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut s = String::with_capacity(53);
    push_base32(bytes, &mut s);
    s.push(marker);
    f.write_str(&s)
}

impl fmt::Display for InputHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_hash(&self.0, INPUT_V1_MARKER, f)
    }
}

impl fmt::Display for OutputHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_hash(&self.0, OUTPUT_V1_MARKER, f)
    }
}

impl fmt::Debug for InputHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let full = self.to_string();
        write!(f, "ih:{}…{}", &full[..8], INPUT_V1_MARKER)
    }
}

impl fmt::Debug for OutputHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let full = self.to_string();
        write!(f, "oh:{}…{}", &full[..8], OUTPUT_V1_MARKER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_encoding() {
        let ih = InputHash([0; 32]);
        let s = ih.to_string();
        assert_eq!(s.len(), 53); // 52 base32 chars + marker
        assert!(s.starts_with(&"a".repeat(52)));
        assert!(s.ends_with('A'));
        let oh = OutputHash([0xff; 32]);
        let s = oh.to_string();
        assert!(s.starts_with("7777"));
        assert!(s.ends_with('B'));
    }

    #[test]
    fn parse_roundtrip() {
        let oh = OutputHash::of(b"some bytes");
        assert_eq!(OutputHash::parse(&oh.to_string()), Some(oh));
        let ih = InputHash::of(b"other bytes");
        assert_eq!(InputHash::parse(&ih.to_string()), Some(ih));
        // wrong marker, wrong length, bad characters
        assert_eq!(OutputHash::parse(&ih.to_string()), None);
        assert_eq!(OutputHash::parse("tooshortB"), None);
        assert_eq!(
            OutputHash::parse(&format!("{}!", &oh.to_string()[..52])),
            None
        );
    }

    #[test]
    fn input_hash_sensitivity() {
        let oh = OutputHash::of(b"x");
        let base = input_hash_of(&[("a", oh)], b"recipe");
        assert_eq!(base, input_hash_of(&[("a", oh)], b"recipe"));
        assert_ne!(base, input_hash_of(&[("b", oh)], b"recipe")); // alias is part of the hash (C1)
        assert_ne!(base, input_hash_of(&[("a", oh)], b"recipe2"));
        assert_ne!(base, input_hash_of(&[], b"recipe"));
    }
}
