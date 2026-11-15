//! Reading a Direction, and applying it to a note.
//!
//! What arrives on a booth is a serialised `direction` model (see
//! `direction.rs` for the envelope). Its `data` is one of four things, and this
//! module is the whole vocabulary this build understands:
//!
//!  * a `D` model — the drawing engine's own direction. `T:0` is an add: each
//!    `i` record points at an `E` element, whose `u` property is an ordinary
//!    unit model (`$text`, `$image`, `$web`, ink).
//!  * a unit model directly — an update to the unit whose id the `target`
//!    names. This is how a text unit's geometry and content are re-sent as it
//!    is edited.
//!  * an `attachsharedirection` — `{ticket: attachment}`, which is how the
//!    bytes behind an image reach everyone else.
//!  * anything else, which is reported rather than guessed at.
//!
//! Where it lands comes from the booth, not from the payload: a booth id is
//! `{pageId}` for an ordinary page, and the *layer* id — `{pageId}_[layer-…]` —
//! for the school layers. A student's own writing is on
//! `{pageId}_[layer-forUser]_{userId}`, which does not exist in the copy the
//! drive hands out: it is created here, on demand.

use serde_json::{json, Value};

use crate::atdoc::{parse_document, ParsedDocument, ParsedModel};
use crate::error::{AppError, AppResult};
use crate::model::{GenericModel, GenericTree};

/// The marker that separates a page id from a layer id in a booth.
const LAYER_MARK: &str = "_[layer-";
const PERSONAL_LAYER_TYPE: &str = "system:personal";
/// The drawing-engine element id a unit arrived under, kept so a later removal
/// can name it.
const ELEMENT_ID_KEY: &str = "$roomElementId";
const COMMON_LAYER_TYPE: &str = "system:common";

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// One handwritten stroke, already folded into the shape the renderer
    /// draws. Ink arrives as a bare `E` element with no unit wrapper — the
    /// element *is* the content — so it lands on the layer's drawing unit
    /// rather than becoming a unit of its own.
    Stroke { id: String, stroke: Value },
    /// A unit to place on a layer, with everything it references.
    Unit {
        unit_id: String,
        /// The unit's own model first, then whatever it points at.
        models: Vec<GenericModel>,
    },
    /// Bytes shared by ticket, for the note's asset store.
    Asset {
        ticket: String,
        mime: String,
        bytes: Vec<u8>,
    },
    /// An element the room says is gone.
    Remove { id: String },
    /// A payload this build does not act on, named so it can be reported.
    Unsupported { kind: String },
}

#[derive(Debug, Clone)]
pub struct Direction {
    pub target: String,
    pub esid: Option<String>,
    pub changes: Vec<Change>,
}

/// Where a booth's contents belong.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub page_id: String,
    /// `None` when the booth is the page itself rather than one of its layers.
    pub layer_id: Option<String>,
    pub layer_type: Option<String>,
}

pub fn placement_of(booth_id: &str) -> Placement {
    match booth_id.split_once(LAYER_MARK) {
        Some((page_id, rest)) => Placement {
            page_id: page_id.to_string(),
            layer_id: Some(booth_id.to_string()),
            layer_type: Some(
                if rest.starts_with("common") {
                    COMMON_LAYER_TYPE
                } else {
                    PERSONAL_LAYER_TYPE
                }
                .to_string(),
            ),
        },
        None => Placement {
            page_id: booth_id.to_string(),
            layer_id: None,
            layer_type: None,
        },
    }
}

/// Makes sure the layer named by `booth_id` exists, and is the one being drawn
/// on when it is this user's own.
///
/// The room creates it as soon as there is something on it, but a note nobody
/// has written on yet arrives without one — and a stroke drawn anywhere else
/// is on a layer the room has no booth for, so it could never be sent. This is
/// what gives a first stroke somewhere to go.
pub fn ensure_booth_layer(tree: &mut GenericTree, booth_id: &str) -> bool {
    let place = placement_of(booth_id);
    if place.layer_id.is_none() {
        return false;
    }
    let Some(page_id) = find_by_prop(tree, "$page", "pageId", &place.page_id) else {
        return false;
    };
    ensure_layer(tree, &page_id, &place).is_some()
}

pub fn decode(payload: &[u8]) -> AppResult<Direction> {
    let doc = parse_document(payload)?;
    let root = doc
        .models
        .get(&(doc.root_index.max(0) as usize))
        .ok_or_else(|| AppError::other("Direction にルートモデルがありません"))?;

    let target = root
        .props
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let esid = root
        .props
        .get("esid")
        .and_then(Value::as_str)
        .map(str::to_string);

    let Some(data) = root.props.get("data") else {
        return Ok(Direction {
            target,
            esid,
            changes: vec![],
        });
    };

    // A scalar payload carries no models, so there is nothing to place.
    let Some(index) = data.get("$ref").and_then(Value::as_u64) else {
        return Ok(Direction {
            target,
            esid,
            changes: vec![Change::Unsupported {
                kind: "scalar".into(),
            }],
        });
    };

    let payload_model = doc
        .models
        .get(&(index as usize))
        .ok_or_else(|| AppError::other("Direction の data が空です"))?;

    let changes = match payload_model.model_type.as_str() {
        "D" => drawing_changes(&doc, index as usize),
        "attachsharedirection" => attachment_changes(&doc, index as usize),
        kind if kind.starts_with('$') => {
            // The unit model itself: an update to whatever `target` names.
            vec![Change::Unit {
                unit_id: unit_id_of(payload_model, &target),
                models: subtree(&doc, index as usize),
            }]
        }
        other => vec![Change::Unsupported {
            kind: other.to_string(),
        }],
    };

    Ok(Direction {
        target,
        esid,
        changes,
    })
}

/// `DrDirectionType._value` — the enum's own number, which is its ordinal
/// minus one because `NONE` is -1. Only the add is acted on; the rest are
/// named so an unsupported edit can be reported as what it was rather than as
/// a number.
fn direction_type_name(value: i64) -> &'static str {
    match value {
        -1 => "NONE",
        0 => "ADD_REMOVE",
        1 => "GROUP",
        2 => "SELECT",
        3 => "DESELECT",
        4 => "TRANSLATE",
        5 => "RESIZE",
        6 => "SCALE",
        7 => "ROTATE",
        8 => "SET_VARIATION",
        9 => "SET_EXTRA_HANDLE_VARIATION",
        10 => "ERASE",
        11 => "CHANGE_STYLE",
        12 => "CHANGE_ORDER",
        13 => "APPLY_VARIATION",
        14 => "EDIT_EXTRA_HANDLE",
        15 => "SET_CONTENT_PROPERTIES",
        16 => "BIND_UNBIND",
        _ => "unknown",
    }
}

/// `D` — the drawing engine's direction. Only "add" carries content.
fn drawing_changes(doc: &ParsedDocument, index: usize) -> Vec<Change> {
    const ADD: i64 = 0;
    const ERASE: i64 = 10;

    let model = &doc.models[&index];
    let kind = model.props.get("T").and_then(Value::as_i64).unwrap_or(-1);
    if kind == ERASE {
        return erase_changes(doc, index);
    }
    if kind != ADD {
        return vec![Change::Unsupported {
            kind: format!("D {}", direction_type_name(kind)),
        }];
    }

    let mut out = Vec::new();
    for record in doc
        .children
        .get(&(index as i32))
        .cloned()
        .unwrap_or_default()
    {
        let props = &doc.models[&record].props;

        // `t` is the execution type: a removal names the element and nothing
        // else, because the receiver already has it.
        if props.get("t").and_then(Value::as_i64) == Some(super::send::EXECUTION_REMOVE) {
            match props.get("i").and_then(Value::as_str) {
                Some(id) => out.push(Change::Remove { id: id.to_string() }),
                None => out.push(Change::Unsupported {
                    kind: "removal without an element id".into(),
                }),
            }
            continue;
        }

        let Some(element) = props
            .get("m")
            .and_then(|m| m.get("$ref"))
            .and_then(Value::as_u64)
        else {
            out.push(Change::Unsupported {
                kind: "D T=0 without an element".into(),
            });
            continue;
        };
        // `u` on the element is the unit the user actually sees. Ink has none:
        // a stroke element carries its own points and pen, and belongs to the
        // layer's drawing unit.
        let Some(unit) = doc
            .models
            .get(&(element as usize))
            .and_then(|e| e.props.get("u"))
            .and_then(|u| u.get("$ref"))
            .and_then(Value::as_u64)
        else {
            match stroke_change(doc, element as usize) {
                Some(change) => out.push(change),
                None => out.push(Change::Unsupported {
                    kind: "element without a unit".into(),
                }),
            }
            continue;
        };

        let Some(unit_model) = doc.models.get(&(unit as usize)) else {
            continue;
        };
        // The element's own id, used below both as the `$roomElementId` hint
        // and — when the unit itself carries no `unitId` — as the identity
        // `place_unit` keys this unit's replacement by. Without it a unit
        // that has no `unitId` of its own (not every unit does) would fall
        // back to an empty string, and an empty `unit_id` makes `place_unit`'s
        // "replace the previous copy of this unit" cleanup match *every*
        // unit ever placed under this root instead of just this one.
        let element_id = doc.models[&(element as usize)]
            .props
            .get("I")
            .and_then(Value::as_str);
        let mut models = subtree(doc, unit as usize);
        if let Some(root) = models.first_mut() {
            if let (Value::Object(props), Some(id)) = (&mut root.props, element_id) {
                props.insert(ELEMENT_ID_KEY.into(), Value::String(id.to_string()));
            }
        }
        out.push(Change::Unit {
            unit_id: unit_id_of(unit_model, element_id.unwrap_or_default()),
            models,
        });
    }
    out
}

/// Folds one stroke element into the shape the renderer draws, reusing the
/// same conversion the importer applies to a document's own ink.
fn stroke_change(doc: &ParsedDocument, index: usize) -> Option<Change> {
    let model = doc.models.get(&index)?;
    if !crate::atdoc::ink::is_stroke_element(&model.props) {
        return None;
    }
    let styles = crate::atdoc::ink::collect_pen_styles(doc);
    let id = model
        .props
        .get("I")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("e{index}"));
    let stroke = crate::atdoc::ink::build_stroke(doc, index, &styles, &|_| id.clone())?;
    Some(Change::Stroke { id, stroke })
}

/// The eraser. Every element a record names is taken out.
///
/// The original erases *ranges*: a record carries `r`, the fraction of the
/// stroke rubbed out, and the drawing engine keeps whatever is left as new
/// fragment elements. This app has no partial stroke to keep, so a stroke
/// touched by the eraser goes entirely. It shows a little less than the
/// original would; the alternative is showing ink the user rubbed out, which
/// is what happened while this direction was ignored — erased strokes came
/// back on every download.
fn erase_changes(doc: &ParsedDocument, index: usize) -> Vec<Change> {
    let mut out = Vec::new();
    for record in doc
        .children
        .get(&(index as i32))
        .cloned()
        .unwrap_or_default()
    {
        let props = &doc.models[&record].props;
        // Both the element erased and, where the engine split one, the
        // fragment it named: neither survives here.
        for key in ["i", "b"] {
            if let Some(id) = props.get(key).and_then(Value::as_str) {
                let change = Change::Remove { id: id.to_string() };
                if !out.contains(&change) {
                    out.push(change);
                }
            }
        }
    }
    if out.is_empty() {
        out.push(Change::Unsupported {
            kind: "D ERASE without an element id".into(),
        });
    }
    out
}

fn attachment_changes(doc: &ParsedDocument, index: usize) -> Vec<Change> {
    use base64::Engine as _;

    let Value::Object(map) = &doc.models[&index].props else {
        return Vec::new();
    };
    // `t` holds the map of ticket to attachment; older payloads put the
    // tickets at the top level.
    let table = match map.get("t") {
        Some(Value::Object(inner)) => inner,
        _ => map,
    };

    let mut out = Vec::new();
    for (ticket, target) in table {
        let Some(model) = target
            .get("$ref")
            .and_then(Value::as_u64)
            .and_then(|i| doc.models.get(&(i as usize)))
        else {
            continue;
        };
        let Some(blob) = model.props.get("$blob") else {
            continue;
        };
        let Some(Value::String(payload)) = blob.get("$blob") else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(payload) else {
            continue;
        };
        let mime = blob
            .get("$mime")
            .and_then(Value::as_str)
            .or_else(|| model.props.get("$mimetype").and_then(Value::as_str))
            .unwrap_or("application/octet-stream")
            .to_string();
        out.push(Change::Asset {
            ticket: ticket.clone(),
            mime,
            bytes,
        });
    }
    out.sort_by(|a, b| match (a, b) {
        (Change::Asset { ticket: x, .. }, Change::Asset { ticket: y, .. }) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    out
}

fn unit_id_of(model: &ParsedModel, fallback: &str) -> String {
    model
        .props
        .get("unitId")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

/// The model at `index` and everything reachable from it, root first.
///
/// Ids are placeholders (`#index`) here; `apply` rewrites them to ids that are
/// stable for this unit, so applying the same direction twice replaces rather
/// than duplicates.
fn subtree(doc: &ParsedDocument, index: usize) -> Vec<GenericModel> {
    let mut order = vec![index];
    let mut seen: std::collections::HashSet<usize> = [index].into_iter().collect();
    let mut queue = vec![index];

    while let Some(current) = queue.pop() {
        let Some(model) = doc.models.get(&current) else {
            continue;
        };
        let mut next: Vec<usize> = doc
            .children
            .get(&(current as i32))
            .cloned()
            .unwrap_or_default();
        collect_refs(&model.props, &mut next);
        for target in next {
            if doc.models.contains_key(&target) && seen.insert(target) {
                order.push(target);
                queue.push(target);
            }
        }
    }

    order
        .iter()
        .map(|i| {
            let model = &doc.models[i];
            // References are table indices on the wire; name them the way the
            // ids here are named, so one rewrite in `place_unit` covers both.
            let mut props = model.props.clone();
            number_refs_to_names(&mut props);
            // The same second encoding a document's models carry: kept
            // verbatim, and read where this build knows how.
            if !model.tail.is_empty() {
                if let Value::Object(map) = &mut props {
                    use base64::Engine as _;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&model.tail);
                    crate::atdoc::note_tail(map, encoded);
                    if model.model_type == "$text" {
                        crate::atdoc::text::apply_to_props(map, &model.tail);
                    }
                }
            }
            GenericModel {
                id: format!("#{i}"),
                parent_id: (model.parent >= 0 && seen.contains(&(model.parent as usize)))
                    .then(|| format!("#{}", model.parent)),
                model_type: model.model_type.clone(),
                props,
                children: doc
                    .children
                    .get(&(*i as i32))
                    .map(|kids| kids.iter().map(|k| format!("#{k}")).collect())
                    .unwrap_or_default(),
            }
        })
        .collect()
}

fn number_refs_to_names(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Number(n)) = map.get("$ref") {
                let index = n.as_u64().unwrap_or(0);
                map.insert("$ref".into(), Value::String(format!("#{index}")));
                return;
            }
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if let Some(child) = map.get_mut(&key) {
                    number_refs_to_names(child);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                number_refs_to_names(v);
            }
        }
        _ => {}
    }
}

fn collect_refs(value: &Value, out: &mut Vec<usize>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Number(n)) = map.get("$ref") {
                if let Some(i) = n.as_u64() {
                    out.push(i as usize);
                }
                return;
            }
            for v in map.values() {
                collect_refs(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_refs(v, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub units: usize,
    pub strokes: usize,
    pub removed: usize,
    /// Strokes now in the note, by the id the room knows them by, with the
    /// layer they are on. The caller records these so a later erase can name
    /// them.
    pub stroke_ids: Vec<(String, String)>,
    pub assets: Vec<(String, String, Vec<u8>)>,
    pub unsupported: Vec<String>,
}

/// Puts a direction's changes into `tree`.
///
/// Idempotent: a unit is keyed by its own id, so replaying a booth from the
/// beginning — which is what a reconnect does — rewrites the same models
/// instead of stacking copies.
/// `aliases` pairs the room's element ids with the note's own stroke ids, for
/// the strokes this app sent: their local id is not the id the room knows them
/// by, so a removal naming one would otherwise find nothing to take out.
pub fn apply(
    tree: &mut GenericTree,
    booth_id: &str,
    direction: &Direction,
    aliases: &std::collections::HashMap<String, String>,
) -> Applied {
    let mut applied = Applied::default();
    let place = placement_of(booth_id);

    for change in &direction.changes {
        match change {
            Change::Asset {
                ticket,
                mime,
                bytes,
            } => applied
                .assets
                .push((ticket.clone(), mime.clone(), bytes.clone())),
            Change::Unsupported { kind } => {
                log::warn!("{booth_id}: 未対応の payload を破棄しました ({kind})");
                applied.unsupported.push(kind.clone());
            }
            Change::Unit { unit_id, models } => {
                if place_unit(tree, &place, unit_id, models) {
                    applied.units += 1;
                }
            }
            Change::Stroke { id, stroke } => {
                if place_stroke(tree, &place, id, stroke) {
                    applied.strokes += 1;
                    if let Some(layer) = place.layer_id.clone() {
                        applied.stroke_ids.push((id.clone(), layer));
                    }
                }
            }
            Change::Remove { id } => {
                let local = aliases.get(id).map(String::as_str);
                if remove_element(tree, id, local) {
                    applied.removed += 1;
                }
            }
        }
    }
    applied
}

fn place_unit(
    tree: &mut GenericTree,
    place: &Placement,
    unit_id: &str,
    models: &[GenericModel],
) -> bool {
    // A unit that is already here keeps its place. An update — a text unit
    // re-sending itself as it is typed into — names only the unit, and the
    // booth it arrives on is the layer's, not the unit's; looking the unit up
    // is both simpler and right in more cases than deriving a home from the
    // booth every time.
    let existing_layer = find_unit(tree, unit_id)
        .and_then(|id| tree.models.get(&id))
        .and_then(|unit| unit.parent_id.clone());

    let layer_id = match existing_layer {
        Some(layer) => layer,
        None => {
            let Some(page_id) = find_by_prop(tree, "$page", "pageId", &place.page_id) else {
                // A booth for a page this note does not have. Better ignored
                // than invented: the page may simply not be downloaded.
                return false;
            };
            match ensure_layer(tree, &page_id, place) {
                Some(layer) => layer,
                None => return false,
            }
        }
    };

    let prefix = format!("{}_r_{}", tree.root_id, sanitise(unit_id));
    let rename = |id: &str| format!("{prefix}{}", id.trim_start_matches('#'));

    // Anything left from a previous copy of this unit goes first, so an update
    // does not leave orphaned sub-models behind. A unit the document itself
    // carried is replaced too — the update is about that unit.
    //
    // Guarded against an empty `unit_id`: `sanitise("")` is itself empty, so
    // an ungated prefix match would degrade to `"{root}_r_"` and catch every
    // unit this function has ever placed under this root, replacing all of
    // them instead of just this one — wiping a note down to whatever the one
    // unsupported direction carried.
    let mut stale: Vec<String> = if unit_id.is_empty() {
        Vec::new()
    } else {
        tree.models
            .keys()
            .filter(|id| id.starts_with(&prefix))
            .cloned()
            .collect()
    };
    stale.extend(find_unit(tree, unit_id));
    for id in &stale {
        detach(tree, id);
    }

    for (position, model) in models.iter().enumerate() {
        let mut props = model.props.clone();
        rewrite_refs(&mut props, &rename);

        let parent_id = match &model.parent_id {
            Some(parent) => Some(rename(parent)),
            // The unit itself hangs off the layer; anything it references but
            // does not parent stays where the original had it — unparented.
            None if position == 0 => Some(layer_id.clone()),
            None => None,
        };

        tree.insert(GenericModel {
            id: rename(&model.id),
            parent_id,
            model_type: model.model_type.clone(),
            props,
            children: Vec::new(),
        });
    }
    true
}

/// Adds one stroke as its own `$draw` unit, creating that unit the first time.
///
/// One unit per stroke, not one shared unit per layer: this is the model the
/// editor's own pen tool writes (`commitStroke` in `canvas/controller.ts`), and
/// what selection and per-stroke move both expect — a stroke sharing a unit
/// with others on its layer can only be selected or moved as part of whatever
/// else landed there first. Keyed by the stroke's own element id, so a
/// replayed booth rewrites the same unit instead of drawing it twice.
fn place_stroke(tree: &mut GenericTree, place: &Placement, id: &str, stroke: &Value) -> bool {
    let Some(page_id) = find_by_prop(tree, "$page", "pageId", &place.page_id) else {
        return false;
    };
    let Some(layer_id) = ensure_layer(tree, &page_id, place) else {
        return false;
    };

    let draw_id = format!("{}_ink_{}", tree.root_id, sanitise(id));

    let mut stroke = stroke.clone();
    if let Value::Object(map) = &mut stroke {
        // A hint for matching a later removal. The note's own record of what
        // the room knows is kept outside the tree — the editor rewrites a
        // stroke's properties from its own model on every save, so nothing put
        // here survives one.
        map.insert(
            super::send::ELEMENT_KEY.into(),
            Value::String(id.to_string()),
        );
    }

    // The renderer culls by frame before it looks at a stroke, so the unit
    // needs one of its own — just this one stroke's bounds, now that nothing
    // else shares it.
    let frame = stroke_bounds(std::slice::from_ref(&stroke));
    let mut props = json!({ "unitId": draw_id, "strokes": [stroke] });
    if let (Value::Object(map), Some((x, y, w, h))) = (&mut props, frame) {
        map.insert("x".into(), json!(x));
        map.insert("y".into(), json!(y));
        map.insert("width".into(), json!(w));
        map.insert("height".into(), json!(h));
    }

    tree.insert(GenericModel {
        id: draw_id,
        parent_id: Some(layer_id),
        model_type: "$draw".into(),
        props,
        children: Vec::new(),
    });
    true
}

/// Takes out whatever the room says is gone: a stroke by the id it is known
/// by, or a unit whose element carried that id.
fn remove_element(tree: &mut GenericTree, element_id: &str, local_id: Option<&str>) -> bool {
    let mut gone = false;

    let draws: Vec<String> = tree
        .models
        .values()
        .filter(|m| m.model_type == "$draw")
        .map(|m| m.id.clone())
        .collect();
    for id in draws {
        let Some(draw) = tree.models.get_mut(&id) else {
            continue;
        };
        let Value::Object(props) = &mut draw.props else {
            continue;
        };
        let Some(Value::Array(strokes)) = props.get_mut("strokes") else {
            continue;
        };
        let before = strokes.len();
        strokes.retain(|stroke| {
            let id = stroke.get("id").and_then(Value::as_str);
            stroke.get(super::send::ELEMENT_KEY).and_then(Value::as_str) != Some(element_id)
                && id != Some(element_id)
                && (local_id.is_none() || id != local_id)
        });
        if strokes.len() != before {
            gone = true;
        }
    }

    let units: Vec<String> = tree
        .models
        .values()
        .filter(|m| m.props.get(ELEMENT_ID_KEY).and_then(Value::as_str) == Some(element_id))
        .map(|m| m.id.clone())
        .collect();
    for id in units {
        detach(tree, &id);
        gone = true;
    }
    gone
}

fn stroke_bounds(strokes: &[Value]) -> Option<(f64, f64, f64, f64)> {
    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for stroke in strokes {
        let bounds = stroke.get("bounds")?;
        let x = bounds.get("x")?.as_f64()?;
        let y = bounds.get("y")?.as_f64()?;
        let w = bounds.get("width")?.as_f64()?;
        let h = bounds.get("height")?.as_f64()?;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
    }
    min_x
        .is_finite()
        .then_some((min_x, min_y, max_x - min_x, max_y - min_y))
}

/// Finds the layer the booth names, creating it when the note has never seen
/// it — which is the normal case for a personal layer, since the drive's copy
/// of a class note does not contain one.
fn ensure_layer(tree: &mut GenericTree, page_id: &str, place: &Placement) -> Option<String> {
    let Some(layer_id) = place.layer_id.as_deref() else {
        // Page-level booth: the page's own edit layer is where units go.
        return find_layer_by_type(tree, page_id, "system:edit")
            .or_else(|| first_layer(tree, page_id));
    };

    let existing = find_by_prop(tree, "$layer", "layerId", layer_id);
    let id = match existing {
        Some(id) => id,
        None => {
            let id = format!("{}_l_{}", tree.root_id, sanitise(layer_id));
            tree.insert(GenericModel {
                id: id.clone(),
                parent_id: Some(page_id.to_string()),
                model_type: "$layer".into(),
                props: json!({
                    "layerId": layer_id,
                    "layerType": place
                        .layer_type
                        .clone()
                        .unwrap_or_else(|| PERSONAL_LAYER_TYPE.into()),
                    "visible": true,
                }),
                children: Vec::new(),
            });
            id
        }
    };

    // Draw on your own layer. Without this the page's own choice stands, and
    // on a class handout that is a system layer the student is not supposed to
    // be writing on — and one the room has no booth for, so nothing drawn
    // there could ever be sent.
    if place.layer_type.as_deref() == Some(PERSONAL_LAYER_TYPE) {
        if let Some(page) = tree.models.get_mut(page_id) {
            if let Value::Object(props) = &mut page.props {
                props.insert("currentLayer".into(), json!({ "$ref": id }));
                crate::atdoc::mark_injected(props, &["currentLayer"]);
            }
        }
    }
    Some(id)
}

/// A unit already in the note, by the id the original gave it.
fn find_unit(tree: &GenericTree, unit_id: &str) -> Option<String> {
    if unit_id.is_empty() {
        return None;
    }
    tree.models
        .values()
        .find(|m| {
            m.model_type.starts_with('$')
                && m.props.get("unitId").and_then(Value::as_str) == Some(unit_id)
        })
        .map(|m| m.id.clone())
}

fn find_by_prop(tree: &GenericTree, model_type: &str, key: &str, value: &str) -> Option<String> {
    tree.models
        .values()
        .find(|m| {
            m.model_type == model_type && m.props.get(key).and_then(Value::as_str) == Some(value)
        })
        .map(|m| m.id.clone())
}

fn find_layer_by_type(tree: &GenericTree, page_id: &str, layer_type: &str) -> Option<String> {
    let page = tree.models.get(page_id)?;
    page.children
        .iter()
        .filter_map(|id| tree.models.get(id))
        .find(|m| {
            m.model_type == "$layer"
                && m.props.get("layerType").and_then(Value::as_str) == Some(layer_type)
        })
        .map(|m| m.id.clone())
}

fn first_layer(tree: &GenericTree, page_id: &str) -> Option<String> {
    let page = tree.models.get(page_id)?;
    page.children
        .iter()
        .filter_map(|id| tree.models.get(id))
        .find(|m| m.model_type == "$layer")
        .map(|m| m.id.clone())
}

fn detach(tree: &mut GenericTree, id: &str) {
    if let Some(model) = tree.models.remove(id) {
        if let Some(parent) = model.parent_id.and_then(|p| tree.models.get_mut(&p)) {
            parent.children.retain(|c| c != id);
        }
    }
}

/// Model ids end up in file paths and asset keys, so they keep to something
/// tame; the unit id itself stays in the properties untouched.
fn sanitise(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn rewrite_refs(value: &mut Value, rename: &impl Fn(&str) -> String) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("$ref") {
                let renamed = rename(id);
                map.insert("$ref".into(), Value::String(renamed));
                return;
            }
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if let Some(child) = map.get_mut(&key) {
                    rewrite_refs(child, rename);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                rewrite_refs(v, rename);
            }
        }
        _ => {}
    }
}

#[path = "apply/tests.rs"]
#[cfg(test)]
mod tests;
