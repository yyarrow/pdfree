//! Document information dictionary: read/write trailer /Info (ISO 32000-1
//! 14.3.3, Table 317). String encoding (PDFDocEncoding vs UTF-16BE with a
//! FEFF BOM, and PDF 2.0's UTF-8 with an EF BB BF BOM) is handled by lopdf's
//! own `text_string`/`decode_text_string` helpers (MIT, crate root
//! re-exports). lopdf 0.43's `decode_text_string` leaves the UTF-8 BOM's
//! U+FEFF in the decoded `String` instead of stripping it, so `read_info`
//! strips a leading U+FEFF itself before returning the value (see
//! `strip_bom`). /Info values may also be indirect references to string
//! objects (ISO 32000-1 7.3.4), which `read_info` resolves with
//! `Document::dereference` before decoding.

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
                        map.insert(
                            field.to_string(),
                            serde_json::Value::String(strip_bom(text)),
                        );
                    }
                }
            }
        }
    }
    serde_json::Value::Object(map)
}

/// Strip a leading BOM character that `decode_text_string` may leave in the
/// decoded value. lopdf 0.43 keeps U+FEFF for PDF 2.0's UTF-8-BOM'd text
/// strings (`EF BB BF` is valid UTF-8 for U+FEFF, so `String::from_utf8`
/// doesn't remove it) instead of stripping it like it does for the
/// UTF-16BE case. Stripping any leading U+FEFF here handles that case, and
/// is a harmless no-op for the UTF-16BE case where lopdf already strips the
/// `FE FF` BOM bytes before decoding.
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
