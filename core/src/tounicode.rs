//! Minimal ToUnicode CMap parser (ISO 32000 9.10.3) for single-byte-code
//! fonts. The spec says ToUnicode is the authoritative source of a glyph's
//! text meaning; generators like Chrome/Skia emit Type3 fonts whose
//! /Encoding Differences names (g0, g1, …) carry no meaning at all, so
//! decoding must go through here whenever the map exists.
//!
//! Only 1-byte source codes are handled (simple and Type3 fonts); CID fonts
//! keep their existing path.

use std::collections::HashMap;

pub struct ToUnicodeMap {
    forward: HashMap<u8, String>,
    /// Reverse lookup for re-encoding replacements; only single-scalar
    /// targets participate (ligature-style multi-char targets are decode-only).
    reverse: HashMap<char, u8>,
}

impl ToUnicodeMap {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let toks = tokenize(data);
        let mut forward: HashMap<u8, String> = HashMap::new();
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                Tok::Kw(k) if k == "beginbfchar" => {
                    i += 1;
                    while i + 1 < toks.len() && !matches!(&toks[i], Tok::Kw(k) if k == "endbfchar") {
                        if let (Tok::Hex(src), Tok::Hex(dst)) = (&toks[i], &toks[i + 1]) {
                            if let (Some(code), Some(text)) = (one_byte(src), utf16_be(dst)) {
                                forward.insert(code, text);
                            }
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
                Tok::Kw(k) if k == "beginbfrange" => {
                    i += 1;
                    while i < toks.len() && !matches!(&toks[i], Tok::Kw(k) if k == "endbfrange") {
                        // <lo> <hi> <dstStart>  |  <lo> <hi> [ <dst> <dst> ... ]
                        if i + 2 < toks.len() {
                            if let (Tok::Hex(lo), Tok::Hex(hi)) = (&toks[i], &toks[i + 1]) {
                                if let (Some(lo), Some(hi)) = (one_byte(lo), one_byte(hi)) {
                                    match &toks[i + 2] {
                                        Tok::Hex(dst) => {
                                            if let Some(start) = utf16_first_scalar(dst) {
                                                for (off, code) in (lo..=hi).enumerate() {
                                                    if let Some(c) = char::from_u32(start + off as u32) {
                                                        forward.insert(code, c.to_string());
                                                    }
                                                }
                                            }
                                            i += 3;
                                            continue;
                                        }
                                        Tok::ArrOpen => {
                                            let mut j = i + 3;
                                            let mut code = lo;
                                            while j < toks.len() && !matches!(toks[j], Tok::ArrClose) {
                                                if let Tok::Hex(dst) = &toks[j] {
                                                    if let Some(text) = utf16_be(dst) {
                                                        forward.insert(code, text);
                                                    }
                                                    code = code.saturating_add(1);
                                                }
                                                j += 1;
                                            }
                                            i = j + 1;
                                            continue;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        i += 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        if forward.is_empty() {
            return None;
        }
        let mut reverse = HashMap::new();
        // Deterministic winner when several codes map to the same char: the
        // smallest code, so repeated parses can't flip-flop the choice.
        let mut pairs: Vec<_> = forward.iter().collect();
        pairs.sort_by_key(|(code, _)| **code);
        for (code, text) in pairs {
            let mut chars = text.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                reverse.entry(c).or_insert(*code);
            }
        }
        Some(Self { forward, reverse })
    }

    /// Decode; None if any byte has no mapping (caller falls back).
    pub fn decode(&self, bytes: &[u8]) -> Option<String> {
        let mut out = String::new();
        for b in bytes {
            out.push_str(self.forward.get(b)?);
        }
        Some(out)
    }

    /// Encode; None if any char has no single-code mapping.
    pub fn encode(&self, text: &str) -> Option<Vec<u8>> {
        text.chars().map(|c| self.reverse.get(&c).copied()).collect()
    }
}

enum Tok {
    Hex(Vec<u8>), // raw hex-string nibble bytes, already packed
    Kw(String),
    ArrOpen,
    ArrClose,
}

fn tokenize(data: &[u8]) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'<' => {
                let end = data[i + 1..].iter().position(|&b| b == b'>').map(|p| i + 1 + p);
                if let Some(end) = end {
                    let hex: Vec<u8> = data[i + 1..end]
                        .iter()
                        .copied()
                        .filter(u8::is_ascii_hexdigit)
                        .collect();
                    let packed = hex
                        .chunks(2)
                        .filter(|c| c.len() == 2)
                        .map(|c| {
                            let s = std::str::from_utf8(c).unwrap_or("00");
                            u8::from_str_radix(s, 16).unwrap_or(0)
                        })
                        .collect();
                    toks.push(Tok::Hex(packed));
                    i = end + 1;
                } else {
                    i += 1;
                }
            }
            b'[' => {
                toks.push(Tok::ArrOpen);
                i += 1;
            }
            b']' => {
                toks.push(Tok::ArrClose);
                i += 1;
            }
            b if b.is_ascii_alphabetic() => {
                let start = i;
                while i < data.len() && data[i].is_ascii_alphabetic() {
                    i += 1;
                }
                toks.push(Tok::Kw(String::from_utf8_lossy(&data[start..i]).into_owned()));
            }
            _ => i += 1,
        }
    }
    toks
}

fn one_byte(hex: &[u8]) -> Option<u8> {
    // 1-byte source codes only; 2-byte codes belong to CID fonts.
    if hex.len() == 1 { Some(hex[0]) } else { None }
}

fn utf16_be(hex: &[u8]) -> Option<String> {
    if hex.len() % 2 != 0 || hex.is_empty() {
        return None;
    }
    let units: Vec<u16> = hex.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16(&units).ok()
}

fn utf16_first_scalar(hex: &[u8]) -> Option<u32> {
    utf16_be(hex)?.chars().next().map(|c| c as u32)
}
