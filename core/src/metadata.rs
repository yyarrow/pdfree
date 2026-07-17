//! Document information dictionary: read/write trailer /Info (ISO 32000-1
//! 14.3.3, Table 317). String encoding (PDFDocEncoding vs UTF-16BE with a
//! FEFF BOM) is handled by lopdf's own `text_string`/`decode_text_string`
//! helpers (MIT, crate root re-exports), which already implement the
//! encode/decode rules called for here.

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
/// absent, or whose string value can't be decoded, are omitted. No /Info at
/// all -> `{}`.
pub fn read_info(doc: &Document) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(info) = info_dict(doc) {
        for &field in FIELDS.iter() {
            if let Ok(obj) = info.get(field.as_bytes()) {
                if let Ok(text) = decode_text_string(obj) {
                    map.insert(field.to_string(), serde_json::Value::String(text));
                }
            }
        }
    }
    serde_json::Value::Object(map)
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
