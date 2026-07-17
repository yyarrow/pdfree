//! Document information dictionary: read/write trailer /Info (ISO 32000-1
//! 14.3.3, Table 317). String encoding (PDFDocEncoding vs UTF-16BE with a
//! FEFF BOM, and PDF 2.0's UTF-8 with an EF BB BF BOM) is handled by lopdf's
//! own `text_string`/`decode_text_string` helpers (MIT, crate root
//! re-exports). lopdf 0.43's `decode_text_string` leaves the UTF-8 BOM's
//! U+FEFF in the decoded `String` instead of stripping it, so `read_info`
//! strips a leading U+FEFF itself before returning the value — but only
//! when the raw string bytes actually started with the UTF-8 BOM (`EF BB
//! BF`); see `strip_bom`. For UTF-16BE-encoded values, lopdf already
//! consumes the `FE FF` encoding BOM before decoding, so a leading U+FEFF
//! surviving into the decoded string there is real content (e.g. a
//! zero-width no-break space) and must be left alone. /Info values may
//! also be indirect references to string objects (ISO 32000-1 7.3.4),
//! which `read_info` resolves with `Document::dereference` before
//! decoding.

use lopdf::{dictionary, decode_text_string, text_string, Dictionary, Document, Object};

/// The standard /Info fields we read and write (ISO 32000-1 Table 317).
/// CreationDate/ModDate are included on read but not settable via `set_info`
/// (no clock dependency required by this module).
const FIELDS: [&str; 8] = [
    "Title",
    "Author",
    "Subject",
    "Keywords",
    "Creator",
    "Producer",
    "CreationDate",
    "ModDate",
];

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("pdf error: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("encrypted documents are not supported yet (saving would strip the encryption)")]
    EncryptedUnsupported,
}

fn info_dict(doc: &Document) -> Option<&Dictionary> {
    match doc.trailer.get(b"Info").ok()? {
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

/// Read /Info into a JSON object: `{"Title": "...", ...}`. Fields that are
/// absent, or whose string value can't be decoded, are omitted. Values that
/// are indirect references (e.g. `/Title 7 0 R`) are resolved before
/// decoding. No /Info at all -> `{}`.
pub fn read_info(doc: &Document) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(info) = info_dict(doc) {
        for &field in FIELDS.iter() {
            if let Ok(obj) = info.get(field.as_bytes()) {
                if let Ok((_, obj)) = doc.dereference(obj) {
                    if let Ok(text) = decode_text_string(obj) {
                        // Only the PDF 2.0 UTF-8-BOM'd encoding (raw bytes
                        // start with `EF BB BF`) leaves a spurious leading
                        // U+FEFF in `text` for us to strip; see `strip_bom`.
                        let is_utf8_bom = obj
                            .as_str()
                            .is_ok_and(|raw| raw.starts_with(b"\xEF\xBB\xBF"));
                        let text = if is_utf8_bom { strip_bom(text) } else { text };
                        map.insert(field.to_string(), serde_json::Value::String(text));
                    }
                }
            }
        }
    }
    serde_json::Value::Object(map)
}

/// Strip a leading U+FEFF left by `decode_text_string` for PDF 2.0's
/// UTF-8-BOM'd text strings (`EF BB BF` is valid UTF-8 for U+FEFF, so
/// `String::from_utf8` doesn't remove it) — unlike the UTF-16BE case, where
/// lopdf strips the `FE FF` encoding-BOM bytes *before* decoding, so any
/// leading U+FEFF left in that decoded string is real content, not a BOM,
/// and must not be stripped. Callers must only invoke this for strings whose
/// raw bytes were confirmed to start with the UTF-8 BOM (`EF BB BF`).
fn strip_bom(text: String) -> String {
    match text.strip_prefix('\u{FEFF}') {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

/// Set the given fields on /Info, creating the dictionary (and linking it
/// from the trailer) if it doesn't exist yet. Fields not present in `fields`
/// are left untouched. Refuses to touch encrypted documents, matching the
/// text-editing paths (saving would silently strip their encryption).
pub fn set_info(doc: &mut Document, fields: &[(&str, &str)]) -> Result<(), MetadataError> {
    crate::replace::reject_encrypted(doc).map_err(|_| MetadataError::EncryptedUnsupported)?;

    let info_id = match doc.trailer.get(b"Info").ok() {
        Some(Object::Reference(id)) => *id,
        // Non-standard (spec requires an indirect reference) but preserve
        // any fields it already has rather than discarding them.
        Some(Object::Dictionary(d)) => {
            let id = doc.add_object(Object::Dictionary(d.clone()));
            doc.trailer.set("Info", Object::Reference(id));
            id
        }
        _ => {
            let id = doc.add_object(Object::Dictionary(dictionary!()));
            doc.trailer.set("Info", Object::Reference(id));
            id
        }
    };

    let dict = doc.get_dictionary_mut(info_id)?;
    for &(key, value) in fields {
        dict.set(key, text_string(value));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::StringFormat;

    /// Build a bare `Document` whose trailer `/Info` is an indirect
    /// reference to a dictionary with the given raw field values.
    fn doc_with_info(fields: &[(&str, Object)]) -> Document {
        let mut doc = Document::new();
        let mut dict = dictionary!();
        for (key, value) in fields {
            dict.set(*key, value.clone());
        }
        let id = doc.add_object(Object::Dictionary(dict));
        doc.trailer.set("Info", Object::Reference(id));
        doc
    }

    #[test]
    fn utf8_bom_is_stripped() {
        // PDF 2.0 UTF-8 text string: EF BB BF (UTF-8 BOM) + "Report".
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice(b"Report");
        let doc = doc_with_info(&[("Title", Object::String(bytes, StringFormat::Literal))]);
        let info = read_info(&doc);
        assert_eq!(info["Title"], "Report");
    }

    #[test]
    fn utf16_leading_feff_is_real_content_and_preserved() {
        // UTF-16BE text string: FE FF (encoding BOM) + U+FEFF + 'R', i.e.
        // content "\u{FEFF}R". lopdf strips the encoding BOM (first FE FF)
        // and decodes the rest, so the result must keep the leading U+FEFF
        // that belongs to the actual content.
        let bytes: Vec<u8> = vec![0xFE, 0xFF, 0xFE, 0xFF, 0x00, 0x52];
        let doc = doc_with_info(&[("Title", Object::String(bytes, StringFormat::Hexadecimal))]);
        let info = read_info(&doc);
        assert_eq!(info["Title"], "\u{FEFF}R");
    }

    #[test]
    fn non_ascii_round_trip_is_exact() {
        let mut doc = doc_with_info(&[]);
        set_info(&mut doc, &[("Title", "报表 Report")]).unwrap();
        let info = read_info(&doc);
        assert_eq!(info["Title"], "报表 Report");
    }

    #[test]
    fn ascii_round_trip_is_exact() {
        let mut doc = doc_with_info(&[]);
        set_info(&mut doc, &[("Title", "Plain ASCII Title")]).unwrap();
        let info = read_info(&doc);
        assert_eq!(info["Title"], "Plain ASCII Title");
    }
}
