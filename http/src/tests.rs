use super::*;
use axum::http::Request;
use serde_json::{json, Value};
use std::ffi::CString;
use std::os::unix::fs::symlink;
use tempfile::TempDir;
use tower::ServiceExt;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    state: AppState,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        let root = temp.path().join("notebooks");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("index.html"), "sidequery").unwrap();
        let config = ServerConfig {
            port: 0,
            local_asset_directory: assets.display().to_string(),
            remote_asset_url: String::new(),
            quack_url: "http://127.0.0.1:49999".into(),
            token: "test-capability".into(),
            duckdb_version: "test".into(),
            notebook_root: root.display().to_string(),
        };
        let state = build_state(config, 4777).unwrap();
        Self {
            _temp: temp,
            root,
            state,
        }
    }

    fn request(&self, method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:4777")
            .header(header::AUTHORIZATION, "Bearer test-capability")
            .header(header::ORIGIN, "http://127.0.0.1:4777");
        let body = if let Some(value) = body {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        } else {
            Body::empty()
        };
        builder.body(body).unwrap()
    }
}

async fn json_body(response: Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_QUACK_MESSAGE_BYTES)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn list_read_create_update_and_conflict() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("nested")).unwrap();
    fs::write(fixture.root.join("nested/example.sqlnb"), "select 1;").unwrap();
    fs::write(fixture.root.join("ignored.txt"), "no").unwrap();
    let app = router(fixture.state.clone());

    let response = app
        .clone()
        .oneshot(fixture.request("GET", "/api/v1/files", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let listed = json_body(response).await;
    assert_eq!(listed["files"], json!([{"path":"nested/example.sqlnb"}]));
    assert_eq!(listed["directories"], json!(["nested"]));

    let response = app
        .clone()
        .oneshot(fixture.request("GET", "/api/v1/files", None))
        .await
        .unwrap();
    assert_eq!(
        json_body(response).await["files"],
        json!([{"path":"nested/example.sqlnb"}])
    );

    let response = app
        .clone()
        .oneshot(fixture.request("GET", "/api/v1/file?path=nested%2Fexample.sqlnb", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let opened = json_body(response).await;
    assert_eq!(opened["source"], "select 1;");
    let revision = opened["revision"].as_str().unwrap();
    assert_eq!(revision.len(), 64);

    let response = app
        .clone()
        .oneshot(fixture.request(
            "PUT",
            "/api/v1/file",
            Some(json!({
                "path":"new/fresh.sql", "source":"select 2;", "expectedRevision":""
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fs::read_to_string(fixture.root.join("new/fresh.sql")).unwrap(),
        "select 2;"
    );

    let response = app
        .clone()
        .oneshot(fixture.request(
            "PUT",
            "/api/v1/file",
            Some(json!({
                "path":"nested/example.sqlnb", "source":"select 3;", "expectedRevision":revision
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let updated = json_body(response).await;
    assert_ne!(updated["revision"], revision);

    let response = app
        .oneshot(fixture.request(
            "PUT",
            "/api/v1/file",
            Some(json!({
                "path":"nested/example.sqlnb", "source":"lost edit", "expectedRevision":revision
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "revision_conflict"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("nested/example.sqlnb")).unwrap(),
        "select 3;"
    );
}

#[tokio::test]
async fn creates_nested_folder_lists_empty_directories_and_rejects_conflicts() {
    let fixture = Fixture::new();
    let app = router(fixture.state.clone());

    let mut unauthorized = fixture.request(
        "POST",
        "/api/v1/folder",
        Some(json!({"path":"not-created"})),
    );
    unauthorized.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer wrong"),
    );
    assert_eq!(
        app.clone().oneshot(unauthorized).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(!fixture.root.join("not-created").exists());

    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/folder",
            Some(json!({"path":"reports/weekly"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({"path":"reports/weekly"}));
    assert!(fixture.root.join("reports/weekly").is_dir());

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(fixture.request("GET", "/api/v1/files", None))
            .await
            .unwrap();
        let listing = json_body(response).await;
        assert_eq!(listing["files"], json!([]));
        assert_eq!(listing["directories"], json!(["reports", "reports/weekly"]));
    }

    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/folder",
            Some(json!({"path":"reports/weekly"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(response).await["error"]["code"], "already_exists");

    let response = app
        .oneshot(fixture.request("POST", "/api/v1/folder", Some(json!({"path":"../escape"}))))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn moves_notebooks_between_nested_and_root_paths_without_overwrite() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("incoming")).unwrap();
    fs::create_dir_all(fixture.root.join("archive")).unwrap();
    fs::write(fixture.root.join("incoming/report.sqlnb"), "select 7;").unwrap();
    let expected = revision(b"select 7;");
    let app = router(fixture.state.clone());

    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/file/move",
            Some(json!({
                "path":"incoming/report.sqlnb",
                "destinationPath":"report.sqlnb",
                "expectedRevision":expected,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({"path":"report.sqlnb", "revision":expected})
    );
    assert!(!fixture.root.join("incoming/report.sqlnb").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("report.sqlnb")).unwrap(),
        "select 7;"
    );

    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/file/move",
            Some(json!({
                "path":"report.sqlnb",
                "destinationPath":"archive/report.sqlnb",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!fixture.root.join("report.sqlnb").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("archive/report.sqlnb")).unwrap(),
        "select 7;"
    );

    let response = app
        .oneshot(fixture.request(
            "POST",
            "/api/v1/file/move",
            Some(json!({
                "path":"archive/report.sqlnb",
                "destinationPath":"archive/report.sqlnb",
                "expectedRevision":expected,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn move_rejects_collisions_stale_revisions_auth_traversal_and_symlinks() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("target")).unwrap();
    fs::write(fixture.root.join("source.sqlnb"), "source").unwrap();
    fs::write(fixture.root.join("target/existing.sqlnb"), "destination").unwrap();
    let app = router(fixture.state.clone());

    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/file/move",
            Some(json!({
                "path":"source.sqlnb",
                "destinationPath":"target/existing.sqlnb",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(response).await["error"]["code"], "already_exists");
    assert_eq!(
        fs::read_to_string(fixture.root.join("source.sqlnb")).unwrap(),
        "source"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("target/existing.sqlnb")).unwrap(),
        "destination"
    );

    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/file/move",
            Some(json!({
                "path":"source.sqlnb",
                "destinationPath":"target/stale.sqlnb",
                "expectedRevision":"stale",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!fixture.root.join("target/stale.sqlnb").exists());

    let mut unauthorized = fixture.request(
        "POST",
        "/api/v1/file/move",
        Some(json!({"path":"source.sqlnb", "destinationPath":"target/auth.sqlnb"})),
    );
    unauthorized.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer wrong"),
    );
    assert_eq!(
        app.clone().oneshot(unauthorized).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(!fixture.root.join("target/auth.sqlnb").exists());

    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/file/move",
            Some(json!({"path":"source.sqlnb", "destinationPath":"../escape.sqlnb"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let outside = fixture._temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("outside.sqlnb"), "outside").unwrap();
    symlink(
        outside.join("outside.sqlnb"),
        fixture.root.join("linked.sqlnb"),
    )
    .unwrap();
    symlink(&outside, fixture.root.join("linked-dir")).unwrap();

    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/file/move",
            Some(json!({"path":"linked.sqlnb", "destinationPath":"target/link.sqlnb"})),
        ))
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    assert!(outside.join("outside.sqlnb").exists());

    let response = app
        .oneshot(fixture.request(
            "POST",
            "/api/v1/file/move",
            Some(json!({"path":"source.sqlnb", "destinationPath":"linked-dir/moved.sqlnb"})),
        ))
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    assert!(!outside.join("moved.sqlnb").exists());
    assert!(fixture.root.join("source.sqlnb").exists());
}

#[tokio::test]
async fn rejects_bad_auth_host_origin_and_paths() {
    let fixture = Fixture::new();
    let app = router(fixture.state.clone());

    let mut unauthorized = fixture.request("GET", "/api/v1/files", None);
    unauthorized.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer wrong"),
    );
    assert_eq!(
        app.clone().oneshot(unauthorized).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let mut bad_origin = fixture.request("GET", "/api/v1/files", None);
    bad_origin.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://evil.test"),
    );
    assert_eq!(
        app.clone().oneshot(bad_origin).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let mut bad_host = fixture.request("GET", "/api/v1/files", None);
    bad_host
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("evil.test"));
    assert_eq!(
        app.clone().oneshot(bad_host).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    for path in [
        "../secret.sql",
        "/absolute.sql",
        "nested\\bad.sql",
        "note.txt",
    ] {
        let uri = format!("/api/v1/file?path={}", urlencoding_for_test(path));
        let response = app
            .clone()
            .oneshot(fixture.request("GET", &uri, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "path {path}");
    }
}

fn urlencoding_for_test(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn refuses_symlink_escape_for_read_write_and_listing() {
    let fixture = Fixture::new();
    let outside = fixture._temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.sqlnb"), "secret").unwrap();
    symlink(&outside, fixture.root.join("linked")).unwrap();
    let app = router(fixture.state.clone());

    let response = app
        .clone()
        .oneshot(fixture.request("GET", "/api/v1/files", None))
        .await
        .unwrap();
    assert_eq!(json_body(response).await["files"], json!([]));

    let response = app
        .clone()
        .oneshot(fixture.request("GET", "/api/v1/file?path=linked%2Fsecret.sqlnb", None))
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(fixture.request(
            "PUT",
            "/api/v1/file",
            Some(json!({
                "path":"linked/new.sqlnb", "source":"escape", "expectedRevision":""
            })),
        ))
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    assert!(!outside.join("new.sqlnb").exists());

    let response = router(fixture.state.clone())
        .oneshot(fixture.request(
            "POST",
            "/api/v1/folder",
            Some(json!({"path":"linked/new-folder"})),
        ))
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    assert!(!outside.join("new-folder").exists());
}

#[tokio::test]
async fn bootstrap_and_static_assets_preserve_security_contract() {
    let fixture = Fixture::new();
    let app = router(fixture.state.clone());
    let response = app
        .clone()
        .oneshot(fixture.request("GET", "/api/v1/bootstrap", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_SECURITY_POLICY], CSP);
    let body = json_body(response).await;
    assert_eq!(body["protocol"], "quack");
    assert_eq!(body["endpoint"], "/quack");

    let response = app
        .oneshot(fixture.request("GET", "/", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap(),
        Bytes::from_static(b"sidequery")
    );
}

#[test]
fn remote_assets_require_safe_fixed_origin() {
    assert!(parse_remote_asset_url("https://sidequery.example/ui").is_ok());
    assert!(parse_remote_asset_url("http://127.0.0.1:4777/ui").is_ok());
    assert!(parse_remote_asset_url("http://sidequery.example/ui").is_err());
    assert!(parse_remote_asset_url("https://user@sidequery.example/ui").is_err());
    assert!(parse_remote_asset_url("https://sidequery.example/ui?channel=dev").is_err());
    assert!(parse_remote_asset_url("https://sidequery.example/%2e%2e/private").is_err());
}

#[test]
fn ffi_server_binds_reports_port_and_stops() {
    let temp = tempfile::tempdir().unwrap();
    let assets = temp.path().join("assets");
    let notebooks = temp.path().join("notebooks");
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("index.html"), "ok").unwrap();
    let config = CString::new(
        json!({
            "port":0,
            "localAssetDirectory":assets,
            "remoteAssetUrl":"",
            "quackUrl":"http://127.0.0.1:49999/quack",
            "token":"test",
            "duckdbVersion":"test",
            "notebookRoot":notebooks
        })
        .to_string(),
    )
    .unwrap();
    let mut error = ptr::null_mut();
    let handle = sidequery_http_start(config.as_ptr(), &mut error);
    assert!(
        !handle.is_null(),
        "{}",
        if error.is_null() {
            "unknown error".into()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        }
    );
    assert_ne!(sidequery_http_port(handle), 0);
    sidequery_http_stop(handle);
}
