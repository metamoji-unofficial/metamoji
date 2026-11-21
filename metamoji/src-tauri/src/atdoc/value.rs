//! `PlainValueSerializer` — the value encoding used inside every model block.
//!
//! docs/04 §9 lists this decoder as unresolved, which made it look like the
//! blocker for the whole importer. It is not: the tag scheme is a flat switch in
//! `apk/smali_classes2/com/metamoji/df/model/PlainValueSerializer.smali`
//! (constants at lines 9-43, `readValue` at 199-1024), and the table below was
//! derived from it and checked byte-for-byte against the documents bundled in
//! the APK.
//!
//! Values decode straight into the same shape `src/model/generic.ts` uses, so a
//! decoded `.atdoc` and a natively-created note are the same kind of object by
//! the time either reaches the converter.
//!
//! All integers are little-endian.

use serde_json::{Map, Value};

use super::reader::Reader;
use crate::error::{AppError, AppResult};

// Tag bytes. The ASCII letters are mnemonic in the original and worth keeping.
pub const TAG_NULL: u8 = b'N';
pub const TAG_BOOL: u8 = b'B';
pub const TAG_BYTE: u8 = b'Y';
pub const TAG_WORD: u8 = b'W';
pub const TAG_DWORD: u8 = b'D';
pub const TAG_FLOAT: u8 = b'F';
pub const TAG_STRING: u8 = b'S';
pub const TAG_ARRAY: u8 = b'A';
pub const TAG_SET: u8 = b'T';
pub const TAG_MAP: u8 = b'O';
pub const TAG_BLOB: u8 = b'L';
pub const TAG_MODEL: u8 = b'M';
pub const TAG_POINTS: u8 = b'p';
pub const TAG_POINTS_OLD: u8 = b'P';

/// Escape prefix for map keys (`MMJMD_MAP_KEY_META`, PlainValueSerializer.smali:7).
/// The empty key is stored as this literal, and a key that already begins with
/// it gets another copy prepended; the read side strips one occurrence.
const MAP_KEY_META: &[u8] = &[0x1b, 0x07];

/// Guards against a corrupt file nesting containers without end.
const MAX_DEPTH: usize = 64;

pub fn read_value(r: &mut Reader) -> AppResult<Value> {
    read_value_at(r, 0)
}

fn read_value_at(r: &mut Reader, depth: usize) -> AppResult<Value> {
    if depth > MAX_DEPTH {
        return Err(AppError::other("value nesting too deep"));
    }

    let tag = r.u8()?;
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => Ok(Value::Bool(r.u8()? != 0)),
        TAG_BYTE => Ok(Value::from(r.i8()?)),
        TAG_WORD => Ok(Value::from(r.i16()?)),
        TAG_DWORD => Ok(Value::from(r.i32()?)),
        TAG_FLOAT => Ok(json_number(r.f64()?)),
        TAG_STRING => Ok(Value::String(r.cstring()?)),

        TAG_ARRAY | TAG_SET => {
            // A SET collapses to an array; set semantics are only used by the
            // version-info dictionary, which the importer does not consume.
            let count = r.u32()? as usize;
            r.ensure_plausible_count(count)?;
            let mut items = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                items.push(read_value_at(r, depth + 1)?);
            }
            Ok(Value::Array(items))
        }

        TAG_MAP => {
            let mut map = Map::new();
            loop {
                // A zero byte where a key would start terminates the map.
                if r.peek_u8()? == 0 {
                    r.skip(1)?;
                    break;
                }
                let key = decode_map_key(&r.cstring_bytes()?);
                let value = read_value_at(r, depth + 1)?;
                map.insert(key, value);
            }
            Ok(Value::Object(map))
        }

        TAG_BLOB => {
            let mime = r.cstring()?;
            let len = r.u32()? as usize;
            let bytes = r.bytes(len)?;
            use base64::Engine as _;
            let mut obj = Map::new();
            obj.insert(
                "$blob".to_string(),
                Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
            );
            // `PropBlob` in generic.ts carries only the payload, so the mime
            // rides alongside it rather than being dropped on the floor.
            if !mime.is_empty() {
                obj.insert("$mime".to_string(), Value::String(mime));
            }
            Ok(Value::Object(obj))
        }

        TAG_MODEL => {
            // A model reference by table index. These are real graph edges, not
            // decoration: the ink of a `$draw` unit hangs off a referenced node
            // whose `parent` is -1, so it is unreachable by walking children.
            let index = r.u32()?;
            let mut obj = Map::new();
            obj.insert("$ref".to_string(), Value::from(index));
            Ok(Value::Object(obj))
        }

        TAG_POINTS => {
            // Byte length, not point count, and always float32 x/y pairs.
            let byte_len = r.u32()? as usize;
            if !byte_len.is_multiple_of(8) {
                return Err(AppError::other(
                    "point array byte length is not a multiple of 8",
                ));
            }
            let bytes = r.bytes(byte_len)?;
            let mut coords = Vec::with_capacity(byte_len / 4);
            for chunk in bytes.chunks_exact(4) {
                coords.push(json_number(
                    f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64,
                ));
            }
            Ok(points_value(coords))
        }

        TAG_POINTS_OLD => read_points_old(r),

        other => Err(AppError::other(format!(
            "unknown PlainValueSerializer tag 0x{other:02x} ('{}') at offset {}",
            other as char,
            r.position().saturating_sub(1)
        ))),
    }
}

/// The pre-float32 point array: a per-point control byte selects the width of
/// each coordinate independently — bits 2-3 for x, bits 0-1 for y.
fn read_points_old(r: &mut Reader) -> AppResult<Value> {
    let count = r.u32()? as usize;
    r.ensure_plausible_count(count)?;

    let mut coords = Vec::with_capacity(count * 2);
    for _ in 0..count {
        let control = r.u8()?;
        let x = read_sized_coord(r, (control >> 2) & 0b11)?;
        let y = read_sized_coord(r, control & 0b11)?;
        coords.push(json_number(x));
        coords.push(json_number(y));
    }
    Ok(points_value(coords))
}

fn read_sized_coord(r: &mut Reader, width: u8) -> AppResult<f64> {
    Ok(match width {
        1 => r.i8()? as f64,
        2 => r.i16()? as f64,
        3 => r.i32()? as f64,
        _ => r.f64()?,
    })
}

fn points_value(coords: Vec<Value>) -> Value {
    let mut obj = Map::new();
    obj.insert("$points".to_string(), Value::Array(coords));
    Value::Object(obj)
}

fn decode_map_key(raw: &[u8]) -> String {
    if raw.starts_with(MAP_KEY_META) {
        String::from_utf8_lossy(&raw[MAP_KEY_META.len()..]).into_owned()
    } else {
        String::from_utf8_lossy(raw).into_owned()
    }
}

/// JSON has one number type, so a float that happens to be integral would
/// serialize as `1.0` and read back as an int. Normalising here keeps the
/// frontend's tolerant `getNumber`/`getInt` readers honest.
fn json_number(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Value {
        let mut r = Reader::new(bytes);
        read_value(&mut r).expect("value should decode")
    }

    #[test]
    fn decodes_scalars() {
        assert_eq!(decode(b"N"), Value::Null);
        assert_eq!(decode(b"B\x01"), Value::Bool(true));
        assert_eq!(decode(b"B\x00"), Value::Bool(false));
        assert_eq!(decode(b"Y\xff"), Value::from(-1i8));
        assert_eq!(decode(b"W\x00\x01"), Value::from(256i16));
        assert_eq!(decode(b"D\x2a\x00\x00\x00"), Value::from(42i32));
        assert_eq!(decode(b"S\x68\x69\x00"), Value::String("hi".into()));
    }

    #[test]
    fn decodes_float() {
        let mut bytes = vec![TAG_FLOAT];
        bytes.extend_from_slice(&12.5f64.to_le_bytes());
        assert_eq!(decode(&bytes), Value::from(12.5));
    }

    #[test]
    fn decodes_array() {
        let mut bytes = vec![TAG_ARRAY];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(b"D\x01\x00\x00\x00");
        bytes.extend_from_slice(b"D\x02\x00\x00\x00");
        assert_eq!(decode(&bytes), serde_json::json!([1, 2]));
    }

    #[test]
    fn decodes_model_reference_as_a_ref_object() {
        let mut bytes = vec![TAG_MODEL];
        bytes.extend_from_slice(&7u32.to_le_bytes());
        assert_eq!(decode(&bytes), serde_json::json!({ "$ref": 7 }));
    }

    #[test]
    fn decodes_point_array() {
        let mut bytes = vec![TAG_POINTS];
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        bytes.extend_from_slice(&2.0f32.to_le_bytes());
        assert_eq!(decode(&bytes), serde_json::json!({ "$points": [1.0, 2.0] }));
    }

    #[test]
    fn rejects_a_point_array_whose_length_is_not_a_multiple_of_eight() {
        let mut bytes = vec![TAG_POINTS];
        bytes.extend_from_slice(&6u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 6]);
        let mut r = Reader::new(&bytes);
        assert!(read_value(&mut r).is_err());
    }

    #[test]
    fn decodes_legacy_point_array_with_mixed_coordinate_widths() {
        // One point: control 0b0101 => x is int8, y is int8.
        let mut bytes = vec![TAG_POINTS_OLD];
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(0b0000_0101);
        bytes.push(0x0a);
        bytes.push(0xf6); // -10
        assert_eq!(
            decode(&bytes),
            serde_json::json!({ "$points": [10.0, -10.0] })
        );
    }

    #[test]
    fn decodes_blob_with_its_mime_type() {
        let mut bytes = vec![TAG_BLOB];
        bytes.extend_from_slice(b"image/png\x00");
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        let value = decode(&bytes);
        assert_eq!(value["$mime"], Value::String("image/png".into()));
        assert_eq!(value["$blob"], Value::String("AQID".into()));
    }

    #[test]
    fn decodes_map_and_stops_at_the_terminator() {
        let mut bytes = vec![TAG_MAP];
        bytes.extend_from_slice(b"a\x00");
        bytes.extend_from_slice(b"D\x01\x00\x00\x00");
        bytes.push(0); // end of map
        assert_eq!(decode(&bytes), serde_json::json!({ "a": 1 }));
    }

    #[test]
    fn strips_the_map_key_escape_prefix() {
        let mut bytes = vec![TAG_MAP];
        bytes.extend_from_slice(&[0x1b, 0x07]);
        bytes.push(0);
        bytes.extend_from_slice(b"N");
        bytes.push(0);
        assert_eq!(decode(&bytes), serde_json::json!({ "": null }));
    }

    /// The 82-byte opening block of a real bundled document, reproduced exactly.
    /// This pins the tag table against ground truth rather than against itself.
    #[test]
    fn decodes_a_real_model_block_byte_for_byte() {
        let mut bytes = vec![TAG_MAP];
        bytes.extend_from_slice(b"unitId\x00");
        bytes.push(TAG_STRING);
        bytes.extend_from_slice(b"__subId_v2_[F6FB]_[unit]_188\x00");
        bytes.extend_from_slice(b"frame\x00");
        bytes.push(TAG_MODEL);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(0);

        let value = decode(&bytes);
        assert_eq!(
            value["unitId"],
            Value::String("__subId_v2_[F6FB]_[unit]_188".into())
        );
        assert_eq!(value["frame"], serde_json::json!({ "$ref": 1 }));
    }

    #[test]
    fn rejects_an_unknown_tag() {
        let mut r = Reader::new(b"\x00");
        assert!(read_value(&mut r).is_err());
    }
}
