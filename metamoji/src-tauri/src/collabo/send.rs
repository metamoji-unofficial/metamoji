//! Building the Direction that adds one stroke, or places a unit.
//!
//! The mirror of `apply.rs`: where that reads what the room already holds,
//! this produces what goes back. A stroke on the wire is four models —
//!
//! ```text
//! D   the drawing engine's direction   T=ADD_REMOVE, V=2, M=5, O=6, C=id
//!  i    one add record                 i=element id, m=the element, t/s/e
//! E   the element                      T=STROKE, B=POINTS, P=points, p=pen
//! S   the pen style                    C=colour, W=width, A=alpha, P=pen type
//! ```
//!
//! — wrapped in the `direction` envelope from `direction.rs` and posted to the
//! booth of the layer it belongs on, with `target` naming that layer's drawing
//! unit.
//!
//! Ids are the drawing engine's own: `{prefix} {count}`, each in the base-92
//! alphabet `DrUtIdGenerator.CHARSET_TABLE`. They have to be in that shape
//! because the receiver parses them back into a prefix and a count.
//!
//! Strokes are sent as `SIMPLE`, not `REDUCED`. A reduced stroke carries the
//! point-reduction bookkeeping the original's own renderer produced (`d`, `z`,
//! `g`), and this app does not reduce anything — claiming otherwise would be
//! sending numbers that mean nothing.

use serde_json::{json, Value};

use super::direction::{self, DirectionData};
use crate::error::{AppError, AppResult};
use crate::model::{GenericModel, GenericTree};

/// `DrUtIdGenerator.CHARSET_TABLE`, and the base its numbers are written in.
const CHARSET: &[u8; 92] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz\"#$%&'()*+,-./:;<=>@[\\]^_`{|}~";

/// Element types (`DrElementType`) and the rest, all `ordinal - 1`.
const ELEMENT_TYPE_STROKE: i64 = 1;
const BASE_TYPE_POINTS: i64 = 0;
const STROKE_TYPE_SIMPLE: i64 = 0;
/// `DrAddRemoveDirection$DrExecutionType`, a plain ordinal this time.
const EXECUTION_ADD: i64 = 0;
pub(crate) const EXECUTION_REMOVE: i64 = 1;
const DIRECTION_ADD_REMOVE: i64 = 0;
/// What the original sends, and what its receiver checks against.
const MODULE_VERSION: i64 = 5;
const MODULE_ACCEPTABLE_VERSION: i64 = 6;
const DIRECTION_VERSION: i64 = 2;

/// `DrUtIdGenerator`: a per-run prefix and a counter, both base-92.
#[derive(Debug)]
pub struct IdGenerator {
    prefix: u64,
    count: u64,
}

impl IdGenerator {
    pub fn new(prefix: u64) -> Self {
        Self { prefix, count: 0 }
    }

    /// A prefix nobody else is using. The original seeds it from the device and
    /// the clock; what matters is only that two clients do not collide.
    pub fn fresh() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        Self::new(now | 1)
    }

    pub fn next_id(&mut self) -> String {
        self.count += 1;
        format!("{} {}", base92(self.prefix), base92(self.count))
    }
}

/// `encode64bitToString`: base-92, most significant digit first.
fn base92(mut value: u64) -> String {
    let base = CHARSET.len() as u64;
    let mut digits = Vec::new();
    loop {
        digits.push(CHARSET[(value % base) as usize]);
        value /= base;
        if value == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).expect("the alphabet is ASCII")
}

/// Who wrote it. The original stamps every element with this, and it is what
/// a teacher's screen shows next to a student's writing.
#[derive(Debug, Clone)]
pub struct Author {
    pub user_id: String,
    pub name: String,
    pub company_id: String,
    pub room_id: String,
    /// The id the *room* gave us, which is not the tenant's user id.
    pub room_user_id: String,
}

/// Builds the payload that adds `stroke` to `layer_id`'s drawing unit.
///
/// `stroke` is this app's own shape — the one `ink.rs` produces and the
/// renderer draws.
pub fn add_stroke(
    stroke: &Value,
    layer_id: &str,
    ids: &mut IdGenerator,
    author: &Author,
    edit_status_id: Option<&str>,
) -> AppResult<(Vec<u8>, String)> {
    let element_id = ids.next_id();
    let style_id = ids.next_id();
    let collaboration_id = ids.next_id();

    let points =
        wire_points(stroke).ok_or_else(|| AppError::other("ストロークに座標がありません"))?;
    let (bx, by, bw, bh) = bounds_of(stroke, &points)
        .ok_or_else(|| AppError::other("ストロークの範囲を求められません"))?;

    let mut tree = GenericTree::new("d", "D");
    if let Value::Object(props) = &mut tree.models.get_mut("d").unwrap().props {
        props.insert("T".into(), json!(DIRECTION_ADD_REMOVE));
        props.insert("V".into(), json!(DIRECTION_VERSION));
        props.insert("M".into(), json!(MODULE_VERSION));
        props.insert("O".into(), json!(MODULE_ACCEPTABLE_VERSION));
        props.insert("C".into(), json!(collaboration_id));
    }

    let count = points.len() / 2;
    tree.insert(GenericModel {
        id: "i0".into(),
        parent_id: Some("d".into()),
        model_type: "i".into(),
        props: json!({
            "i": element_id,
            "m": { "$ref": "e" },
            "s": 0,
            "e": count.max(1) - 1,
            "t": EXECUTION_ADD,
        }),
        children: Vec::new(),
    });

    tree.insert(detached(GenericModel {
        id: "e".into(),
        parent_id: Some("d".into()),
        model_type: "E".into(),
        props: json!({
            "I": element_id,
            "T": ELEMENT_TYPE_STROKE,
            "B": BASE_TYPE_POINTS,
            "t": STROKE_TYPE_SIMPLE,
            "P": { "$points": points },
            "p": { "$ref": "s" },
            "BX": bx,
            "BY": by,
            "BW": bw,
            "BH": bh,
            "uII": author.user_id,
            "uIN": author.name,
            "uIG": author.company_id,
            "uIR": author.room_id,
            "uIC": author.room_user_id,
            "uIT": now_seconds(),
        }),
        children: Vec::new(),
    }));

    tree.insert(detached(GenericModel {
        id: "s".into(),
        parent_id: Some("d".into()),
        model_type: "S".into(),
        props: json!({
            "I": style_id,
            "C": wire_colour(stroke),
            "W": stroke.get("width").cloned().unwrap_or(json!(2.4)),
            "A": stroke.get("opacity").cloned().unwrap_or(json!(1.0)),
            "P": pen_type_value(stroke),
        }),
        children: Vec::new(),
    }));

    let payload = direction::encode(
        DirectionData::Model(tree),
        &format!("{layer_id}_[unit]_draw"),
        edit_status_id,
        Default::default(),
    )?;
    // The caller has to keep this: a stroke can only be erased later by the id
    // it was sent under.
    Ok((payload, element_id))
}

fn detached(mut model: GenericModel) -> GenericModel {
    if let Value::Object(props) = &mut model.props {
        crate::atdoc::mark_detached(props);
    }
    model
}

/// The element's bounding box.
///
/// A stroke that came from a document carries one, because the importer
/// computed it. A stroke the user just drew does not: `strokeToProp` writes
/// points and pen and nothing else, and the app recomputes the box every time
/// it reads the note. So compute it here too, by the same rule the importer
/// uses — the box is the points grown by the pen's half-width and a little
/// more, which is roughly what the ink covers.
fn bounds_of(stroke: &Value, points: &[Value]) -> Option<(f64, f64, f64, f64)> {
    if let Some(bounds) = stroke.get("bounds") {
        let read = |key: &str| bounds.get(key).and_then(Value::as_f64);
        if let (Some(x), Some(y), Some(w), Some(h)) =
            (read("x"), read("y"), read("width"), read("height"))
        {
            return Some((x, y, w, h));
        }
    }

    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for pair in points.chunks(2) {
        let (Some(x), Some(y)) = (
            pair.first().and_then(Value::as_f64),
            pair.get(1).and_then(Value::as_f64),
        ) else {
            continue;
        };
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    if !min_x.is_finite() {
        return None;
    }

    let pad = stroke.get("width").and_then(Value::as_f64).unwrap_or(2.4) * 1.5;
    Some((
        min_x - pad,
        min_y - pad,
        (max_x - min_x) + pad * 2.0,
        (max_y - min_y) + pad * 2.0,
    ))
}

/// This app stores a point as `x, y, pressure, time`; the wire wants `x, y`.
fn wire_points(stroke: &Value) -> Option<Vec<Value>> {
    let flat = stroke.get("points")?.get("$points")?.as_array()?;
    let mut out = Vec::with_capacity(flat.len() / 2);
    for point in flat.chunks(4) {
        if point.len() < 2 {
            break;
        }
        out.push(point[0].clone());
        out.push(point[1].clone());
    }
    (!out.is_empty()).then_some(out)
}

/// `#rrggbb` back to the bare `RRGGBB` the style holds.
fn wire_colour(stroke: &Value) -> String {
    stroke
        .get("color")
        .and_then(Value::as_str)
        .unwrap_or("#1f1f1f")
        .trim_start_matches('#')
        .to_ascii_uppercase()
}

fn pen_type_value(stroke: &Value) -> i64 {
    match stroke.get("penType").and_then(Value::as_str) {
        Some("marker") | Some("highlighter") => 1,
        Some("fountain") => 2,
        _ => 0,
    }
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// Removes an element the room already has.
///
/// The same `ADD_REMOVE` direction as an add, with the record marked `REMOVE`
/// and carrying only the element's id: the receiver finds the element in its
/// own document (`removeElementWithInternal` calls `getElementById`), so
/// sending a copy of it would be sending back what it already has.
pub fn remove_element(
    element_id: &str,
    layer_id: &str,
    ids: &mut IdGenerator,
    edit_status_id: Option<&str>,
) -> AppResult<Vec<u8>> {
    let mut tree = GenericTree::new("d", "D");
    if let Value::Object(props) = &mut tree.models.get_mut("d").unwrap().props {
        props.insert("T".into(), json!(DIRECTION_ADD_REMOVE));
        props.insert("V".into(), json!(DIRECTION_VERSION));
        props.insert("M".into(), json!(MODULE_VERSION));
        props.insert("O".into(), json!(MODULE_ACCEPTABLE_VERSION));
        props.insert("C".into(), json!(ids.next_id()));
    }
    tree.insert(GenericModel {
        id: "i0".into(),
        parent_id: Some("d".into()),
        model_type: "i".into(),
        props: json!({ "i": element_id, "t": EXECUTION_REMOVE }),
        children: Vec::new(),
    });

    direction::encode(
        DirectionData::Model(tree),
        &format!("{layer_id}_[unit]_draw"),
        edit_status_id,
        Default::default(),
    )
}

/// The id the room knows a stroke by, when the note happens to be carrying it.
///
/// Not to be relied on: the editor rewrites a stroke's properties from its own
/// model on every save (`strokeToProp` writes points and pen and nothing
/// else), so anything added here is gone the next time the user saves. What
/// the room knows lives in the note's `room_strokes` table instead — see
/// `Ledger`. This is a hint for matching an incoming removal, no more.
pub const ELEMENT_KEY: &str = "$roomStrokeId";

/// What the room has been told, keyed by the note's own stroke id.
///
/// The pairing has to live outside the note. The editor owns the document
/// while it is open and rewrites it on every save from its own model, so
/// anything this code writes into a stroke is lost — and a lost marker reads
/// as "the user erased it", which is how a first version of this deleted a
/// note's writing on the next save.
#[derive(Debug, Clone, PartialEq)]
pub struct Ledger {
    /// The note's own id for the stroke, which `strokeToProp` does preserve.
    pub stroke_id: String,
    /// The id the room knows it by.
    pub element_id: String,
    pub layer_id: String,
}

/// One stroke waiting to go out, and where it belongs.
#[derive(Debug, Clone)]
pub struct Pending {
    pub stroke_id: String,
    pub layer_id: String,
    pub stroke: Value,
}

/// Every stroke on a personal layer, with the layer it is on.
///
/// Only personal layers: they are the ones the room has a booth for, and the
/// only place a student is meant to be writing on a distributed note.
fn personal_strokes(tree: &GenericTree) -> Vec<Pending> {
    let mut out = Vec::new();
    for layer in tree.models.values() {
        if layer.model_type != "$layer" {
            continue;
        }
        if layer.props.get("layerType").and_then(Value::as_str) != Some("system:personal") {
            continue;
        }
        let Some(layer_id) = layer.props.get("layerId").and_then(Value::as_str) else {
            continue;
        };

        for child in &layer.children {
            let Some(draw) = tree.models.get(child) else {
                continue;
            };
            if draw.model_type != "$draw" {
                continue;
            }
            let Some(strokes) = draw.props.get("strokes").and_then(Value::as_array) else {
                continue;
            };
            for stroke in strokes {
                let Some(stroke_id) = stroke.get("id").and_then(Value::as_str) else {
                    // Without an id there is no way to tell this stroke from
                    // the next one on the next save, so it cannot be tracked.
                    continue;
                };
                out.push(Pending {
                    stroke_id: stroke_id.to_string(),
                    layer_id: layer_id.to_string(),
                    stroke: stroke.clone(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.stroke_id.cmp(&b.stroke_id));
    out
}

/// The mark of a real booth id. A layer the room addresses is named
/// `{pageId}_[layer-…]`; anything else is a name this app made up.
const BOOTH_MARK: &str = "_[layer-";

/// What has changed since the room was last told.
///
/// `ledger` is what the room has been told so far. A stroke it does not know
/// is new; an entry whose stroke is no longer in the note was erased.
///
/// The third number is strokes that cannot be sent because their layer is not
/// named the way the room names layers — a note taken by a build that replaced
/// those names with its own model ids. There is nothing to do about such a
/// note but take it from the class box again, and saying so beats posting to a
/// channel nobody is listening to.
pub fn changes(tree: &GenericTree, ledger: &[Ledger]) -> (Vec<Pending>, Vec<Ledger>, usize) {
    let all = personal_strokes(tree);
    let stale = all
        .iter()
        .filter(|p| !p.layer_id.contains(BOOTH_MARK))
        .count();
    let here: Vec<Pending> = all
        .into_iter()
        .filter(|p| p.layer_id.contains(BOOTH_MARK))
        .collect();
    let known: std::collections::HashSet<&str> =
        ledger.iter().map(|l| l.stroke_id.as_str()).collect();
    let added: Vec<Pending> = here
        .iter()
        .filter(|p| !known.contains(p.stroke_id.as_str()))
        .cloned()
        .collect();

    // Erased strokes are looked for across the whole note, not just the
    // personal layers: a stroke that moved to another layer has not been
    // erased, and reporting it as such would delete it for everyone.
    let alive = all_stroke_ids(tree);
    let layers = layer_ids(tree);
    let removed: Vec<Ledger> = ledger
        .iter()
        .filter(|l| !alive.contains(&l.stroke_id))
        // And only from a layer the note still has. If the layer itself is
        // missing, the note is not what this expected — a half-loaded tree, a
        // page that did not come down — and "everything on it was erased" is
        // the wrong conclusion to draw from that.
        .filter(|l| layers.contains(&l.layer_id))
        .cloned()
        .collect();
    (added, removed, stale)
}

fn layer_ids(tree: &GenericTree) -> std::collections::HashSet<String> {
    tree.models
        .values()
        .filter(|m| m.model_type == "$layer")
        .filter_map(|m| m.props.get("layerId").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn all_stroke_ids(tree: &GenericTree) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for draw in tree.models.values() {
        if draw.model_type != "$draw" {
            continue;
        }
        for stroke in draw
            .props
            .get("strokes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = stroke.get("id").and_then(Value::as_str) {
                out.insert(id.to_string());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Units — $shape, $text, $form
// ---------------------------------------------------------------------------
//
// A stroke is write-once: it is either on a layer or it has been erased, and
// that is the whole lifecycle `Ledger` above tracks. A shape, text box or
// form is not — it is moved, resized, retyped, recoloured — so telling the
// room about it needs a third state `Ledger` has no room for: the unit is
// still here, but what it holds has changed. `UnitLedger` carries a
// fingerprint for exactly that; `unit_changes` is `changes`'s counterpart,
// three buckets instead of two.
//
// Added: a `D`/`T=0` direction, the same one a stroke goes through
// (`add_unit`) — confirmed by `apply.rs`'s own doc comment, which names
// `$text`/`$image`/`$web`/ink alike as what an element's `u` can point at.
// Changed: no `D`, no element — the unit resends its whole model directly
// (`update_unit`), the other half of what `apply.rs::decode` understands.
// Removed: `remove_element`, unchanged — it already finds a unit by the
// `$roomElementId` hint `apply.rs::drawing_changes` stamps on, not anything
// stroke-specific.

/// The unit types this build will post on a student's behalf. `$bgimage` is
/// scenery, not a live collaboration target — `src/render/hitTest.ts` treats
/// it the same way — and `$image`/`$web` are left for later: this change is
/// scoped to the three kinds the issue was filed about.
const UNIT_TYPES: &[&str] = &["$shape", "$text", "$form"];

/// What the room has been told about one unit, keyed by the note's own id for
/// it — which, unlike a stroke's, is the same id every device that has ever
/// seen this unit agrees on (`converter.ts` sets a unit's `unitId` to its own
/// model id on the way into this build's generic tree).
#[derive(Debug, Clone, PartialEq)]
pub struct UnitLedger {
    pub unit_id: String,
    /// The id the room knows this unit's *placing element* by — present only
    /// because a unit arrived (or was sent) through the `D`/`T=0` add path;
    /// an update carries no element and so cannot mint one.
    pub element_id: String,
    pub layer_id: String,
    /// A hash of the unit's own subtree, taken the last time this was sent or
    /// received. Not cryptographic — it only has to change when the content
    /// does.
    pub fingerprint: String,
}

/// One unit waiting to go out: its own model and everything it references,
/// root first — the same shape `apply.rs::Change::Unit` carries in reverse.
#[derive(Debug, Clone)]
pub struct PendingUnit {
    pub unit_id: String,
    pub layer_id: String,
    pub models: Vec<GenericModel>,
}

/// A stable hash of a unit's whole subtree, so `unit_changes` can tell "still
/// here, unedited" from "still here, but different" without re-sending on
/// every call to find out.
///
/// `serde_json::Value`'s map is a `BTreeMap` in this build (the
/// `preserve_order` feature is off), so two reads of the same row serialise
/// identically; sorting the models by id first makes the hash independent of
/// whatever order `unit_subtree` happened to discover them in.
pub(crate) fn fingerprint(models: &[GenericModel]) -> String {
    use std::hash::{Hash, Hasher};
    let mut ordered: Vec<&GenericModel> = models.iter().collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for model in ordered {
        model.model_type.hash(&mut hasher);
        serde_json::to_string(&model.props)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// The unit at `root_id`, together with everything it references — the mirror
/// image of `apply.rs::subtree`, read from a live tree instead of a decoded
/// one. Same traversal, so the same invariant holds: a model's parent always
/// appears earlier in the result, which is what lets `graft_unit` insert in
/// one pass.
fn unit_subtree(tree: &GenericTree, root_id: &str) -> Vec<GenericModel> {
    let mut order = vec![root_id.to_string()];
    let mut seen: std::collections::HashSet<String> = order.iter().cloned().collect();
    let mut queue = vec![root_id.to_string()];

    while let Some(current) = queue.pop() {
        let Some(model) = tree.models.get(&current) else {
            continue;
        };
        let mut next = model.children.clone();
        collect_string_refs(&model.props, &mut next);
        for target in next {
            if tree.models.contains_key(&target) && seen.insert(target.clone()) {
                order.push(target.clone());
                queue.push(target);
            }
        }
    }

    order
        .into_iter()
        .filter_map(|id| tree.models.get(&id).cloned())
        .collect()
}

fn collect_string_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("$ref") {
                out.push(id.clone());
                return;
            }
            for v in map.values() {
                collect_string_refs(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_string_refs(v, out);
            }
        }
        _ => {}
    }
}

/// Every `$shape`/`$text`/`$form` directly on a personal layer, with the
/// layer it is on. Direct children only, the same restriction `$draw`'s
/// strokes have: a unit nested inside another one is that unit's content, not
/// a booth-addressable thing of its own.
fn personal_units(tree: &GenericTree) -> Vec<PendingUnit> {
    let mut out = Vec::new();
    for layer in tree.models.values() {
        if layer.model_type != "$layer" {
            continue;
        }
        if layer.props.get("layerType").and_then(Value::as_str) != Some("system:personal") {
            continue;
        }
        let Some(layer_id) = layer.props.get("layerId").and_then(Value::as_str) else {
            continue;
        };

        for child_id in &layer.children {
            let Some(unit) = tree.models.get(child_id) else {
                continue;
            };
            if !UNIT_TYPES.contains(&unit.model_type.as_str()) {
                continue;
            }
            let Some(unit_id) = unit.props.get("unitId").and_then(Value::as_str) else {
                // No id of its own, no way to tell it from the next one on the
                // next save — the same reason an id-less stroke is skipped.
                continue;
            };
            out.push(PendingUnit {
                unit_id: unit_id.to_string(),
                layer_id: layer_id.to_string(),
                models: unit_subtree(tree, child_id),
            });
        }
    }
    out.sort_by(|a, b| a.unit_id.cmp(&b.unit_id));
    out
}

/// What has changed about this note's units since the room was last told.
///
/// Three buckets where `changes` has two: a unit with no ledger row is new;
/// one whose fingerprint no longer matches what was last sent has been
/// edited in place; a ledger row with no unit behind it any more — on a layer
/// the note still has — was erased. The fourth number is units that cannot be
/// sent because their layer is not named the way the room names layers, the
/// same situation `changes`'s third return guards.
pub fn unit_changes(
    tree: &GenericTree,
    ledger: &[UnitLedger],
) -> (Vec<PendingUnit>, Vec<PendingUnit>, Vec<UnitLedger>, usize) {
    let all = personal_units(tree);
    let stale = all
        .iter()
        .filter(|p| !p.layer_id.contains(BOOTH_MARK))
        .count();
    let here: Vec<&PendingUnit> = all
        .iter()
        .filter(|p| p.layer_id.contains(BOOTH_MARK))
        .collect();

    let known: std::collections::HashMap<&str, &UnitLedger> =
        ledger.iter().map(|l| (l.unit_id.as_str(), l)).collect();

    let mut added = Vec::new();
    let mut changed = Vec::new();
    for pending in &here {
        match known.get(pending.unit_id.as_str()) {
            None => added.push((*pending).clone()),
            Some(entry) if entry.fingerprint != fingerprint(&pending.models) => {
                changed.push((*pending).clone());
            }
            Some(_) => {}
        }
    }

    // A unit that moved to another layer (a teacher adopting a student's
    // shape onto a shared one, say) has not been erased — look for it across
    // the whole note, not just the personal layers, the same guard `changes`
    // applies to strokes.
    let alive = all_unit_ids(tree);
    let layers = layer_ids(tree);
    let removed: Vec<UnitLedger> = ledger
        .iter()
        .filter(|l| !alive.contains(l.unit_id.as_str()))
        // And only from a layer the note still has — the same reasoning
        // `changes` applies: a half-loaded tree must not read as "the student
        // erased everything on it".
        .filter(|l| layers.contains(&l.layer_id))
        .cloned()
        .collect();

    (added, changed, removed, stale)
}

fn all_unit_ids(tree: &GenericTree) -> std::collections::HashSet<&str> {
    tree.models
        .values()
        .filter(|m| UNIT_TYPES.contains(&m.model_type.as_str()))
        .filter_map(|m| m.props.get("unitId").and_then(Value::as_str))
        .collect()
}

/// Builds the payload that adds a whole unit — `$shape`/`$text`/`$form` — to
/// `layer_id`. The same `D`/`T=0` mechanism `add_stroke` uses, because the
/// wire does not distinguish them: an element's `u` can point at any ordinary
/// unit model, ink included (`apply.rs`'s own doc comment).
///
/// Unlike a stroke's element, this one carries neither a `T` (element type)
/// nor bounds: this build has not captured what the native client writes
/// there for a non-stroke element, and `apply.rs::decode` reads neither —
/// only `u`'s presence tells it this element wraps a unit rather than being
/// one. Confirming the real values needs a wire capture this build does not
/// have.
pub fn add_unit(
    models: &[GenericModel],
    layer_id: &str,
    ids: &mut IdGenerator,
    edit_status_id: Option<&str>,
) -> AppResult<(Vec<u8>, String)> {
    if models.is_empty() {
        return Err(AppError::other("ユニットの本体がありません"));
    }
    let element_id = ids.next_id();
    let collaboration_id = ids.next_id();

    let mut tree = GenericTree::new("d", "D");
    if let Value::Object(props) = &mut tree.models.get_mut("d").unwrap().props {
        props.insert("T".into(), json!(DIRECTION_ADD_REMOVE));
        props.insert("V".into(), json!(DIRECTION_VERSION));
        props.insert("M".into(), json!(MODULE_VERSION));
        props.insert("O".into(), json!(MODULE_ACCEPTABLE_VERSION));
        props.insert("C".into(), json!(collaboration_id));
    }

    tree.insert(GenericModel {
        id: "i0".into(),
        parent_id: Some("d".into()),
        model_type: "i".into(),
        props: json!({ "i": element_id, "m": { "$ref": "e" }, "t": EXECUTION_ADD }),
        children: Vec::new(),
    });

    let unit_root = graft_unit(&mut tree, models);

    tree.insert(detached(GenericModel {
        id: "e".into(),
        parent_id: Some("d".into()),
        model_type: "E".into(),
        props: json!({ "I": element_id, "u": { "$ref": unit_root } }),
        children: Vec::new(),
    }));

    let payload = direction::encode(
        DirectionData::Model(tree),
        &format!("{layer_id}_[unit]_draw"),
        edit_status_id,
        Default::default(),
    )?;
    Ok((payload, element_id))
}

/// Copies a unit's own subtree into `tree` under fresh ids, and returns the id
/// its root landed under. The root goes in detached, parented by reference
/// (the element's `u`) rather than by child link — the same as a stroke's
/// element and pen style in `add_stroke`.
fn graft_unit(tree: &mut GenericTree, models: &[GenericModel]) -> String {
    let renamed: std::collections::HashMap<&str, String> = models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.as_str(), format!("u{i}")))
        .collect();
    let rename = |id: &str| renamed.get(id).cloned().unwrap_or_else(|| id.to_string());

    for (position, model) in models.iter().enumerate() {
        let mut props = model.props.clone();
        rewrite_string_refs(&mut props, &rename);
        if position == 0 {
            if let Value::Object(map) = &mut props {
                crate::atdoc::mark_detached(map);
            }
        }
        tree.insert(GenericModel {
            id: rename(&model.id),
            parent_id: if position == 0 {
                Some("d".to_string())
            } else {
                model.parent_id.as_deref().map(&rename)
            },
            model_type: model.model_type.clone(),
            props,
            children: Vec::new(),
        });
    }
    rename(&models[0].id)
}

fn rewrite_string_refs(value: &mut Value, rename: &impl Fn(&str) -> String) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("$ref") {
                let renamed = rename(id);
                map.insert("$ref".into(), Value::String(renamed));
                return;
            }
            for v in map.values_mut() {
                rewrite_string_refs(v, rename);
            }
        }
        Value::Array(items) => {
            for v in items {
                rewrite_string_refs(v, rename);
            }
        }
        _ => {}
    }
}

/// A direct whole-unit-model Direction: no `D`, no element, the unit resends
/// itself. The other half of `apply.rs::decode`'s vocabulary — "a unit model
/// directly … is how a text unit's geometry and content are re-sent as it is
/// edited" (`apply.rs:10-12`) — and so the natural way to tell the room about
/// an edit to a `$shape`/`$form` too, not only `$text`.
///
/// `target` names the layer the unit belongs to, the booth `apply.rs` expects
/// — not the unit itself: `apply.rs::decode` only falls back to `target` for
/// the unit id when the model carries no `unitId` of its own, and every unit
/// this build sends does (`converter.ts` sets one on the way in).
pub fn update_unit(
    models: &[GenericModel],
    layer_id: &str,
    edit_status_id: Option<&str>,
) -> AppResult<Vec<u8>> {
    let Some(root) = models.first() else {
        return Err(AppError::other("ユニットの本体がありません"));
    };

    let mut subtree = GenericTree {
        root_id: root.id.clone(),
        models: std::collections::HashMap::new(),
    };
    for model in models {
        subtree.models.insert(model.id.clone(), model.clone());
    }

    direction::encode(
        DirectionData::Model(subtree),
        layer_id,
        edit_status_id,
        Default::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atdoc::parse_document;

    fn stroke() -> Value {
        json!({
            "id": "local-1",
            "points": { "$points": [10.0, 20.0, 0.5, 0.0, 30.5, 40.5, 0.5, 8.0] },
            "color": "#0000fb",
            "width": 2.4,
            "opacity": 0.65,
            "penType": "ballpoint",
            "bounds": { "x": 10.0, "y": 20.0, "width": 20.5, "height": 20.5 },
        })
    }

    fn author() -> Author {
        Author {
            user_id: "213163099101".into(),
            name: "田中 博悠".into(),
            company_id: "12386000".into(),
            room_id: "5777426101072728".into(),
            room_user_id: "1786500056276".into(),
        }
    }

    fn built() -> crate::atdoc::ParsedDocument {
        let (bytes, _) = add_stroke(
            &stroke(),
            "P1_[layer-forUser]_213163099101",
            &mut IdGenerator::new(1),
            &author(),
            Some("esid-1"),
        )
        .unwrap();
        parse_document(&bytes).unwrap()
    }

    fn find<'a>(
        doc: &'a crate::atdoc::ParsedDocument,
        kind: &str,
    ) -> &'a crate::atdoc::ParsedModel {
        doc.models
            .values()
            .find(|m| m.model_type == kind)
            .unwrap_or_else(|| panic!("no {kind} in the direction"))
    }

    #[test]
    fn base92_matches_the_alphabet_the_receiver_parses() {
        assert_eq!(base92(0), "0");
        assert_eq!(base92(1), "1");
        assert_eq!(base92(91), "~");
        assert_eq!(base92(92), "10");
        // Every character has to come from the table, or the id will not parse.
        let text = base92(u64::MAX);
        assert!(text.bytes().all(|b| CHARSET.contains(&b)), "{text}");
    }

    #[test]
    fn an_id_is_a_prefix_and_a_count() {
        let mut ids = IdGenerator::new(92);
        assert_eq!(ids.next_id(), "10 1");
        assert_eq!(ids.next_id(), "10 2");
    }

    #[test]
    fn the_direction_says_where_it_belongs() {
        let doc = built();
        let root = &doc.models[&(doc.root_index as usize)];
        assert_eq!(root.model_type, "direction");
        assert_eq!(
            root.props["target"],
            json!("P1_[layer-forUser]_213163099101_[unit]_draw")
        );
        assert_eq!(root.props["esid"], json!("esid-1"));
    }

    #[test]
    fn it_is_an_add_at_the_version_the_receiver_expects() {
        let doc = built();
        let d = find(&doc, "D");
        assert_eq!(d.props["T"], json!(0));
        assert_eq!(d.props["V"], json!(2));
        assert_eq!(d.props["M"], json!(5));
        assert_eq!(d.props["O"], json!(6));
        assert!(d.props["C"].as_str().is_some_and(|c| c.contains(' ')));
    }

    #[test]
    fn the_element_carries_the_points_two_at_a_time() {
        // This app keeps `x, y, pressure, time`; the wire keeps only the pair.
        let doc = built();
        let e = find(&doc, "E");
        assert_eq!(e.props["P"]["$points"], json!([10.0, 20.0, 30.5, 40.5]));
        assert_eq!(e.props["T"], json!(1));
        assert_eq!(e.props["B"], json!(0));
        assert_eq!(e.props["t"], json!(0));
    }

    #[test]
    fn the_element_is_stamped_with_who_wrote_it() {
        let doc = built();
        let e = find(&doc, "E");
        assert_eq!(e.props["uII"], json!("213163099101"));
        assert_eq!(e.props["uIN"], json!("田中 博悠"));
        assert_eq!(e.props["uIR"], json!("5777426101072728"));
    }

    #[test]
    fn the_pen_style_is_the_bare_hex_the_original_stores() {
        let doc = built();
        let s = find(&doc, "S");
        assert_eq!(s.props["C"], json!("0000FB"));
        assert_eq!(s.props["A"], json!(0.65));
        assert_eq!(s.props["P"], json!(0));
    }

    #[test]
    fn the_record_points_at_the_element_and_the_element_at_the_pen() {
        let doc = built();
        let record = find(&doc, "i");
        let element = record.props["m"]["$ref"].as_u64().unwrap() as usize;
        assert_eq!(doc.models[&element].model_type, "E");
        let pen = doc.models[&element].props["p"]["$ref"].as_u64().unwrap() as usize;
        assert_eq!(doc.models[&pen].model_type, "S");
        assert_eq!(record.props["i"], doc.models[&element].props["I"]);
    }

    #[test]
    fn what_it_builds_is_what_the_reader_reads_back() {
        // The proof that the two halves agree: decode our own direction with
        // the code that decodes the room's.
        let (bytes, element_id) = add_stroke(
            &stroke(),
            "P1_[layer-forUser]_9",
            &mut IdGenerator::new(7),
            &author(),
            None,
        )
        .unwrap();
        assert!(element_id.contains(' '), "{element_id}");
        let direction = super::super::apply::decode(&bytes).unwrap();
        match direction.changes.as_slice() {
            [super::super::apply::Change::Stroke { stroke, .. }] => {
                assert_eq!(stroke["color"], json!("#0000fb"));
                assert_eq!(stroke["points"]["$points"][0], json!(10.0));
            }
            other => panic!("{other:?}"),
        }
    }

    /// What `strokeToProp` writes for a stroke the user just drew: points and
    /// pen, and no bounding box — the app recomputes that on every read.
    fn freshly_drawn() -> Value {
        json!({
            "id": "local-2",
            "points": { "$points": [10.0, 20.0, 0.5, 0.0, 30.0, 50.0, 0.5, 8.0] },
            "color": "#1f1f1f",
            "width": 2.0,
            "penType": "ballpoint",
            "opacity": 1.0,
            "pressureSensitivity": 0.5,
        })
    }

    #[test]
    fn a_stroke_the_user_just_drew_has_its_box_worked_out() {
        // It has no `bounds`; refusing to send it made every save from the
        // editor fail.
        let (bytes, _) = add_stroke(
            &freshly_drawn(),
            "P1_[layer-forUser]_9",
            &mut IdGenerator::new(1),
            &author(),
            None,
        )
        .unwrap();
        let doc = parse_document(&bytes).unwrap();
        let e = find(&doc, "E");
        // The points span (10,20)-(30,50), grown by 1.5x the 2.0 pen width.
        let at = |key: &str| e.props[key].as_f64().unwrap();
        assert_eq!(
            (at("BX"), at("BY"), at("BW"), at("BH")),
            (7.0, 17.0, 26.0, 36.0)
        );
    }

    #[test]
    fn a_box_the_stroke_already_has_is_used_as_it_stands() {
        let doc = built();
        let e = find(&doc, "E");
        assert_eq!(e.props["BX"].as_f64(), Some(10.0));
        assert_eq!(e.props["BW"].as_f64(), Some(20.5));
    }

    #[test]
    fn a_stroke_with_no_points_is_refused_rather_than_sent_empty() {
        let mut bad = stroke();
        bad["points"]["$points"] = json!([]);
        assert!(add_stroke(&bad, "L", &mut IdGenerator::new(1), &author(), None).is_err());
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    use crate::model::GenericModel;

    fn note_with(strokes: Value, layer_type: &str) -> GenericTree {
        let mut tree = GenericTree::new("root", "$sharenote");
        tree.insert(GenericModel {
            id: "page".into(),
            parent_id: Some("root".into()),
            model_type: "$page".into(),
            props: json!({ "pageId": "P1" }),
            children: Vec::new(),
        });
        tree.insert(GenericModel {
            id: "layer".into(),
            parent_id: Some("page".into()),
            model_type: "$layer".into(),
            props: json!({ "layerId": "P1_[layer-forUser]_9", "layerType": layer_type }),
            children: Vec::new(),
        });
        tree.insert(GenericModel {
            id: "draw".into(),
            parent_id: Some("layer".into()),
            model_type: "$draw".into(),
            props: json!({ "strokes": strokes }),
            children: Vec::new(),
        });
        tree
    }

    fn ledger(stroke_id: &str) -> Ledger {
        Ledger {
            stroke_id: stroke_id.into(),
            element_id: format!("el {stroke_id}"),
            layer_id: "P1_[layer-forUser]_9".into(),
        }
    }

    #[test]
    fn a_stroke_the_room_has_not_been_told_about_goes_out() {
        let tree = note_with(json!([{ "id": "a" }, { "id": "b" }]), "system:personal");
        let (added, removed, _) = changes(&tree, &[ledger("a")]);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].stroke_id, "b");
        assert_eq!(added[0].layer_id, "P1_[layer-forUser]_9");
        assert!(removed.is_empty());
    }

    #[test]
    fn a_stroke_no_longer_in_the_note_is_reported_as_erased() {
        let tree = note_with(json!([{ "id": "a" }]), "system:personal");
        let (added, removed, _) = changes(&tree, &[ledger("a"), ledger("gone")]);
        assert!(added.is_empty());
        assert_eq!(removed, vec![ledger("gone")]);
    }

    #[test]
    fn saving_does_not_turn_every_stroke_into_an_erasure() {
        // The editor rewrites a stroke's properties from its own model on
        // every save, keeping only what `strokeToProp` writes — id, points,
        // pen. A first version of this kept "already sent" *in the stroke*, so
        // the first save wiped the marks and the next send deleted the note's
        // entire contents for everyone in the class.
        let saved = json!([
            { "id": "a", "points": { "$points": [1.0, 2.0, 0.5, 0.0] }, "color": "#000000",
              "width": 2.0, "penType": "ballpoint", "opacity": 1.0 },
        ]);
        let tree = note_with(saved, "system:personal");
        let (added, removed, _) = changes(&tree, &[ledger("a")]);
        assert!(added.is_empty(), "nothing new to send");
        assert!(removed.is_empty(), "and nothing to erase");
    }

    #[test]
    fn a_stroke_moved_to_another_layer_has_not_been_erased() {
        let mut tree = note_with(json!([{ "id": "a" }]), "system:edit");
        // The ledger remembers it from when it was on a personal layer.
        let (added, removed, _) = changes(&tree, &[ledger("a")]);
        assert!(added.is_empty());
        assert!(removed.is_empty(), "still in the note, just not here");

        // Gone from the note entirely is a different matter.
        tree.models.get_mut("draw").unwrap().props = json!({ "strokes": [] });
        let (_, removed, _) = changes(&tree, &[ledger("a")]);
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn a_layer_the_room_has_no_booth_for_is_not_sent_from() {
        let tree = note_with(json!([{ "id": "a" }]), "system:edit");
        let (added, _, _) = changes(&tree, &[]);
        assert!(added.is_empty());
    }

    #[test]
    fn a_stroke_with_no_id_of_its_own_is_left_alone() {
        // There would be no way to tell it from the next one after a save.
        let tree = note_with(
            json!([{ "points": { "$points": [1.0, 2.0] } }]),
            "system:personal",
        );
        assert!(changes(&tree, &[]).0.is_empty());
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    use crate::model::GenericModel;

    #[test]
    fn nothing_is_erased_from_a_layer_the_note_does_not_have() {
        // A note that did not load the way this expects — a page that never
        // came down, a tree read mid-write — must not be read as "the user
        // erased all of it". Getting that wrong once deleted a class note's
        // contents for everyone in it.
        let tree = GenericTree::new("root", "$sharenote");
        let ledger = vec![Ledger {
            stroke_id: "a".into(),
            element_id: "el a".into(),
            layer_id: "P1_[layer-forUser]_9".into(),
        }];
        let (added, removed, _) = changes(&tree, &ledger);
        assert!(added.is_empty());
        assert!(removed.is_empty(), "an absent layer is not an erasure");
    }

    #[test]
    fn an_empty_layer_that_is_still_there_does_mean_erased() {
        let mut tree = GenericTree::new("root", "$sharenote");
        tree.insert(GenericModel {
            id: "layer".into(),
            parent_id: Some("root".into()),
            model_type: "$layer".into(),
            props: json!({ "layerId": "P1_[layer-forUser]_9", "layerType": "system:personal" }),
            children: Vec::new(),
        });
        let ledger = vec![Ledger {
            stroke_id: "a".into(),
            element_id: "el a".into(),
            layer_id: "P1_[layer-forUser]_9".into(),
        }];
        assert_eq!(changes(&tree, &ledger).1.len(), 1);
    }
}

#[cfg(test)]
mod stale_tests {
    use super::*;
    use crate::model::GenericModel;

    fn note_with_layer(layer_id: &str) -> GenericTree {
        let mut tree = GenericTree::new("root", "$sharenote");
        tree.insert(GenericModel {
            id: "layer".into(),
            parent_id: Some("root".into()),
            model_type: "$layer".into(),
            props: json!({ "layerId": layer_id, "layerType": "system:personal" }),
            children: Vec::new(),
        });
        tree.insert(GenericModel {
            id: "draw".into(),
            parent_id: Some("layer".into()),
            model_type: "$draw".into(),
            props: json!({ "strokes": [{ "id": "a" }] }),
            children: Vec::new(),
        });
        tree
    }

    #[test]
    fn a_layer_named_the_way_the_room_names_them_is_sendable() {
        let tree = note_with_layer("__subId_v2_[x]_[page]_4_[layer-forUser]_9");
        let (added, _, stale) = changes(&tree, &[]);
        assert_eq!(added.len(), 1);
        assert_eq!(stale, 0);
    }

    #[test]
    fn a_layer_this_app_named_itself_is_reported_rather_than_posted() {
        // A note taken by a build that overwrote the classroom's names with
        // its own model ids. Posting to that name reaches nobody, and looks
        // from the outside exactly like sending working.
        let tree = note_with_layer("note_abc_l___subId_v2__x___page__4__layer_forUser__9");
        let (added, _, stale) = changes(&tree, &[]);
        assert!(added.is_empty());
        assert_eq!(stale, 1);
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::model::GenericModel;

    fn note_with_unit(unit_type: &str, unit_id: &str, layer_id: &str, props: Value) -> GenericTree {
        let mut tree = GenericTree::new("root", "$sharenote");
        tree.insert(GenericModel {
            id: "page".into(),
            parent_id: Some("root".into()),
            model_type: "$page".into(),
            props: json!({ "pageId": "P1" }),
            children: Vec::new(),
        });
        tree.insert(GenericModel {
            id: "layer".into(),
            parent_id: Some("page".into()),
            model_type: "$layer".into(),
            props: json!({ "layerId": layer_id, "layerType": "system:personal" }),
            children: Vec::new(),
        });
        let mut unit_props = props;
        if let Value::Object(map) = &mut unit_props {
            map.insert("unitId".into(), json!(unit_id));
        }
        tree.insert(GenericModel {
            id: "unit".into(),
            parent_id: Some("layer".into()),
            model_type: unit_type.into(),
            props: unit_props,
            children: Vec::new(),
        });
        tree
    }

    fn ledger_entry(unit_id: &str, fingerprint_value: &str) -> UnitLedger {
        UnitLedger {
            unit_id: unit_id.into(),
            element_id: format!("el {unit_id}"),
            layer_id: "P1_[layer-forUser]_9".into(),
            fingerprint: fingerprint_value.into(),
        }
    }

    // -- personal_units / unit_changes ---------------------------------------

    #[test]
    fn a_unit_the_room_has_not_been_told_about_goes_out_as_new() {
        let tree = note_with_unit(
            "$text",
            "u-1",
            "P1_[layer-forUser]_9",
            json!({ "x": 1.0, "width": 10.0 }),
        );
        let (added, changed, removed, stale) = unit_changes(&tree, &[]);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].unit_id, "u-1");
        assert_eq!(added[0].layer_id, "P1_[layer-forUser]_9");
        assert!(changed.is_empty());
        assert!(removed.is_empty());
        assert_eq!(stale, 0);
    }

    #[test]
    fn a_unit_whose_fingerprint_still_matches_is_left_alone() {
        let tree = note_with_unit(
            "$shape",
            "u-1",
            "P1_[layer-forUser]_9",
            json!({ "x": 1.0, "width": 10.0 }),
        );
        let models = unit_subtree(&tree, "unit");
        let known_good = fingerprint(&models);
        let (added, changed, removed, _) = unit_changes(&tree, &[ledger_entry("u-1", &known_good)]);
        assert!(added.is_empty(), "nothing new to send");
        assert!(changed.is_empty(), "the content has not moved");
        assert!(removed.is_empty());
    }

    #[test]
    fn a_unit_edited_since_the_last_send_goes_out_as_changed() {
        let tree = note_with_unit(
            "$shape",
            "u-1",
            "P1_[layer-forUser]_9",
            json!({ "x": 50.0, "width": 10.0 }),
        );
        // The ledger's fingerprint is from before the edit, so it no longer
        // matches what the tree holds now.
        let (added, changed, removed, _) =
            unit_changes(&tree, &[ledger_entry("u-1", "stale-fingerprint")]);
        assert!(added.is_empty());
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].unit_id, "u-1");
        assert!(removed.is_empty());
    }

    #[test]
    fn a_unit_no_longer_in_the_note_is_reported_as_erased() {
        let tree = note_with_unit("$form", "u-1", "P1_[layer-forUser]_9", json!({ "x": 1.0 }));
        let known_good = fingerprint(&unit_subtree(&tree, "unit"));
        let (added, changed, removed, _) = unit_changes(
            &tree,
            &[
                ledger_entry("u-1", &known_good),
                ledger_entry("gone", "whatever"),
            ],
        );
        assert!(added.is_empty());
        assert!(changed.is_empty());
        assert_eq!(removed, vec![ledger_entry("gone", "whatever")]);
    }

    #[test]
    fn a_unit_moved_to_another_layer_has_not_been_erased() {
        // Moved from the personal layer the ledger remembers it on to a
        // shared one — gone from `personal_units`, but still in the note.
        let mut tree = note_with_unit("$shape", "u-1", "P1_[layer-forUser]_9", json!({ "x": 1.0 }));
        tree.models.get_mut("layer").unwrap().props["layerType"] = json!("system:edit");
        let (_, _, removed, _) = unit_changes(&tree, &[ledger_entry("u-1", "whatever")]);
        assert!(removed.is_empty(), "still in the note, just not there");

        // Gone from the note entirely is a different matter.
        tree.models.get_mut("unit").unwrap().model_type = "$dummy".into();
        let (_, _, removed, _) = unit_changes(&tree, &[ledger_entry("u-1", "whatever")]);
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn a_unit_on_a_layer_this_app_named_itself_is_reported_rather_than_posted() {
        let tree = note_with_unit(
            "$text",
            "u-1",
            "note_abc_l___subId_v2__x___page__4__layer_forUser__9",
            json!({}),
        );
        let (added, _, _, stale) = unit_changes(&tree, &[]);
        assert!(added.is_empty());
        assert_eq!(stale, 1);
    }

    // -- add_unit / update_unit round trips ----------------------------------

    fn text_unit(unit_id: &str) -> Vec<GenericModel> {
        vec![GenericModel {
            id: "unit".into(),
            parent_id: Some("layer".into()),
            model_type: "$text".into(),
            props: json!({ "unitId": unit_id, "x": 10.0, "y": 20.0, "width": 30.5 }),
            children: Vec::new(),
        }]
    }

    #[test]
    fn an_added_unit_decodes_back_as_itself() {
        // The proof that encoder and decoder agree: build with `add_unit`,
        // read with the unmodified `apply.rs::decode`.
        let (bytes, element_id) = add_unit(
            &text_unit("u-9"),
            "P1_[layer-forUser]_9",
            &mut IdGenerator::new(3),
            None,
        )
        .unwrap();
        assert!(element_id.contains(' '));

        let direction = super::super::apply::decode(&bytes).unwrap();
        match direction.changes.as_slice() {
            [super::super::apply::Change::Unit { unit_id, models }] => {
                assert_eq!(unit_id, "u-9");
                assert_eq!(models[0].model_type, "$text");
                assert_eq!(models[0].props["unitId"], json!("u-9"));
                assert_eq!(models[0].props["width"], json!(30.5));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_added_unit_applies_onto_its_layer() {
        let mut tree = GenericTree::new("root", "$sharenote");
        tree.insert(GenericModel {
            id: "page".into(),
            parent_id: Some("root".into()),
            model_type: "$page".into(),
            props: json!({ "pageId": "P1" }),
            children: Vec::new(),
        });

        let (bytes, _) = add_unit(
            &text_unit("u-9"),
            "P1_[layer-forUser]_9",
            &mut IdGenerator::new(3),
            None,
        )
        .unwrap();
        let direction = super::super::apply::decode(&bytes).unwrap();
        let applied = super::super::apply::apply(
            &mut tree,
            "P1_[layer-forUser]_9",
            &direction,
            &Default::default(),
        );
        assert_eq!(applied.units, 1);
        let placed = tree
            .models
            .values()
            .find(|m| m.model_type == "$text")
            .expect("the unit was placed");
        assert_eq!(placed.props["unitId"], json!("u-9"));
    }

    #[test]
    fn an_updated_unit_decodes_as_an_update_to_that_unit() {
        let bytes = update_unit(&text_unit("u-9"), "P1_[layer-forUser]_9", None).unwrap();
        let direction = super::super::apply::decode(&bytes).unwrap();
        match direction.changes.as_slice() {
            [super::super::apply::Change::Unit { unit_id, models }] => {
                assert_eq!(unit_id, "u-9");
                assert_eq!(models[0].props["width"], json!(30.5));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_unit_with_no_body_is_refused_rather_than_sent_empty() {
        assert!(add_unit(&[], "L", &mut IdGenerator::new(1), None).is_err());
        assert!(update_unit(&[], "L", None).is_err());
    }

    #[test]
    fn the_same_subtree_fingerprints_the_same_regardless_of_discovery_order() {
        let a = vec![
            GenericModel {
                id: "x".into(),
                parent_id: None,
                model_type: "$text".into(),
                props: json!({ "unitId": "u-1" }),
                children: Vec::new(),
            },
            GenericModel {
                id: "y".into(),
                parent_id: None,
                model_type: "$text".into(),
                props: json!({ "a": 1 }),
                children: Vec::new(),
            },
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn a_different_subtree_fingerprints_differently() {
        let a = text_unit("u-1");
        let mut b = text_unit("u-1");
        b[0].props["width"] = json!(999.0);
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }
}
