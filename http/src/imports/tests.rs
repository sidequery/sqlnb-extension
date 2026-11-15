use super::*;
use axum::http::Request;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use tempfile::TempDir;
use tower::ServiceExt;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    state: AppState,
}

const PORT: u16 = 4778;

fn config(temp: &TempDir, port: u16) -> ServerConfig {
    let assets = temp.path().join("assets");
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("index.html"), "sidequery").unwrap();
    ServerConfig {
        port,
        local_asset_directory: assets.display().to_string(),
        remote_asset_url: String::new(),
        quack_url: "http://127.0.0.1:49999".into(),
        token: "test-capability".into(),
        duckdb_version: "test".into(),
        notebook_root: temp.path().join("notebooks").display().to_string(),
        mcp_token: String::new(),
    }
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let state = build_state(config(&temp, PORT), PORT).unwrap();
        let root = state.notebooks.root.clone();
        Self { temp, root, state }
    }

    fn imports(&self) -> PathBuf {
        self.root.join(IMPORTS_FOLDER)
    }

    fn request(&self, method: &str, uri: &str, body: &[u8]) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, format!("127.0.0.1:{PORT}"))
            .header(header::AUTHORIZATION, "Bearer test-capability")
            .header(header::ORIGIN, format!("http://127.0.0.1:{PORT}"))
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = router(self.state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn upload(&self, name: &str, body: &[u8]) -> (StatusCode, Value) {
        let uri = format!("/api/v1/import?name={}", encode(name));
        self.send(self.request("PUT", &uri, body)).await
    }

    /// Files in the imports folder other than its `.gitignore`.
    fn saved(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.imports())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .filter(|name| name != ".gitignore")
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[tokio::test]
async fn saves_the_upload_under_the_notebook_root_and_advertises_it() {
    let fixture = Fixture::new();
    let (status, body) = fixture.upload("Q3 sales.csv", b"id,total\n1,2\n").await;
    assert_eq!(status, StatusCode::CREATED);
    let saved = fixture.imports().join("Q3 sales.csv");
    assert_eq!(body["name"], "Q3 sales.csv");
    assert_eq!(body["bytes"], 13);
    assert_eq!(body["path"], saved.display().to_string());
    assert!(Path::new(body["path"].as_str().unwrap()).is_absolute());
    assert_eq!(fs::read(&saved).unwrap(), b"id,total\n1,2\n");
    // Uploaded data is ignored by Git and kept out of the notebook listing.
    assert_eq!(
        fs::read_to_string(fixture.imports().join(".gitignore")).unwrap(),
        "*\n"
    );
    fs::write(fixture.imports().join("stray.sql"), "select 1").unwrap();
    let (_, listing) = fixture
        .send(fixture.request("GET", "/api/v1/files", b""))
        .await;
    assert_eq!(listing["files"], serde_json::json!([]));
    assert_eq!(listing["directories"], serde_json::json!([]));

    let (_, bootstrap) = fixture
        .send(fixture.request("GET", "/api/v1/bootstrap", b""))
        .await;
    assert_eq!(bootstrap["imports"]["endpoint"], "/api/v1/import");
    assert_eq!(bootstrap["imports"]["maxBytes"], MAX_IMPORT_BYTES);
    assert_eq!(
        bootstrap["imports"]["folder"],
        fixture.imports().display().to_string()
    );
}

#[tokio::test]
async fn never_overwrites_and_numbers_the_name_instead() {
    let fixture = Fixture::new();
    for body in [b"one".as_slice(), b"two", b"three"] {
        assert_eq!(
            fixture.upload("events.json.gz", body).await.0,
            StatusCode::CREATED
        );
    }
    let (_, body) = fixture.upload("Events.JSON.GZ", b"four").await;
    assert_eq!(
        fixture.saved(),
        [
            "events (2).json.gz",
            "events (3).json.gz",
            "events.json.gz",
            body["name"].as_str().unwrap()
        ]
        .into_iter()
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
    );
    assert_eq!(
        fs::read(fixture.imports().join("events.json.gz")).unwrap(),
        b"one"
    );
    assert_eq!(
        fs::read(fixture.imports().join("events (3).json.gz")).unwrap(),
        b"three"
    );
}

#[test]
fn reduces_names_to_a_safe_base_name_with_a_data_extension() {
    assert_eq!(safe_name("..hidden.CSV").unwrap(), "hidden.csv");
    assert_eq!(
        safe_name("we:ird*name?.Parquet").unwrap(),
        "we_ird_name_.parquet"
    );
    assert_eq!(safe_name("logs.ndjson.zst").unwrap(), "logs.ndjson.zst");
    assert_eq!(safe_name("ventes été.tsv").unwrap(), "ventes été.tsv");
    let long = format!("{}.txt", "a".repeat(400));
    assert_eq!(
        safe_name(&long).unwrap(),
        format!("{}.txt", "a".repeat(120))
    );
    for name in [
        "../escape.csv",
        "nested/file.csv",
        "..\\escape.csv",
        "/etc/passwd.csv",
        "line\nbreak.csv",
        "",
    ] {
        assert_eq!(
            safe_name(name).unwrap_err().code,
            "invalid_name",
            "{name:?}"
        );
    }
    for name in [".csv", "...csv", " . .json"] {
        assert!(safe_name(name).is_err(), "{name:?}");
    }
    for name in [
        "notes.sql",
        "book.sqlnb",
        "run.sh",
        "data.csv.exe",
        "data.gz",
        "data.xlsx",
    ] {
        assert_eq!(
            safe_name(name).unwrap_err().code,
            "unsupported_file_type",
            "{name:?}"
        );
    }
}

#[tokio::test]
async fn refuses_traversal_separators_and_unsupported_types_without_writing() {
    let fixture = Fixture::new();
    for (name, code) in [
        ("../escape.csv", "invalid_name"),
        ("a/b.csv", "invalid_name"),
        ("a\\b.csv", "invalid_name"),
        ("payload.sh", "unsupported_file_type"),
        ("notebook.sqlnb", "unsupported_file_type"),
    ] {
        let (status, body) = fixture.upload(name, b"x").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(body["error"]["code"], code, "{name}");
    }
    assert!(!fixture.imports().exists());
    assert!(!fixture.temp.path().join("escape.csv").exists());
}

#[tokio::test]
async fn requires_the_file_api_capability_and_origin() {
    let fixture = Fixture::new();
    let uri = "/api/v1/import?name=data.csv";
    let unauthenticated = Request::builder()
        .method("PUT")
        .uri(uri)
        .header(header::HOST, format!("127.0.0.1:{PORT}"))
        .body(Body::from("x"))
        .unwrap();
    let mut wrong_origin = fixture.request("PUT", uri, b"x");
    wrong_origin
        .headers_mut()
        .insert(header::ORIGIN, HeaderValue::from_static("http://evil.test"));
    let mut wrong_host = fixture.request("PUT", uri, b"x");
    wrong_host
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("evil.test"));
    let mut delete = fixture.request("DELETE", uri, b"");
    delete.headers_mut().remove(header::AUTHORIZATION);
    for (request, status) in [
        (unauthenticated, StatusCode::UNAUTHORIZED),
        (wrong_origin, StatusCode::UNAUTHORIZED),
        (wrong_host, StatusCode::BAD_REQUEST),
        (delete, StatusCode::UNAUTHORIZED),
    ] {
        assert_eq!(fixture.send(request).await.0, status);
    }
    assert!(!fixture.imports().exists());
}

#[cfg(unix)]
#[tokio::test]
async fn does_not_follow_symlinks_out_of_the_root() {
    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.imports()).unwrap();
    let (status, body) = fixture.upload("data.csv", b"x").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "invalid_imports_folder");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    let (status, _) = fixture
        .send(fixture.request("DELETE", "/api/v1/import?name=data.csv", b""))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A symlink inside the real folder is never written through or deleted.
    fs::remove_file(fixture.imports()).unwrap();
    fs::create_dir(fixture.imports()).unwrap();
    fs::write(outside.join("target.csv"), "outside").unwrap();
    std::os::unix::fs::symlink(
        outside.join("target.csv"),
        fixture.imports().join("data.csv"),
    )
    .unwrap();
    let (status, body) = fixture.upload("data.csv", b"inside").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "data (2).csv");
    assert_eq!(
        fs::read_to_string(outside.join("target.csv")).unwrap(),
        "outside"
    );
    let (status, _) = fixture
        .send(fixture.request("DELETE", "/api/v1/import?name=data.csv", b""))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(outside.join("target.csv").exists());
    assert!(fixture.imports().join("data.csv").is_symlink());
}

#[tokio::test]
async fn refuses_a_declared_length_over_the_limit() {
    let fixture = Fixture::new();
    let mut request = fixture.request("PUT", "/api/v1/import?name=big.parquet", b"");
    request.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&(MAX_IMPORT_BYTES + 1).to_string()).unwrap(),
    );
    let (status, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"]["code"], "file_too_large");
    assert!(fixture.saved().is_empty());
}

#[tokio::test]
async fn enforces_the_limit_while_streaming_and_removes_the_partial_file() {
    let fixture = Fixture::new();
    let store = fixture.state.notebooks.clone();
    let error = receive(
        store.clone(),
        "big.csv".into(),
        None,
        Body::from(vec![b'x'; 4096]),
        1024,
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(fixture.saved().is_empty());
    // Exactly at the limit is accepted.
    let saved = receive(
        store,
        "fits.csv".into(),
        None,
        Body::from(vec![b'x'; 1024]),
        1024,
    )
    .await
    .unwrap();
    assert_eq!(saved.bytes, 1024);
    assert_eq!(fixture.saved(), ["fits.csv"]);
}

#[test]
fn removes_the_partial_file_when_the_client_aborts() {
    let temp = tempfile::tempdir().unwrap();
    let mut handle = start(config(&temp, 0)).unwrap();
    let port = handle.port;
    let imports = temp.path().join("notebooks").join(IMPORTS_FOLDER);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "PUT /api/v1/import?name=partial.csv HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Authorization: Bearer test-capability\r\nContent-Length: 1000000\r\n\r\n"
    )
    .unwrap();
    stream.write_all(&[b'x'; 4096]).unwrap();
    stream.flush().unwrap();
    // The temporary file exists while the upload is in flight.
    let in_flight = (0..200).any(|_| {
        thread::sleep(Duration::from_millis(10));
        fs::read_dir(&imports).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
        })
    });
    assert!(in_flight);
    drop(stream);
    let cleaned = (0..200).any(|_| {
        thread::sleep(Duration::from_millis(10));
        fs::read_dir(&imports)
            .unwrap()
            .flatten()
            .all(|entry| entry.file_name() == ".gitignore")
    });
    assert!(cleaned, "partial upload left behind");
    handle.stop();
}

#[test]
fn serves_uploads_over_a_real_connection() {
    let temp = tempfile::tempdir().unwrap();
    let mut handle = start(config(&temp, 0)).unwrap();
    let port = handle.port;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "PUT /api/v1/import?name=real.csv HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Authorization: Bearer test-capability\r\nContent-Length: 4\r\nConnection: close\r\n\r\na,b\n"
    )
    .unwrap();
    let mut status = String::new();
    BufReader::new(&stream).read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 201"), "{status}");
    handle.stop();
    assert_eq!(
        fs::read(
            temp.path()
                .join("notebooks")
                .join(IMPORTS_FOLDER)
                .join("real.csv")
        )
        .unwrap(),
        b"a,b\n"
    );
}

#[tokio::test]
async fn deletes_only_an_uploaded_file_by_its_saved_name() {
    let fixture = Fixture::new();
    let (status, _) = fixture
        .send(fixture.request("DELETE", "/api/v1/import?name=none.csv", b""))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    fixture.upload("keep.csv", b"keep").await;
    fixture.upload("drop.csv", b"drop").await;
    fs::write(fixture.root.join("outside.csv"), "root file").unwrap();
    for name in ["../outside.csv", ".gitignore", "we:ird.csv", "drop.sql"] {
        let uri = format!("/api/v1/import?name={}", encode(name));
        let (status, _) = fixture.send(fixture.request("DELETE", &uri, b"")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
    }
    let (status, body) = fixture
        .send(fixture.request("DELETE", "/api/v1/import?name=drop.csv", b""))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "drop.csv");
    assert_eq!(fixture.saved(), ["keep.csv"]);
    assert!(fixture.root.join("outside.csv").exists());
    assert!(fixture.imports().join(".gitignore").exists());
}

#[tokio::test]
async fn cancelling_by_upload_id_discards_a_saved_or_later_upload() {
    let fixture = Fixture::new();
    let first = "6f1c1e8e-3c55-4f0a-9d8e-0a6b9a6c1f01";
    let second = "6f1c1e8e-3c55-4f0a-9d8e-0a6b9a6c1f02";
    // The whole body arrived and was saved before the client's cancel.
    let uri = format!("/api/v1/import?name=a.csv&id={first}");
    let (status, _) = fixture.send(fixture.request("PUT", &uri, b"a")).await;
    assert_eq!(status, StatusCode::CREATED);
    fixture.upload("keep.csv", b"keep").await;
    let uri = format!("/api/v1/import?id={first}");
    let (status, body) = fixture.send(fixture.request("DELETE", &uri, b"")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cancelled"], true);
    assert_eq!(fixture.saved(), ["keep.csv"]);
    // Cancelling again, or an id never seen, succeeds without touching files.
    assert_eq!(
        fixture.send(fixture.request("DELETE", &uri, b"")).await.0,
        StatusCode::OK
    );
    let uri = format!("/api/v1/import?id={second}");
    assert_eq!(
        fixture.send(fixture.request("DELETE", &uri, b"")).await.0,
        StatusCode::OK
    );
    // An upload whose id was cancelled before it began is refused.
    let uri = format!("/api/v1/import?name=b.csv&id={second}");
    let (status, body) = fixture.send(fixture.request("PUT", &uri, b"b")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "upload_aborted");
    assert_eq!(fixture.saved(), ["keep.csv"]);

    for uri in [
        "/api/v1/import?name=c.csv&id=not-a-uuid",
        "/api/v1/import?name=c.csv&id=6F1C1E8E-3C55-4F0A-9D8E-0A6B9A6C1F03",
    ] {
        let (status, body) = fixture.send(fixture.request("PUT", uri, b"c")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(body["error"]["code"], "invalid_upload_id");
    }
    for uri in [
        "/api/v1/import",
        "/api/v1/import?name=keep.csv&id=6f1c1e8e-3c55-4f0a-9d8e-0a6b9a6c1f04",
    ] {
        let (status, _) = fixture.send(fixture.request("DELETE", uri, b"")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
    assert_eq!(fixture.saved(), ["keep.csv"]);
}
