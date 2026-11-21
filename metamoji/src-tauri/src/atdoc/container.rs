//! The `.atdoc` container: header, block heap, model table, extra table.
//!
//! Follows docs/04, with two corrections found by reading the smali directly:
//!
//!  * §3 has the block header semantics slightly off. `StateData.readData`
//!    seeks `pos + 4` and reads `usedSize` bytes, and `blockSize` is the payload
//!    capacity *excluding* the 8-byte header — so the stride from one block to
//!    the next is `8 + blockSize`.
//!  * §5/§9 leave the preload-property split unresolved. `ModelManager
//!    .isPreloadProperty` hardcodes exactly three keys — `"!type"`, `"!version"`
//!    and `"!stopPropergation"` (the original's own spelling) — and they live in
//!    the `pp` table, not in the model's own block. A model's type is therefore
//!    *not* recoverable from its data block alone; the `pp` and `td` tables are
//!    both required.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::reader::Reader;
use super::value::read_value;
use crate::error::{AppError, AppResult};

pub const MAGIC: &[u8] = b"MMJDrawModels";
pub const HEADER_SIZE: usize = 0x2a;
const MODEL_TABLE_ITEM_SIZE: usize = 28;
const MODEL_TABLE_HEADER_SIZE: usize = 8;
const BLOCK_HEADER_SIZE: usize = 8;
const FREE_BLOCK_SENTINEL: u32 = 0xFFFF_FFFF;
const PRELOAD_ITEM_SIZE: usize = 6;
/// docs/04 §8: `0xFFFF` marks a discarded model-type-dictionary index.
const INVALID_TYPE_INDEX: u16 = 0xFFFF;
pub const NONE_I32: i32 = -1;

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub format_version: u16,
    pub model_table_pos: u64,
    pub undo_table_pos: u64,
    pub extra_table_pos: u64,
}

pub fn parse_header(buf: &[u8]) -> AppResult<Header> {
    if buf.len() < HEADER_SIZE {
        return Err(AppError::Unsupported(
            "file is shorter than an .atdoc header".into(),
        ));
    }
    if !buf.starts_with(MAGIC) {
        return Err(AppError::Unsupported(
            "not an .atdoc document (magic number mismatch)".into(),
        ));
    }

    let mut r = Reader::at(buf, 0x10);
    let format_version = r.u16()?;
    if !(1..=3).contains(&format_version) {
        return Err(AppError::Unsupported(format!(
            "unsupported .atdoc format version {format_version}"
        )));
    }

    Ok(Header {
        format_version,
        model_table_pos: r.u64()?,
        undo_table_pos: r.u64()?,
        extra_table_pos: r.u64()?,
    })
}

// ---------------------------------------------------------------------------
// Block heap
// ---------------------------------------------------------------------------

/// Payload of the block whose header starts at `pos`.
pub fn read_block(buf: &[u8], pos: u64) -> AppResult<&[u8]> {
    if pos == u64::MAX || pos == 0 {
        return Err(AppError::other("block offset is unset"));
    }
    let pos = usize::try_from(pos).map_err(|_| AppError::other("block offset out of range"))?;

    let mut r = Reader::at(buf, pos);
    let block_size = r.u32()?;
    let used_size = r.u32()?;
    if used_size == FREE_BLOCK_SENTINEL {
        return Err(AppError::other("block is on the free list"));
    }

    let used = used_size.min(block_size) as usize;
    let start = pos + BLOCK_HEADER_SIZE;
    buf.get(start..start + used)
        .ok_or_else(|| AppError::other("block payload runs past the end of the file"))
}

// ---------------------------------------------------------------------------
// Model table
// ---------------------------------------------------------------------------

/// One 28-byte slot of the model table (docs/04 §4).
///
/// `last_child` and `prev_sibling` are not read by the importer — walking
/// `first_child`/`next_sibling` is enough — but they are part of the on-disk
/// record and naming them keeps this struct a faithful description of it.
#[derive(Debug, Clone, Copy)]
pub struct ModelTableItem {
    pub data_position: u64,
    pub parent: i32,
    pub first_child: i32,
    #[allow(dead_code)]
    pub last_child: i32,
    pub next_sibling: i32,
    #[allow(dead_code)]
    pub prev_sibling: i32,
}

#[derive(Debug, Clone)]
pub struct ModelTable {
    pub root_index: i32,
    pub boneyard_top: i32,
    pub items: Vec<ModelTableItem>,
}

pub fn parse_model_table(buf: &[u8], pos: u64) -> AppResult<ModelTable> {
    let block = read_block(buf, pos)?;
    if block.len() < MODEL_TABLE_HEADER_SIZE {
        return Err(AppError::other("model table block is too small"));
    }

    let mut r = Reader::new(block);
    let root_index = r.i32()?;
    let boneyard_top = r.i32()?;

    let count = (block.len() - MODEL_TABLE_HEADER_SIZE) / MODEL_TABLE_ITEM_SIZE;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        items.push(ModelTableItem {
            data_position: r.u64()?,
            parent: r.i32()?,
            first_child: r.i32()?,
            last_child: r.i32()?,
            next_sibling: r.i32()?,
            prev_sibling: r.i32()?,
        });
    }

    Ok(ModelTable {
        root_index,
        boneyard_top,
        items,
    })
}

/// Slots on the "boneyard" free list — deleted models, linked through
/// `next_sibling` (docs/04 §4). They must not appear in an imported document.
pub fn boneyard(table: &ModelTable) -> HashSet<i32> {
    let mut set = HashSet::new();
    let mut cursor = table.boneyard_top;
    let mut guard = table.items.len() + 1;
    while cursor != NONE_I32 && guard > 0 {
        if !set.insert(cursor) {
            break; // a cycle in a corrupt file
        }
        let Some(item) = table.items.get(cursor as usize) else {
            break;
        };
        cursor = item.next_sibling;
        guard -= 1;
    }
    set
}

/// Model indices referenced by the undo table (docs/04 §7).
///
/// docs/05 §7 is emphatic that undo records use a different vocabulary from
/// content — `n*`/`o*` new-and-old value pairs — and that mixing the two
/// corrupts the content schema. Excluding these is how we honour that.
pub fn undo_models(buf: &[u8], pos: u64) -> HashSet<i32> {
    let mut set = HashSet::new();
    let Ok(block) = read_block(buf, pos) else {
        return set;
    };
    if block.len() < 4 {
        return set;
    }
    let mut r = Reader::at(block, 4);
    while r.remaining() >= 4 {
        if let Ok(idx) = r.i32() {
            if idx >= 0 {
                set.insert(idx);
            }
        }
    }
    set
}

// ---------------------------------------------------------------------------
// Extra table: model-type dictionary and preload properties
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ExtraTable {
    /// Model type strings, indexed by `typeIndex`.
    pub type_dict: Vec<String>,
    /// Per-model preload properties, indexed by model index.
    pub preload: Vec<PreloadItem>,
    /// The `vi` table: the schema version each model type is current at. The
    /// importer does not consult it, but the writer needs it — a model written
    /// at the wrong version invites the reader to migrate it.
    pub version_info: HashMap<String, u16>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PreloadItem {
    pub type_index: u16,
    /// Model schema version. The importer does not migrate on it, but the
    /// writer has to put it back.
    pub version: u16,
}

impl ExtraTable {
    pub fn model_type(&self, index: usize) -> Option<&str> {
        let item = self.preload.get(index)?;
        if item.type_index == INVALID_TYPE_INDEX {
            return None; // tombstoned slot
        }
        self.type_dict
            .get(item.type_index as usize)
            .map(|s| s.as_str())
    }
}

/// docs/04 §6: a plain concatenation of NUL-terminated UTF-8 strings with no
/// count prefix; `typeIndex` is the 0-based position of first appearance.
fn parse_type_dict(block: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in block.iter().enumerate() {
        if *b == 0 {
            out.push(String::from_utf8_lossy(&block[start..i]).into_owned());
            start = i + 1;
        }
    }
    if start < block.len() {
        out.push(String::from_utf8_lossy(&block[start..]).into_owned());
    }
    out
}

/// Fixed 6-byte records: `typeIndex u16`, two flag bytes, `version u16`.
fn parse_preload_table(block: &[u8]) -> AppResult<Vec<PreloadItem>> {
    let count = block.len() / PRELOAD_ITEM_SIZE;
    let mut r = Reader::new(block);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let type_index = r.u16()?;
        r.skip(2)?; // flagsS1, flagsV1
        let version = r.u16()?;
        out.push(PreloadItem {
            type_index,
            version,
        });
    }
    Ok(out)
}

pub fn parse_extra_table(buf: &[u8], header: &Header) -> AppResult<ExtraTable> {
    match header.format_version {
        1 | 2 => parse_extra_table_v1_v2(buf, header),
        _ => parse_extra_table_v3(buf, header),
    }
}

/// V1/V2 point `extraTablePos` at the preload-property table itself, prefixed by
/// a small header holding the dictionary positions (docs/04 §6). V1's header is
/// 8 bytes (type dictionary only); V2's is 16 (type dictionary + version info).
fn parse_extra_table_v1_v2(buf: &[u8], header: &Header) -> AppResult<ExtraTable> {
    let block = read_block(buf, header.extra_table_pos)?;
    let header_len = if header.format_version == 1 { 8 } else { 16 };
    if block.len() < header_len {
        return Err(AppError::other("extra table block is too small"));
    }

    let mut r = Reader::new(block);
    let type_dict_pos = r.u64()?;
    let type_dict = read_block(buf, type_dict_pos)
        .map(parse_type_dict)
        .unwrap_or_default();

    let version_info = if header.format_version >= 2 {
        let mut r = Reader::at(block, 8);
        r.u64()
            .ok()
            .map(|pos| read_version_info(buf, pos))
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    let preload = parse_preload_table(&block[header_len..])?;
    Ok(ExtraTable {
        type_dict,
        preload,
        version_info,
    })
}

/// V3 replaces the fixed header with a `PlainValueSerializer` map keyed
/// `"pp"`/`"td"`/`"vi"`/`"ci"`, whose values are two-element lists holding the
/// low and high 32 bits of a file offset (docs/04 §6).
fn parse_extra_table_v3(buf: &[u8], header: &Header) -> AppResult<ExtraTable> {
    let block = read_block(buf, header.extra_table_pos)?;
    let mut r = Reader::new(block);
    let map = read_value(&mut r)?;

    let offset_of = |key: &str| -> Option<u64> {
        let entry = map.get(key)?;
        let arr = entry.as_array()?;
        let lo = arr.first()?.as_i64()? as u64 & 0xFFFF_FFFF;
        let hi = arr.get(1).and_then(Value::as_i64).unwrap_or(0) as u64 & 0xFFFF_FFFF;
        Some((hi << 32) | lo)
    };

    let type_dict = offset_of("td")
        .and_then(|pos| read_block(buf, pos).ok())
        .map(parse_type_dict)
        .unwrap_or_default();

    // In V3 the preload table is headerless — items start at offset 0.
    let preload = match offset_of("pp").and_then(|pos| read_block(buf, pos).ok()) {
        Some(block) => parse_preload_table(block)?,
        None => Vec::new(),
    };

    Ok(ExtraTable {
        type_dict,
        preload,
        version_info: offset_of("vi")
            .map(|pos| read_version_info(buf, pos))
            .unwrap_or_default(),
    })
}

/// `{modelType: {version}}`. The version rides in a one-member set, so this
/// takes the first number it finds rather than assuming a shape.
fn read_version_info(buf: &[u8], pos: u64) -> HashMap<String, u16> {
    let Ok(block) = read_block(buf, pos) else {
        return HashMap::new();
    };
    let mut r = Reader::new(block);
    let Ok(Value::Object(map)) = read_value(&mut r) else {
        return HashMap::new();
    };

    let mut out = HashMap::new();
    for (name, value) in map {
        let version = match &value {
            Value::Number(n) => n.as_u64(),
            Value::Array(items) => items.first().and_then(Value::as_u64),
            _ => None,
        };
        if let Some(version) = version {
            out.insert(name, version as u16);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Whole-document parse
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ParsedModel {
    pub index: usize,
    pub model_type: String,
    /// Whatever follows the property map in the model's block.
    ///
    /// A text unit keeps its body here, in a second encoding — see `text.rs`.
    /// Held verbatim rather than parsed: it has to go back out unchanged when
    /// the document is written, whether or not this build understands it.
    pub tail: Vec<u8>,
    /// The schema version this model was written at.
    pub version: u16,
    pub parent: i32,
    pub props: Value,
    /// Set when the property block would not decode; the model still occupies
    /// its place in the tree so the document's structure survives.
    pub decode_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedDocument {
    pub format_version: u16,
    /// Schema version per model type, from the `vi` table.
    pub version_info: HashMap<String, u16>,
    pub root_index: i32,
    pub models: HashMap<usize, ParsedModel>,
    /// Child order per parent index, taken from the sibling links.
    pub children: HashMap<i32, Vec<usize>>,
}

pub fn parse_document(buf: &[u8]) -> AppResult<ParsedDocument> {
    let header = parse_header(buf)?;
    let table = parse_model_table(buf, header.model_table_pos)?;
    let extra = parse_extra_table(buf, &header).unwrap_or_default();
    let dead = boneyard(&table);
    let undo = undo_models(buf, header.undo_table_pos);

    let mut models: HashMap<usize, ParsedModel> = HashMap::new();

    for (index, item) in table.items.iter().enumerate() {
        let i = index as i32;
        if dead.contains(&i) || undo.contains(&i) {
            continue;
        }
        // A live slot may have no data block at all: the drawing engine
        // allocates its element managers (`M`) up front and only writes them
        // once there is ink. They are still models, and still referenced by
        // `$ref` from the `$draw` unit that owns them — dropping them leaves a
        // dangling reference that nothing downstream can resolve.
        let unallocated = item.data_position == u64::MAX || item.data_position == 0;

        let model_type = extra
            .model_type(index)
            .map(str::to_string)
            .unwrap_or_else(|| "$dummy".to_string());

        let (props, tail, decode_error) = if unallocated {
            (Value::Object(Default::default()), Vec::new(), None)
        } else {
            match read_block(buf, item.data_position) {
                Ok(payload) => {
                    let mut r = Reader::new(payload);
                    match read_value(&mut r) {
                        Ok(value) => (value, payload[r.position()..].to_vec(), None),
                        Err(err) => (
                            Value::Object(Default::default()),
                            Vec::new(),
                            Some(err.to_string()),
                        ),
                    }
                }
                Err(err) => (
                    Value::Object(Default::default()),
                    Vec::new(),
                    Some(err.to_string()),
                ),
            }
        };

        models.insert(
            index,
            ParsedModel {
                index,
                model_type,
                version: extra.preload.get(index).map(|p| p.version).unwrap_or(0),
                tail,
                parent: item.parent,
                props,
                decode_error,
            },
        );
    }

    // Child order comes from the first_child / next_sibling chain, which is the
    // document order; the flat table order is allocation order and unrelated.
    let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
    for (index, item) in table.items.iter().enumerate() {
        if !models.contains_key(&index) && index as i32 != table.root_index {
            continue;
        }
        let mut cursor = item.first_child;
        let mut guard = table.items.len() + 1;
        let mut ordered = Vec::new();
        while cursor != NONE_I32 && guard > 0 {
            if models.contains_key(&(cursor as usize)) {
                ordered.push(cursor as usize);
            }
            let Some(child) = table.items.get(cursor as usize) else {
                break;
            };
            cursor = child.next_sibling;
            guard -= 1;
        }
        if !ordered.is_empty() {
            children.insert(index as i32, ordered);
        }
    }

    Ok(ParsedDocument {
        format_version: header.format_version,
        version_info: extra.version_info.clone(),
        root_index: table.root_index,
        models,
        children,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(format_version: u16, model: u64, undo: u64, extra: u64) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[..MAGIC.len()].copy_from_slice(MAGIC);
        buf[MAGIC.len()] = 1;
        buf[0x0E..0x10].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        buf[0x10..0x12].copy_from_slice(&format_version.to_le_bytes());
        buf[0x12..0x1A].copy_from_slice(&model.to_le_bytes());
        buf[0x1A..0x22].copy_from_slice(&undo.to_le_bytes());
        buf[0x22..0x2A].copy_from_slice(&extra.to_le_bytes());
        buf
    }

    #[test]
    fn rejects_a_file_without_the_magic_number() {
        assert!(parse_header(&[0u8; 128]).is_err());
    }

    #[test]
    fn rejects_an_unsupported_format_version() {
        assert!(parse_header(&header_bytes(9, 0, 0, 0)).is_err());
    }

    #[test]
    fn reads_the_table_offsets_from_the_header() {
        let h = parse_header(&header_bytes(3, 0x100, 0x200, 0x300)).unwrap();
        assert_eq!(h.format_version, 3);
        assert_eq!(h.model_table_pos, 0x100);
        assert_eq!(h.undo_table_pos, 0x200);
        assert_eq!(h.extra_table_pos, 0x300);
    }

    #[test]
    fn a_free_block_is_refused() {
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&64u32.to_le_bytes());
        buf[4..8].copy_from_slice(&FREE_BLOCK_SENTINEL.to_le_bytes());
        assert!(read_block(&buf, 0).is_err());
    }

    #[test]
    fn a_block_yields_only_its_used_bytes() {
        // Offset 0 is reserved: the 42-byte header sits there, so `read_block`
        // treats 0 as "unset". Put the block after it, as a real file would.
        let mut buf = vec![0u8; HEADER_SIZE];
        let pos = buf.len() as u64;
        buf.extend_from_slice(&64u32.to_le_bytes()); // capacity
        buf.extend_from_slice(&3u32.to_le_bytes()); // used
        buf.extend_from_slice(&[1, 2, 3, 9, 9, 9]);
        assert_eq!(read_block(&buf, pos).unwrap(), &[1, 2, 3]);
    }

    #[test]
    fn offset_zero_is_treated_as_unset() {
        let buf = vec![0u8; 64];
        assert!(read_block(&buf, 0).is_err());
        assert!(read_block(&buf, u64::MAX).is_err());
    }

    #[test]
    fn parses_the_type_dictionary() {
        assert_eq!(
            parse_type_dict(b"$page\0$layer\0$draw\0"),
            vec!["$page", "$layer", "$draw"]
        );
    }

    #[test]
    fn preload_items_resolve_a_model_type_through_the_dictionary() {
        let mut block = Vec::new();
        block.extend_from_slice(&1u16.to_le_bytes()); // typeIndex -> "$layer"
        block.extend_from_slice(&[0, 0]);
        block.extend_from_slice(&2u16.to_le_bytes()); // version

        let extra = ExtraTable {
            version_info: HashMap::new(),
            type_dict: vec!["$page".into(), "$layer".into()],
            preload: parse_preload_table(&block).unwrap(),
        };
        assert_eq!(extra.model_type(0), Some("$layer"));
        assert_eq!(extra.preload[0].version, 2);
    }

    #[test]
    fn a_tombstoned_type_index_resolves_to_no_type() {
        let mut block = Vec::new();
        block.extend_from_slice(&INVALID_TYPE_INDEX.to_le_bytes());
        block.extend_from_slice(&[0, 0, 0, 0]);
        let extra = ExtraTable {
            version_info: HashMap::new(),
            type_dict: vec!["$page".into()],
            preload: parse_preload_table(&block).unwrap(),
        };
        assert_eq!(extra.model_type(0), None);
    }

    #[test]
    fn the_boneyard_walk_terminates_on_a_cycle() {
        let table = ModelTable {
            root_index: 0,
            boneyard_top: 0,
            items: vec![ModelTableItem {
                data_position: 0,
                parent: -1,
                first_child: -1,
                last_child: -1,
                next_sibling: 0, // points at itself
                prev_sibling: -1,
            }],
        };
        assert_eq!(boneyard(&table), HashSet::from([0]));
    }
}
