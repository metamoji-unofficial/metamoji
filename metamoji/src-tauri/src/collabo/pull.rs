//! Fetching what a note's room already holds.
//!
//! A class note's own file, as the drive hands it out, contains only what the
//! teacher put there: every student's writing lives in the room, on the booth
//! for their personal layer. Opening the note therefore means asking the room
//! for its history — `AttachBooth` with `last:0` replays a booth from the
//! beginning — and folding the result into the document.
//!
//! This is a *pull*, not a session: it joins, collects, and leaves. The relay
//! never says "that is all", so the end is decided by silence — nothing for
//! `QUIET`, or `LIMIT` overall, whichever comes first. A live session that
//! stays joined is a different thing and belongs in `session.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use super::apply::{self, Applied};
use super::send;
use super::session::ClassroomState;
use super::socket::{self, CollaboEvent, Command};
use crate::cloud::CloudClient;
use crate::error::AppResult;
use crate::model::GenericTree;

/// A booth id and the bytes that arrived on it.
type Received = Arc<Mutex<Vec<(String, i64, Vec<u8>)>>>;
/// A ticket, its media type, and its bytes.
type Asset = (String, String, Vec<u8>);
/// A booth and the bytes to post to it.
pub type Frame = (String, Vec<u8>);

/// No traffic for this long means the replay is over.
const QUIET: Duration = Duration::from_millis(1500);
/// However busy the room, stop asking after this — unless the room is still
/// sending when it is reached, in which case the wait is extended (see
/// `MAX_EXTENSIONS`).
const LIMIT: Duration = Duration::from_secs(20);
/// How many times `LIMIT` may be extended for a room that is still sending
/// when the current window closes. The relay has no pagination or
/// continuation token — `AttachBooth` either keeps talking or it does not —
/// so the only way to tell a long history from a stuck one is whether
/// anything is still arriving. Cutting a still-busy room off at a flat 20s
/// is what left a reopened note with only part of what had been written
/// (#9); bounding the extensions keeps a room that truly never goes quiet
/// from hanging the open forever.
const MAX_EXTENSIONS: u32 = 4;
/// Long enough for `LoginRoomResult` to come back before booths are asked for.
const LOGIN_GRACE: Duration = Duration::from_millis(1200);

/// Whether a pull should keep waiting for more of a room's history.
///
/// `QUIET` ends it unconditionally — nothing is coming. Short of that, the
/// first `LIMIT` always finishes the wait undisturbed; past it, a room still
/// sending (traffic arrived since `quiet_elapsed` was last reset) earns
/// another `LIMIT`, up to `extensions_used` reaching `MAX_EXTENSIONS`.
fn should_keep_waiting(
    quiet_elapsed: Duration,
    total_elapsed: Duration,
    extensions_used: &mut u32,
) -> bool {
    if quiet_elapsed >= QUIET {
        return false;
    }
    if total_elapsed < LIMIT + LIMIT * (*extensions_used) {
        return true;
    }
    if *extensions_used < MAX_EXTENSIONS {
        *extensions_used += 1;
        true
    } else {
        false
    }
}

/// What a pull found, for the user rather than for the log.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomPull {
    pub directions: usize,
    /// How far each booth was read, so the next connection can carry on.
    #[serde(skip)]
    pub marks: Vec<(String, i64)>,
    /// Everything the room replayed, kept so a resync can work out what the
    /// room actually holds without asking twice.
    #[serde(skip)]
    pub history: Vec<(String, i64, Vec<u8>)>,
    pub units: usize,
    pub strokes: usize,
    /// Elements the room says have been erased since.
    pub removed: usize,
    pub assets: usize,
    /// Strokes now in the note, by the id the room knows them by, and the
    /// layer they are on. The caller records these so a later erase can be
    /// reported.
    #[serde(skip)]
    pub stroke_ids: Vec<(String, String)>,
    /// Edit kinds this build received but does not apply, named and counted.
    pub unsupported: Vec<String>,
    /// Set when the room could not be reached. The note still opens.
    pub error: Option<String>,
}

/// The booths a page keeps its content on.
///
/// The page itself, its common layer, and — depending on how the note was
/// distributed — the layer that belongs to this user. `NsDirectionManager
/// .boothIdArrayOnPage` builds the same list, and `NtPageController` builds
/// the ids: `{pageId}_[layer-common]`, `{pageId}_[layer-forUser]_{userId}`,
/// `{pageId}_[layer-forClass]`.
pub fn booths_for(tree: &GenericTree, user_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    for model in tree.models.values() {
        if model.model_type != "$page" {
            continue;
        }
        let Some(page_id) = model.props.get("pageId").and_then(|v| v.as_str()) else {
            continue;
        };
        out.push(page_id.to_string());
        out.push(format!("{page_id}_[layer-common]"));

        // `forSchoolPageType`: 1 per user, 2 per group, 3 per class.
        match model
            .props
            .get("forSchoolPageType")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
        {
            1 => out.push(format!("{page_id}_[layer-forUser]_{user_id}")),
            3 => out.push(format!("{page_id}_[layer-forClass]")),
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Posts strokes and unit changes into the room, one Direction each.
///
/// Joins, sends, leaves — the same shape as `fetch`, and for the same reason:
/// a session that stays open belongs to the editor, not to a one-off write.
/// Returns how many went out; the caller marks those as sent.
#[allow(clippy::too_many_arguments)]
pub async fn post_strokes(
    cloud: &CloudClient,
    classroom: &ClassroomState,
    room_id: &str,
    strokes: &[send::Pending],
    removals: &[send::Ledger],
    unit_added: &[send::PendingUnit],
    unit_changed: &[send::PendingUnit],
    unit_removed: &[send::UnitLedger],
) -> AppResult<(Posted, PostedUnits)> {
    if strokes.is_empty()
        && removals.is_empty()
        && unit_added.is_empty()
        && unit_changed.is_empty()
        && unit_removed.is_empty()
    {
        return Ok((Posted::default(), PostedUnits::default()));
    }
    let Some(session) = cloud.session() else {
        return Err(crate::error::AppError::other("サインインしていません"));
    };

    let device_id = classroom.device_id(cloud).await?;
    let relay = classroom
        .rest(cloud)
        .await?
        .login_room(room_id, None)
        .await?;

    // The room hands out its own id for us on login, and stamps it on every
    // element. Until it arrives there is nothing honest to put there.
    let room_user_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&room_user_id);
    let connection = socket::connect(&relay.host, relay.port, move |event| {
        if let CollaboEvent::LoggedIn { user_id, .. } = event {
            *seen.lock().unwrap() = user_id;
        }
    })
    .await?;

    connection
        .commands
        .send(Command::Login {
            room_id: room_id.to_string(),
            device_id,
            session_id: relay.session_id.clone(),
            nickname: session.name.clone(),
        })
        .await
        .ok();
    tokio::time::sleep(LOGIN_GRACE).await;

    let author = author_of(
        &session,
        room_id,
        room_user_id.lock().unwrap().clone().unwrap_or_default(),
    );

    let (mut frames, posted) = build_posts(strokes, removals, &author)?;
    let (unit_frames, posted_units) = build_unit_posts(unit_added, unit_changed, unit_removed)?;
    frames.extend(unit_frames);
    for (booth_id, payload) in frames {
        post(&connection, &booth_id, payload).await;
    }

    // The posts are queued on the writer task; leaving at once would drop
    // them. Wait for the acknowledgements to have had time to come back.
    let count = (posted.sent.len()
        + posted.removed.len()
        + posted_units.added.len()
        + posted_units.changed.len()
        + posted_units.removed.len())
    .min(50) as u64;
    tokio::time::sleep(Duration::from_millis(300 + 80 * count)).await;

    let _ = connection
        .commands
        .send(Command::Logout {
            room_id: room_id.to_string(),
        })
        .await;
    let _ = connection.commands.send(Command::Disconnect).await;
    Ok((posted, posted_units))
}

/// Builds the frames a set of changes turns into, without sending anything.
///
/// Shared by the two ways out: a note that is open has a connection already —
/// posting down a second one and then logging out takes the first one with it,
/// because the relay ends the room session for the whole device.
pub fn build_posts(
    strokes: &[send::Pending],
    removals: &[send::Ledger],
    author: &send::Author,
) -> AppResult<(Vec<Frame>, Posted)> {
    let mut ids = send::IdGenerator::fresh();
    let mut frames = Vec::new();
    let mut posted = Posted::default();

    for pending in strokes {
        let (payload, element_id) =
            send::add_stroke(&pending.stroke, &pending.layer_id, &mut ids, author, None)?;
        frames.push((pending.layer_id.clone(), payload));
        posted.sent.push(element_id);
    }
    for entry in removals {
        let payload = send::remove_element(&entry.element_id, &entry.layer_id, &mut ids, None)?;
        frames.push((entry.layer_id.clone(), payload));
        // Keyed the way the ledger is, so the caller can drop the right row.
        posted.removed.push(entry.stroke_id.clone());
    }
    Ok((frames, posted))
}

/// The author stamp the room puts on every element.
pub fn author_of(
    session: &crate::cloud::CloudSession,
    room_id: &str,
    room_user_id: String,
) -> send::Author {
    send::Author {
        user_id: session.user_id.clone(),
        name: session.name.clone(),
        company_id: session.company_id.clone().unwrap_or_default(),
        room_id: room_id.to_string(),
        room_user_id,
    }
}

/// What went out. `sent` holds the room's new element ids, in the order the
/// strokes were given; `removed` holds the note's own stroke ids, which is how
/// the ledger is keyed.
#[derive(Debug, Default, Clone)]
pub struct Posted {
    pub sent: Vec<String>,
    pub removed: Vec<String>,
}

/// Builds the frames a set of unit changes turns into, without sending
/// anything — the unit-shaped counterpart to `build_posts`.
pub fn build_unit_posts(
    added: &[send::PendingUnit],
    changed: &[send::PendingUnit],
    removed: &[send::UnitLedger],
) -> AppResult<(Vec<Frame>, PostedUnits)> {
    let mut ids = send::IdGenerator::fresh();
    let mut frames = Vec::new();
    let mut posted = PostedUnits::default();

    for pending in added {
        let (payload, element_id) =
            send::add_unit(&pending.models, &pending.layer_id, &mut ids, None)?;
        frames.push((pending.layer_id.clone(), payload));
        posted.added.push((
            pending.unit_id.clone(),
            element_id,
            pending.layer_id.clone(),
        ));
    }
    for pending in changed {
        let payload = send::update_unit(&pending.models, &pending.layer_id, None)?;
        frames.push((pending.layer_id.clone(), payload));
        posted
            .changed
            .push((pending.unit_id.clone(), pending.layer_id.clone()));
    }
    for entry in removed {
        let payload = send::remove_element(&entry.element_id, &entry.layer_id, &mut ids, None)?;
        frames.push((entry.layer_id.clone(), payload));
        posted.removed.push(entry.unit_id.clone());
    }
    Ok((frames, posted))
}

/// What went out for units. `added` carries the room's new element id for
/// each unit, alongside the layer it landed on — a unit's own id does not
/// change, but there is no element id to remember until the room has one.
/// `changed` carries no element id: an update mints none. Both are keyed so
/// the caller can fingerprint what it just sent; `removed` is keyed the way
/// the ledger is.
#[derive(Debug, Default, Clone)]
pub struct PostedUnits {
    pub added: Vec<(String, String, String)>,
    pub changed: Vec<(String, String)>,
    pub removed: Vec<String>,
}

pub(crate) async fn post(connection: &socket::Connection, booth_id: &str, payload: Vec<u8>) {
    let _ = connection
        .commands
        .send(Command::PostData {
            booth_id: booth_id.to_string(),
            payload,
            // No echo — we already have it. `save` is the whole point: it is
            // what makes the relay keep it for whoever opens the note next,
            // this user included.
            send_back: false,
            save: true,
            rip_off_size: "0".to_string(),
        })
        .await;
}

/// Joins the room, replays every booth, and folds the result into `tree`.
///
/// Assets come back rather than being written here: they belong in the note's
/// store, which is the caller's to open.
pub async fn fetch(
    cloud: &CloudClient,
    classroom: &ClassroomState,
    room_id: &str,
    tree: &mut GenericTree,
    ledger: &[send::Ledger],
) -> AppResult<(RoomPull, Vec<Asset>)> {
    let Some(session) = cloud.session() else {
        return Ok((
            RoomPull {
                error: Some("サインインしていません".into()),
                ..Default::default()
            },
            Vec::new(),
        ));
    };

    let booths = booths_for(tree, &session.user_id);
    if booths.is_empty() {
        return Ok((RoomPull::default(), Vec::new()));
    }

    let device_id = classroom.device_id(cloud).await?;
    let relay = classroom
        .rest(cloud)
        .await?
        .login_room(room_id, None)
        .await?;

    let received: Received = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&received);
    let connection = socket::connect(&relay.host, relay.port, move |event| {
        if let CollaboEvent::Direction {
            booth_id,
            sequence,
            payload,
            ..
        } = event
        {
            seen.lock().unwrap().push((booth_id, sequence, payload));
        }
    })
    .await?;

    connection
        .commands
        .send(Command::Login {
            room_id: room_id.to_string(),
            device_id,
            session_id: relay.session_id.clone(),
            nickname: session.name.clone(),
        })
        .await
        .ok();
    tokio::time::sleep(LOGIN_GRACE).await;

    for booth in &booths {
        connection
            .commands
            .send(Command::AttachBooth {
                booth_id: booth.clone(),
                last_sequence: 0,
            })
            .await
            .ok();
    }

    // Wait for the replay to go quiet.
    let started = std::time::Instant::now();
    let mut last_count = 0usize;
    let mut quiet_since = std::time::Instant::now();
    let mut extensions_used = 0u32;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let count = received.lock().unwrap().len();
        if count != last_count {
            last_count = count;
            quiet_since = std::time::Instant::now();
        }
        if !should_keep_waiting(
            quiet_since.elapsed(),
            started.elapsed(),
            &mut extensions_used,
        ) {
            break;
        }
    }

    let _ = connection
        .commands
        .send(Command::Logout {
            room_id: room_id.to_string(),
        })
        .await;
    let _ = connection.commands.send(Command::Disconnect).await;

    let marks = {
        let mut highest: std::collections::HashMap<String, i64> = Default::default();
        for (booth_id, sequence, _) in received.lock().unwrap().iter() {
            let mark = highest.entry(booth_id.clone()).or_insert(*sequence);
            *mark = (*mark).max(*sequence);
        }
        highest.into_iter().collect::<Vec<_>>()
    };
    let aliases: std::collections::HashMap<String, String> = ledger
        .iter()
        .map(|l| (l.element_id.clone(), l.stroke_id.clone()))
        .collect();
    let directions = std::mem::take(&mut *received.lock().unwrap());
    let (mut pull, assets) = fold(tree, directions.clone(), &aliases);
    pull.marks = marks;
    pull.history = directions;
    let result = (pull, assets);

    // Whether or not the room had anything, this user needs a layer of their
    // own to write on. Without one the next stroke lands on a layer the room
    // has no booth for and can never be sent.
    for booth in booths.iter().filter(|b| b.contains("_[layer-for")) {
        apply::ensure_booth_layer(tree, booth);
    }
    Ok(result)
}

/// What the room currently holds, worked out from its whole history.
///
/// `added` minus `removed` is what is actually there. Both are needed: a
/// ledger row for an element the room never saw means a post that did not
/// land, and one for an element it has since dropped means someone erased it.
/// They call for opposite repairs, and only the history tells them apart.
#[derive(Debug, Default, Clone)]
pub struct RoomState {
    pub added: std::collections::HashSet<String>,
    pub removed: std::collections::HashSet<String>,
}

impl RoomState {
    pub fn holds(&self, element_id: &str) -> bool {
        self.added.contains(element_id) && !self.removed.contains(element_id)
    }
}

/// Reads the room's history without touching any note.
pub fn survey(directions: &[(String, i64, Vec<u8>)]) -> RoomState {
    let mut state = RoomState::default();
    for (_, _, payload) in directions {
        let Ok(direction) = apply::decode(payload) else {
            continue;
        };
        for change in direction.changes {
            match change {
                apply::Change::Stroke { id, .. } => {
                    state.added.insert(id);
                }
                apply::Change::Remove { id } => {
                    state.removed.insert(id);
                }
                _ => {}
            }
        }
    }
    state
}

/// Applies what came back, in the order it came back.
fn fold(
    tree: &mut GenericTree,
    directions: Vec<(String, i64, Vec<u8>)>,
    aliases: &std::collections::HashMap<String, String>,
) -> (RoomPull, Vec<Asset>) {
    let mut pull = RoomPull {
        directions: directions.len(),
        ..Default::default()
    };
    let mut assets = Vec::new();
    let mut unsupported: std::collections::BTreeMap<String, usize> = Default::default();

    for (booth_id, _sequence, payload) in directions {
        let Ok(direction) = apply::decode(&payload) else {
            log::warn!(
                "{booth_id}: Direction を読み取れませんでした ({} bytes)",
                payload.len()
            );
            *unsupported
                .entry("読み取れない Direction".into())
                .or_default() += 1;
            continue;
        };
        let Applied {
            units,
            strokes,
            removed,
            stroke_ids,
            assets: found,
            unsupported: kinds,
        } = apply::apply(tree, &booth_id, &direction, aliases);
        pull.units += units;
        pull.strokes += strokes;
        pull.removed += removed;
        pull.stroke_ids.extend(stroke_ids);
        assets.extend(found);
        for kind in kinds {
            log::warn!("{booth_id}: 未対応の payload を破棄しました ({kind})");
            *unsupported.entry(kind).or_default() += 1;
        }
    }

    pull.assets = assets.len();
    pull.unsupported = unsupported
        .into_iter()
        .map(|(kind, count)| format!("{kind} x{count}"))
        .collect();
    (pull, assets)
}

#[cfg(test)]
mod waiting_tests {
    use super::*;

    #[test]
    fn quiet_ends_it_no_matter_how_much_time_is_left() {
        let mut extensions = 0;
        assert!(!should_keep_waiting(
            QUIET,
            Duration::from_secs(1),
            &mut extensions
        ));
        assert_eq!(extensions, 0);
    }

    #[test]
    fn well_within_the_first_window_just_keeps_waiting() {
        let mut extensions = 0;
        assert!(should_keep_waiting(
            Duration::from_millis(200),
            Duration::from_secs(5),
            &mut extensions
        ));
        assert_eq!(extensions, 0, "no extension spent while still inside LIMIT");
    }

    #[test]
    fn a_room_still_sending_when_the_limit_is_reached_earns_more_time() {
        let mut extensions = 0;
        for expected in 1..=MAX_EXTENSIONS {
            assert!(should_keep_waiting(
                Duration::from_millis(0),
                LIMIT + LIMIT * (expected - 1),
                &mut extensions
            ));
            assert_eq!(extensions, expected, "one extension spent per busy window");
        }
    }

    #[test]
    fn extensions_run_out_eventually_so_a_never_quiet_room_cannot_hang_forever() {
        let mut extensions = MAX_EXTENSIONS;
        assert!(!should_keep_waiting(
            Duration::from_millis(0),
            LIMIT + LIMIT * MAX_EXTENSIONS,
            &mut extensions
        ));
        assert_eq!(extensions, MAX_EXTENSIONS, "no extension left to spend");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GenericModel;
    use serde_json::json;

    fn page(id: &str, page_type: i64) -> GenericModel {
        GenericModel {
            id: id.into(),
            parent_id: Some("root".into()),
            model_type: "$page".into(),
            props: json!({ "pageId": id, "forSchoolPageType": page_type }),
            children: Vec::new(),
        }
    }

    #[test]
    fn a_per_user_page_asks_for_this_users_own_layer() {
        let mut tree = GenericTree::new("root", "$sharenote");
        tree.insert(page("P1", 1));
        assert_eq!(
            booths_for(&tree, "42"),
            vec![
                "P1".to_string(),
                "P1_[layer-common]".to_string(),
                "P1_[layer-forUser]_42".to_string(),
            ]
        );
    }

    #[test]
    fn a_class_wide_page_asks_for_the_shared_layer_instead() {
        let mut tree = GenericTree::new("root", "$sharenote");
        tree.insert(page("P1", 3));
        assert!(booths_for(&tree, "42").contains(&"P1_[layer-forClass]".to_string()));
        assert!(!booths_for(&tree, "42")
            .iter()
            .any(|b| b.contains("forUser")));
    }

    #[test]
    fn an_ordinary_page_asks_only_for_itself_and_the_common_layer() {
        let mut tree = GenericTree::new("root", "$sharenote");
        tree.insert(page("P1", 0));
        assert_eq!(booths_for(&tree, "42").len(), 2);
    }

    #[test]
    fn a_note_with_no_pages_asks_for_nothing() {
        let tree = GenericTree::new("root", "$sharenote");
        assert!(booths_for(&tree, "42").is_empty());
    }

    #[test]
    fn a_direction_that_will_not_decode_is_counted_rather_than_fatal() {
        let mut tree = GenericTree::new("root", "$sharenote");
        let (pull, assets) = fold(
            &mut tree,
            vec![("P1".into(), 1, b"not a container".to_vec())],
            &Default::default(),
        );
        assert_eq!(pull.directions, 1);
        assert_eq!(pull.units, 0);
        assert!(assets.is_empty());
        assert_eq!(
            pull.unsupported,
            vec!["読み取れない Direction x1".to_string()]
        );
    }
}

#[cfg(test)]
mod survey_tests {
    use super::*;
    use crate::atdoc::{write_document, DocumentMeta};
    use crate::model::{GenericModel, GenericTree};
    use serde_json::{json, Value};

    /// A direction as the wire carries one.
    fn direction(build: impl FnOnce(&mut GenericTree) -> Value) -> (String, i64, Vec<u8>) {
        let mut tree = GenericTree::new("direction", "direction");
        let data = build(&mut tree);
        if let Value::Object(props) = &mut tree.models.get_mut("direction").unwrap().props {
            props.insert("data".into(), data);
            props.insert("target".into(), json!("b_[unit]_draw"));
        }
        (
            "b".into(),
            1,
            write_document(&tree, &DocumentMeta::default()).unwrap(),
        )
    }

    fn model(id: &str, parent: Option<&str>, kind: &str, props: Value) -> GenericModel {
        let mut m = GenericModel {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            model_type: kind.into(),
            props,
            children: Vec::new(),
        };
        if parent == Some("direction") {
            if let Value::Object(p) = &mut m.props {
                crate::atdoc::mark_detached(p);
            }
        }
        m
    }

    fn add(element_id: &str) -> (String, i64, Vec<u8>) {
        direction(|tree| {
            tree.insert(model("d", Some("direction"), "D", json!({ "T": 0 })));
            tree.insert(model(
                "i0",
                Some("d"),
                "i",
                json!({ "i": element_id, "m": { "$ref": "e" } }),
            ));
            tree.insert(model(
                "e",
                Some("direction"),
                "E",
                json!({ "I": element_id, "T": 1, "P": { "$points": [1.0, 2.0, 3.0, 4.0] },
                        "BX": 1.0, "BY": 2.0, "BW": 2.0, "BH": 2.0 }),
            ));
            json!({ "$ref": "d" })
        })
    }

    fn remove(element_id: &str) -> (String, i64, Vec<u8>) {
        direction(|tree| {
            tree.insert(model("d", Some("direction"), "D", json!({ "T": 0 })));
            tree.insert(model(
                "i0",
                Some("d"),
                "i",
                json!({ "i": element_id, "t": 1 }),
            ));
            json!({ "$ref": "d" })
        })
    }

    #[test]
    fn what_the_room_holds_is_what_was_added_and_not_taken_back() {
        let state = survey(&[add("a"), add("b"), remove("a")]);
        assert!(state.holds("b"));
        assert!(!state.holds("a"), "erased");
        assert!(!state.holds("c"), "never there");
    }

    #[test]
    fn an_element_it_never_saw_is_told_apart_from_one_it_dropped() {
        // The repair differs: a post that never landed goes out again, and an
        // element someone erased must not.
        let state = survey(&[add("a"), remove("a")]);
        assert!(state.added.contains("a"), "the room saw this one");
        assert!(!state.added.contains("z"), "and never saw this one");
    }

    #[test]
    fn an_empty_room_holds_nothing() {
        let state = survey(&[]);
        assert!(state.added.is_empty());
        assert!(!state.holds("a"));
    }
}
