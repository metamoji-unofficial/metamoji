//! End-to-end tests for the ClassShare wire format.
//!
//! The unit tests in `cloud.rs` cover the pure parts. These cover the part that
//! actually matters and cannot be checked by reading: that a request leaves
//! this client shaped the way `CsHttpClient` shapes it — right URL, right
//! headers, right envelope — and that the two-step host resolution works.
//!
//! The stub is a raw `TcpListener` rather than a framework: it has to be able
//! to assert on bytes that a typed server would have already normalised away,
//! and adding an HTTP server dependency to ship one test is a poor trade.

use crate::cloud::{CloudClient, DEFAULT_ROOT_SERVER};
use crate::test_support::stub;

fn client() -> CloudClient {
    // No store path: these tests are about the wire, and a shared session file
    // would let them see each other's state.
    CloudClient::new(
        "test-machine".into(),
        "ja_JP".into(),
        "Asia/Tokyo".into(),
        None,
    )
    .unwrap()
}

/// What `mpsroot/RequestServlet` really answers.
///
/// Taken verbatim from a live lookup of a real school code, with the host
/// swapped for the stub. Note `serverURL`, capitalised, and that the id is not
/// echoed back — both were got wrong from the field names alone.
const SCHOOL_OK: &str = r#"{"isOnPremise":false,"isClassRoom":true,
    "serverURL":"{BASE}","errorMessage":"","errorCode":0,"isSeminar":false}"#;

/// A successful login, keyed as `prepareLoginResponse` reads it: `uuid` for the
/// user id, and no `restHost`/`isClassRoom` — those come from the school
/// lookup, which passes them in.
fn login_ok() -> String {
    r#"{"uuid":"u-1","loginName":"student01","name":"山田 太郎",
        "email":"a@example.jp","companyId":"c-1","companyName":"例示小学校",
        "companyVersion":3,"serverVersion":2.0}"#
        .to_string()
}

/// `login_ok`, plus the `qwd` a server can hand back alongside the session
/// (`CsLoginResponse.qwd`, `docs/typespec/auth/auth.tsp`).
fn login_ok_with_qwd() -> String {
    r#"{"uuid":"u-1","loginName":"student01","name":"山田 太郎",
        "email":"a@example.jp","companyId":"c-1","companyName":"例示小学校",
        "companyVersion":3,"serverVersion":2.0,"qwd":"q-1"}"#
        .to_string()
}

#[test]
fn the_root_server_defaults_to_the_documented_one() {
    // `ModelInfo$BuildOptions.DIGITAL_CABINET_URL_ROOT`.
    assert_eq!(DEFAULT_ROOT_SERVER, "https://mps.metamoji.com/");
    assert_eq!(client().root_server(), DEFAULT_ROOT_SERVER);
}

#[tokio::test]
async fn resolving_a_school_hits_the_root_servlet() {
    let stub = stub(vec![(
        "200 OK",
        r#"{"isOnPremise":false,"isClassRoom":true,
            "serverURL":"https://tenant.example/","errorCode":0}"#
            .to_string(),
    )]);
    let client = client();
    client.set_root_server(&stub.base);

    let school = client.resolve_school("school01").await.unwrap();
    let seen = stub.seen.recv().unwrap();

    // A plain query string, not the JSON envelope: a different servlet.
    assert_eq!(seen.method, "GET");
    assert_eq!(seen.path, "/mpsroot/RequestServlet?coLoginId=school01");
    assert_eq!(seen.body, "");

    assert_eq!(school.server_url, "https://tenant.example/");
    assert!(school.is_class_room);
    assert_eq!(school.co_login_id, "school01");
}

#[tokio::test]
async fn a_school_id_is_escaped_into_the_query() {
    let stub = stub(vec![(
        "200 OK",
        r#"{"serverURL":"https://tenant.example/"}"#.to_string(),
    )]);
    let client = client();
    client.set_root_server(&stub.base);

    let _ = client.resolve_school("学校 01").await;
    let seen = stub.seen.recv().unwrap();

    assert_eq!(
        seen.path,
        "/mpsroot/RequestServlet?coLoginId=%E5%AD%A6%E6%A0%A1%2001"
    );
}

#[tokio::test]
async fn an_unknown_school_says_so_rather_than_failing_later() {
    // The servlet answers 200 with no `serverUrl` for an id it does not know.
    let stub = stub(vec![("200 OK", r#"{"errorCode":0}"#.to_string())]);
    let client = client();
    client.set_root_server(&stub.base);

    let err = client.resolve_school("nope").await.unwrap_err().to_string();
    assert!(err.contains("nope"), "{err}");
}

#[tokio::test]
async fn login_posts_the_documented_envelope_to_the_tenant() {
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
    ]);
    let client = client();
    client.set_root_server(&stub.base);

    let session = client
        .login("school01", "student01", "hunter2")
        .await
        .unwrap();

    let resolve = stub.seen.recv().unwrap();
    assert!(resolve.path.starts_with("/mpsroot/RequestServlet"));

    let login = stub.seen.recv().unwrap();
    // `getRestHost() + contextRoot + command`.
    assert_eq!(login.method, "POST");
    assert_eq!(login.path, "/mmjeditor2/2.0/users3/login");
    assert_eq!(
        login.header("content-type"),
        Some("application/json; charset=utf-8")
    );

    // The four X-DM headers `CsHttpClient` adds to every request.
    assert_eq!(
        login.header("x-dm-appversion"),
        Some("MMJDmCloudService/2.0")
    );
    assert_eq!(login.header("x-dm-locale"), Some("ja_JP"));
    assert_eq!(
        login.header("x-dm-productname"),
        Some("Android-Share-G-ClassRoom")
    );
    // Pinned, not merely present. Sending this app's own version here got the
    // login refused with "The product version is not supported" — the server
    // validates it, so it is part of the contract, not telemetry.
    assert_eq!(login.header("x-dm-productversion"), Some("3.15.1.0"));
    assert_eq!(login.header("user-agent"), Some("MMJCmCloudService/1.0"));

    let body = login.json();
    assert_eq!(body["coLoginId"], "school01");
    assert_eq!(body["loginName"], "student01");
    assert_eq!(body["password"], "hunter2");
    // The five fields `CsParamBaseAbstract#stringify()` always adds.
    assert_eq!(body["deviceName"], "test-machine");
    assert_eq!(body["productName"], "Android-Share-G-ClassRoom");
    assert_eq!(body["locale"], "ja_JP");
    assert_eq!(body["timezone"], "Asia/Tokyo");
    assert_eq!(body["productVersion"], "3.15.1.0");

    // `uuid`, not `userId`.
    assert_eq!(session.user_id, "u-1");
    assert_eq!(session.company_name.as_deref(), Some("例示小学校"));
    assert_eq!(client.session().unwrap().login_name, "student01");
    // These come from the school lookup, not the login response.
    assert_eq!(session.rest_host, stub.base);
    assert!(session.is_class_room);
    assert_eq!(session.co_login_id, "school01");
}

#[tokio::test]
async fn simple_login_sends_the_class_and_the_roll_number() {
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
    ]);
    let client = client();
    client.set_root_server(&stub.base);

    client
        .classroom_login("school01", "cg-32", "7", "sakura")
        .await
        .unwrap();

    let _ = stub.seen.recv().unwrap();
    let login = stub.seen.recv().unwrap();

    assert_eq!(login.path, "/mmjeditor2/2.0/users3/classroomlogin");
    let body = login.json();
    assert_eq!(body["classGroupId"], "cg-32");
    assert_eq!(body["idNumber"], "7");
    // No `loginName`: the class and the number are the identity here.
    assert!(body.get("loginName").is_none());
}

#[tokio::test]
async fn an_error_code_becomes_the_message_the_user_sees() {
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        (
            "200 OK",
            r#"{"name":"LicenseExpired","message":"ライセンスの有効期限が切れています",
                "data":{"errorCode":9}}"#
                .to_string(),
        ),
    ]);
    let client = client();
    client.set_root_server(&stub.base);

    let err = client
        .login("school01", "student01", "x")
        .await
        .unwrap_err()
        .to_string();

    // The message the server sent, plus the request that produced it.
    assert!(
        err.starts_with("ライセンスの有効期限が切れています"),
        "{err}"
    );
    assert!(err.contains("/users3/login"), "{err}");
    // A refused login must not leave a session behind.
    assert!(client.session().is_none());
}

#[tokio::test]
async fn an_http_error_page_does_not_surface_as_a_parse_error() {
    let stub = stub(vec![("502 Bad Gateway", "<html>nope</html>".to_string())]);
    let client = client();
    client.set_root_server(&stub.base);

    let err = client
        .resolve_school("school01")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("502"), "{err}");
}

#[tokio::test]
async fn the_class_list_comes_back_in_name_list_order() {
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        (
            "200 OK",
            // Trimmed from a live school's answer, keeping the shape that
            // matters: the outer key is `alllist`, and a staff group arrives in
            // the same list with an empty roll.
            r#"{"alllist":{
                "nameList":["教師グループ","高校3年1組"],
                "detailList":{
                  "高校3年1組":{"idNumberList":["1","2"],"id":"499970003101"},
                  "教師グループ":{"idNumberList":[],"id":"499969981101"}}}}"#
                .to_string(),
        ),
    ]);
    let client = client();
    client.set_root_server(&stub.base);

    let groups = client.class_groups("school01").await.unwrap();
    let _ = stub.seen.recv().unwrap();
    let info = stub.seen.recv().unwrap();

    assert_eq!(info.path, "/mmjeditor2/2.0/users3/getclassroominfo");
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].name, "教師グループ");
    assert!(groups[0].id_numbers.is_empty());
    assert_eq!(groups[1].id, "499970003101");
    assert_eq!(groups[1].id_numbers, ["1", "2"]);
}

#[tokio::test]
async fn signing_out_clears_the_session_even_if_the_server_is_gone() {
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
    ]);
    let client = client();
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();
    assert!(client.session().is_some());

    // The stub has served its two replies and stopped listening, so the logout
    // request cannot succeed — which is exactly the case that matters.
    client.logout().await.unwrap();
    assert!(client.session().is_none());
}

#[tokio::test]
async fn changing_the_root_server_drops_the_session() {
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
    ]);
    let client = client();
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();

    // A different root is a different tenant; carrying the session across would
    // show one school's identity while talking to another's server.
    client.set_root_server("https://other.example/");
    assert!(client.session().is_none());
    assert_eq!(client.root_server(), "https://other.example/");
}

// ---------------------------------------------------------------------------
// Session persistence
// ---------------------------------------------------------------------------
//
// The reason this exists: without it the app asked for a password on every
// launch. The cookie *is* the session, so nothing short of persisting the jar
// keeps a sign-in alive across a restart.

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("metamoji-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn client_at(path: &std::path::Path) -> CloudClient {
    CloudClient::new(
        "test-machine".into(),
        "ja_JP".into(),
        "Asia/Tokyo".into(),
        Some(path.to_path_buf()),
    )
    .unwrap()
}

#[tokio::test]
async fn a_sign_in_survives_a_restart() {
    let path = temp_path("survives.json");
    let _ = std::fs::remove_file(&path);

    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
    ]);

    {
        let client = client_at(&path);
        client.set_root_server(&stub.base);
        client
            .login("school01", "student01", "hunter2")
            .await
            .unwrap();
        assert!(client.session().is_some());
    }

    // A new process, reading only what the last one left behind.
    let restarted = client_at(&path);
    let session = restarted.session().expect("the session came back");
    assert_eq!(session.login_name, "student01");
    assert_eq!(session.co_login_id, "school01");
    assert_eq!(restarted.root_server(), stub.base);
    // And the credential `cosmos/*` needs on every request.
    assert!(restarted.collabo_identity().is_some());
}

#[tokio::test]
async fn the_session_cookie_itself_is_restored() {
    // Restoring the identity but not the cookie would look signed in and then
    // fail on the first request — the worst of both.
    let path = temp_path("cookie.json");
    let _ = std::fs::remove_file(&path);

    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
        ("200 OK", r#"{"drives":[]}"#.to_string()),
    ]);

    {
        let client = client_at(&path);
        client.set_root_server(&stub.base);
        client.login("school01", "student01", "x").await.unwrap();
    }

    let restarted = client_at(&path);
    // The stub set `JSESSIONID` on every reply; a restored jar sends it back.
    let _ = restarted.drive_entries().await;
    let _ = stub.seen.recv().unwrap();
    let _ = stub.seen.recv().unwrap();
    let third = stub.seen.recv().unwrap();
    assert_eq!(
        third.header("cookie"),
        Some("JSESSIONID=abc123"),
        "the restored jar must carry the session cookie"
    );
}

#[tokio::test]
async fn signing_out_erases_the_stored_session() {
    let path = temp_path("signout.json");
    let _ = std::fs::remove_file(&path);

    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
    ]);

    let client = client_at(&path);
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();
    client.logout().await.unwrap();

    // Not merely forgotten in memory: the file must not hold a credential
    // after an explicit sign-out.
    let restarted = client_at(&path);
    assert!(restarted.session().is_none());
    assert!(restarted.collabo_identity().is_none());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("Password"),
        "credential left on disk: {text}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_session_file_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let path = temp_path("perms.json");
    let _ = std::fs::remove_file(&path);

    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
    ]);
    let client = client_at(&path);
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "it holds a credential");
}

#[tokio::test]
async fn listing_drives_is_a_get_with_no_body() {
    // Two things at once, both easy to get wrong: `/drives/entry` is a `GET`,
    // and this API sends a JSON body on `GET` anyway
    // (docs/typespec/README.md §通信の基本仕様).
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
        (
            "200 OK",
            r#"{"uid":"u-1","list":[
                {"id":"d-1","name":"1年1組","groupId":"g-1","hidden":0},
                {"id":"d-2","name":"職員","hidden":1},
                {"name":"idがない"}]}"#
                .to_string(),
        ),
    ]);

    let client = client();
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();
    let entries = client.drive_entries().await.unwrap();

    let _ = stub.seen.recv().unwrap();
    let _ = stub.seen.recv().unwrap();
    let list = stub.seen.recv().unwrap();

    assert_eq!(list.method, "GET");
    assert_eq!(list.path, "/mmjeditor2/2.0/drives/entry");
    // No body. `docs/typespec/README.md` says this API sends JSON on `GET`,
    // but that is per-command: `CsCloudService$28` passes a literal null, and
    // sending one anyway is what broke the drive-home call.
    assert_eq!(list.body, "", "GET /drives/entry sends no body");

    assert_eq!(entries.len(), 2, "the entry with no id is dropped");
    assert_eq!(entries[0].name.as_deref(), Some("1年1組"));
    assert!(entries[1].hidden);
}

#[tokio::test]
async fn a_drives_home_is_fetched_with_get() {
    // `sync-drive.tsp` hangs every drive path off this value, so getting the
    // verb wrong here breaks the whole class-box feature and nothing else.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
        (
            "200 OK",
            r#"{"homeDir":"https://drive.example/x"}"#.to_string(),
        ),
    ]);

    let client = client();
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();
    let home = client.drive_home("d-1").await.unwrap();

    let _ = stub.seen.recv().unwrap();
    let _ = stub.seen.recv().unwrap();
    let seen = stub.seen.recv().unwrap();

    assert_eq!(seen.method, "GET");
    assert_eq!(seen.path, "/mmjeditor2/2.0/drives/d-1/home");
    // Likewise `CsCloudService$30`. The symptom of getting this wrong was a
    // `200` with no `homeDir` in it, which reads as a server problem.
    assert_eq!(seen.body, "", "GET /drives/<id>/home sends no body");
    // Every drive path is concatenated onto this, so it needs the slash.
    assert_eq!(home, "https://drive.example/x/");
}

#[tokio::test]
async fn an_expired_session_is_renewed_rather_than_reported() {
    // The symptom this fixes: the tenant expires the cookie, the next call
    // answers `{"message":"It doesn't log it in."}`, and the app showed that
    // sentence to a user who was plainly signed in. The credential is kept
    // precisely so the client can do what the original's
    // `executeWithAutoLoginFor` does — sign in again and retry, once.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
        // The session has lapsed by the time the drive list is asked for.
        (
            "200 OK",
            r#"{"name":"NotLoginException","message":"It doesn't log it in.",
                "data":{"errorCode":106}}"#
                .to_string(),
        ),
        // The silent re-login.
        ("200 OK", login_ok()),
        // And the retry, which succeeds.
        (
            "200 OK",
            r#"{"uid":"u-1","list":[{"id":"d-1","name":"1年1組"}]}"#.to_string(),
        ),
    ]);

    let client = client();
    client.set_root_server(&stub.base);
    client
        .login("school01", "student01", "hunter2")
        .await
        .unwrap();

    let entries = client.drive_entries().await.expect("recovered");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name.as_deref(), Some("1年1組"));

    let seen: Vec<_> = (0..5).map(|_| stub.seen.recv().unwrap()).collect();
    assert_eq!(seen[2].path, "/mmjeditor2/2.0/drives/entry");
    // The re-login repeats the original handshake, password and all.
    assert_eq!(seen[3].path, "/mmjeditor2/2.0/users3/login");
    assert_eq!(seen[3].json()["password"], "hunter2");
    assert_eq!(seen[4].path, "/mmjeditor2/2.0/drives/entry");
}

#[tokio::test]
async fn a_classroom_session_is_renewed_the_way_it_was_made() {
    // A 簡易ログイン account has no `loginName` to send to `/users3/login`, so
    // renewing it with the wrong handshake would fail for reasons that look
    // nothing like the cause.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
        ("200 OK", r#"{"data":{"errorCode":106}}"#.to_string()),
        ("200 OK", login_ok()),
        ("200 OK", r#"{"list":[]}"#.to_string()),
    ]);

    let client = client();
    client.set_root_server(&stub.base);
    client
        .classroom_login("school01", "cg-32", "7", "sakura")
        .await
        .unwrap();
    client.drive_entries().await.unwrap();

    let seen: Vec<_> = (0..5).map(|_| stub.seen.recv().unwrap()).collect();
    assert_eq!(seen[3].path, "/mmjeditor2/2.0/users3/classroomlogin");
    assert_eq!(seen[3].json()["classGroupId"], "cg-32");
    assert_eq!(seen[3].json()["idNumber"], "7");
}

#[tokio::test]
async fn a_login_that_keeps_failing_stops_rather_than_looping() {
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
        ("200 OK", r#"{"data":{"errorCode":106}}"#.to_string()),
        // The re-login is itself refused.
        (
            "200 OK",
            r#"{"message":"password changed","data":{"errorCode":9}}"#.to_string(),
        ),
    ]);

    let client = client();
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();

    let err = client.drive_entries().await.unwrap_err().to_string();
    assert!(err.starts_with("password changed"), "{err}");
}

#[tokio::test]
async fn a_session_saved_before_the_method_was_recorded_still_renews() {
    // An upgrade must not present itself as "sign in again". Sessions written
    // by an earlier build have no `method`; the normal login is the one with a
    // `loginName`, so it can be reconstructed.
    //
    // The legacy file is produced by signing in for real and then deleting the
    // field, rather than hand-written: a hand-written one with a mistyped key
    // silently falls back to the *production* root server, which is a way for
    // a test to reach the real service by accident.
    let path = temp_path("legacy.json");
    let _ = std::fs::remove_file(&path);

    {
        let stub = stub(vec![
            ("200 OK", SCHOOL_OK.to_string()),
            ("200 OK", login_ok()),
        ]);
        let client = client_at(&path);
        client.set_root_server(&stub.base);
        client
            .login("school01", "student01", "hunter2")
            .await
            .unwrap();
    }

    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(saved.get("method").is_some_and(|m| !m.is_null()));
    saved.as_object_mut().unwrap().remove("method");

    // The old session pointed at a stub that has now stopped; repoint it at a
    // live one without going through `set_root_server`, which clears sessions.
    let stub = stub(vec![
        ("200 OK", r#"{"data":{"errorCode":106}}"#.to_string()),
        ("200 OK", login_ok()),
        (
            "200 OK",
            r#"{"list":[{"id":"d-1","name":"1年1組"}]}"#.to_string(),
        ),
    ]);
    for key in ["root_server", "rest_host"] {
        saved[key] = serde_json::json!(stub.base);
    }
    saved["school"]["serverUrl"] = serde_json::json!(stub.base);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let client = client_at(&path);
    let entries = client.drive_entries().await;
    assert!(entries.is_ok(), "{:?}", entries.err());
    assert_eq!(entries.unwrap().len(), 1);

    let seen: Vec<_> = (0..3).map(|_| stub.seen.recv().unwrap()).collect();
    assert_eq!(seen[0].path, "/mmjeditor2/2.0/drives/entry");
    // The renewal used the normal handshake, derived from `loginName`.
    assert_eq!(seen[1].path, "/mmjeditor2/2.0/users3/login");
    assert_eq!(seen[1].json()["loginName"], "student01");
    assert_eq!(seen[2].path, "/mmjeditor2/2.0/drives/entry");
}

#[tokio::test]
async fn a_qwd_from_the_login_response_is_what_reauthentication_uses_next() {
    // `/users3/login` can hand back a `qwd` (`CsLoginResponse.qwd`) alongside
    // the session it approves, and `CsLoginParam` accepts one in place of a
    // password. Once a response has offered one there is no reason left to
    // keep the password around for the next renewal — this is also the only
    // avenue by which a restart (same store path) can recover without ever
    // having the password in hand again.
    let path = temp_path("qwd.json");
    let _ = std::fs::remove_file(&path);

    {
        let stub = stub(vec![
            ("200 OK", SCHOOL_OK.to_string()),
            ("200 OK", login_ok_with_qwd()),
        ]);
        let client = client_at(&path);
        client.set_root_server(&stub.base);
        client
            .login("school01", "student01", "hunter2")
            .await
            .unwrap();
    }

    // The credential never lands in the file at all — it goes through the OS
    // keyring (a per-path in-memory stand-in under `#[cfg(test)]`), not here.
    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        saved.get("credential").is_none(),
        "session.json must never carry the credential: {saved}"
    );

    // The old session pointed at a stub that has now stopped; repoint it at a
    // live one without going through `set_root_server`, which clears sessions.
    let stub = stub(vec![
        ("200 OK", r#"{"data":{"errorCode":106}}"#.to_string()),
        ("200 OK", login_ok()),
        (
            "200 OK",
            r#"{"list":[{"id":"d-1","name":"1年1組"}]}"#.to_string(),
        ),
    ]);
    for key in ["root_server", "rest_host"] {
        saved[key] = serde_json::json!(stub.base);
    }
    saved["school"]["serverUrl"] = serde_json::json!(stub.base);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let client = client_at(&path);
    let entries = client.drive_entries().await;
    assert!(entries.is_ok(), "{:?}", entries.err());

    let seen: Vec<_> = (0..3).map(|_| stub.seen.recv().unwrap()).collect();
    let renewal = seen[1].json();
    assert_eq!(renewal["qwd"], "q-1");
    assert!(
        renewal.get("password").is_none(),
        "renewal must not resend a password once a qwd exists: {renewal}"
    );
}

/// No test may reach the network.
///
/// The bug this guards: a persisted-session fixture with a mistyped key falls
/// back to `DEFAULT_ROOT_SERVER`, and the test then talks to the real service.
/// Nothing in the code says "this field name is load-bearing for test safety",
/// so the check has to be explicit.
#[test]
fn a_client_with_no_saved_root_server_is_not_pointed_at_production_by_a_test() {
    let path = temp_path("empty.json");
    std::fs::write(&path, b"{}").unwrap();

    let client = client_at(&path);
    // It *does* default to production — that is right for the app. The point
    // is that a test must never then issue a request, which is why every test
    // above calls `set_root_server` before anything else.
    assert_eq!(client.root_server(), crate::cloud::DEFAULT_ROOT_SERVER);
    assert!(
        client.session().is_none(),
        "no session, so no request is possible"
    );
}

#[tokio::test]
async fn a_home_response_without_the_field_names_what_it_did_contain() {
    // "サーバーの応答に homeDir がありません" on its own is undiagnosable from a
    // screenshot. The keys say whether the call reached the right endpoint at
    // all — and keys are safe to show; the values are the user's.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
        ("200 OK", r#"{"list":[],"uid":"u-1"}"#.to_string()),
    ]);

    let client = client();
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();

    let err = client.drive_home("d-1").await.unwrap_err().to_string();
    assert!(err.contains("list"), "{err}");
    assert!(err.contains("uid"), "{err}");
}

#[tokio::test]
async fn a_failure_names_the_request_that_caused_it() {
    // "HTTP 500" on its own could be any of a dozen calls. The path is what
    // makes one screenshot enough to know which.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
        ("500 Internal Server Error", r#"{"uid":"u-1"}"#.to_string()),
    ]);

    let client = client();
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();

    let err = client.drive_entries().await.unwrap_err().to_string();
    assert!(err.contains("500"), "{err}");
    assert!(err.contains("GET /mmjeditor2/2.0/drives/entry"), "{err}");
    // Keys, so the reader can tell a wrong endpoint from a broken one.
    assert!(err.contains("uid"), "{err}");
    // Never the host: it is the tenant's and adds nothing.
    assert!(!err.contains("127.0.0.1"), "{err}");
}

#[tokio::test]
async fn the_user_id_is_read_from_uuid_or_from_user_id() {
    // `prepareLoginResponse` reads `uuid` into the field named `userId`, so the
    // key and the field name differ. Accepting either matters because this id
    // is what the *drive* service authenticates with: get it wrong and the
    // failure surfaces as `InvalidUserOrPasswordException` from a different
    // host, which is a long way from the cause.
    for (key, expected) in [("uuid", "u-uuid"), ("userId", "u-plain")] {
        let stub = stub(vec![
            ("200 OK", SCHOOL_OK.to_string()),
            (
                "200 OK",
                format!(r#"{{"{key}":"{expected}","loginName":"student01"}}"#),
            ),
        ]);
        let client = client();
        client.set_root_server(&stub.base);
        let session = client.login("school01", "student01", "x").await.unwrap();
        assert_eq!(session.user_id, expected, "from {key}");
    }
}

#[tokio::test]
async fn a_login_response_with_no_user_id_fails_at_the_login() {
    // Rather than three services later, with a message about a password.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        (
            "200 OK",
            r#"{"loginName":"student01","name":"山田"}"#.to_string(),
        ),
    ]);
    let client = client();
    client.set_root_server(&stub.base);

    let err = client
        .login("school01", "student01", "x")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("ユーザー ID"), "{err}");
    // And the keys, so the real name of the field can be found.
    assert!(err.contains("loginName"), "{err}");
    assert!(client.session().is_none(), "a broken session is not kept");
}

#[tokio::test]
async fn a_session_missing_its_user_id_repairs_itself() {
    // A build that read the id from one key only could store a session that
    // looks signed in — name, school, credential all present — and is useless
    // to the drive service. Telling the user to sign out and back in is an
    // instruction the app can carry out itself.
    let path = temp_path("incomplete.json");
    let _ = std::fs::remove_file(&path);

    {
        let stub = stub(vec![
            ("200 OK", SCHOOL_OK.to_string()),
            ("200 OK", login_ok()),
        ]);
        let client = client_at(&path);
        client.set_root_server(&stub.base);
        client
            .login("school01", "student01", "hunter2")
            .await
            .unwrap();
    }

    // Blank the id the way the older parse would have left it.
    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    saved["session"]["userId"] = serde_json::json!("");

    let stub = stub(vec![("200 OK", login_ok())]);
    for key in ["root_server", "rest_host"] {
        saved[key] = serde_json::json!(stub.base);
    }
    saved["school"]["serverUrl"] = serde_json::json!(stub.base);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let client = client_at(&path);
    assert_eq!(client.session().unwrap().user_id, "");

    client.ensure_complete().await.unwrap();
    assert_eq!(client.session().unwrap().user_id, "u-1");

    // One request, and it was the login — no prompt, no sign-out.
    let seen = stub.seen.recv().unwrap();
    assert_eq!(seen.path, "/mmjeditor2/2.0/users3/login");
}

#[tokio::test]
async fn a_complete_session_is_left_alone() {
    // `ensure_complete` runs before every class-box open; it must not cost a
    // login each time.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        ("200 OK", login_ok()),
    ]);
    let client = client();
    client.set_root_server(&stub.base);
    client.login("school01", "student01", "x").await.unwrap();

    let _ = stub.seen.recv().unwrap();
    let _ = stub.seen.recv().unwrap();

    client.ensure_complete().await.unwrap();
    assert!(
        stub.seen.try_recv().is_err(),
        "a complete session must make no request"
    );
}

#[tokio::test]
async fn a_numeric_user_id_is_read_as_one() {
    // This API is not consistent about scalar types — `CmUtils.toString` exists
    // precisely because of it. `as_str()` alone reads a numeric id as absent,
    // and the failure then surfaces from the drive service as a rejected
    // password.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        (
            "200 OK",
            r#"{"uuid":12345,"loginName":"student01"}"#.to_string(),
        ),
    ]);
    let client = client();
    client.set_root_server(&stub.base);

    let session = client.login("school01", "student01", "x").await.unwrap();
    assert_eq!(session.user_id, "12345");
}

#[tokio::test]
async fn a_null_user_id_says_it_is_null_rather_than_missing() {
    // "the key is not there" and "the key is there and null" need different
    // fixes, and the message is the only place that distinction can surface.
    let stub = stub(vec![
        ("200 OK", SCHOOL_OK.to_string()),
        (
            "200 OK",
            r#"{"uuid":null,"loginName":"student01"}"#.to_string(),
        ),
    ]);
    let client = client();
    client.set_root_server(&stub.base);

    let err = client
        .login("school01", "student01", "x")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("uuid=null"), "{err}");
    assert!(err.contains("userId=なし"), "{err}");
}
