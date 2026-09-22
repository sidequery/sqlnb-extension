use super::*;
use axum::http::Request;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

type AssetRoutes = Arc<Mutex<HashMap<String, (u16, Vec<u8>, Duration)>>>;

struct AssetServer {
    address: SocketAddr,
    routes: AssetRoutes,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl AssetServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let routes = AssetRoutes::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_routes = routes.clone();
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // macOS can inherit the listener's nonblocking mode.
                        // Request threads must wait for complete HTTP headers.
                        stream.set_nonblocking(false).unwrap();
                        let request_routes = thread_routes.clone();
                        thread::spawn(move || serve_asset_request(stream, &request_routes));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            routes,
            stop,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> Url {
        Url::parse(&format!("http://{}/ui/", self.address)).unwrap()
    }

    fn publish(&self, manifest: &AssetManifest, files: &[(&str, &[u8])]) {
        let mut routes = self.routes.lock().unwrap();
        routes.clear();
        routes.insert(
            "/ui/manifest.json".into(),
            (200, serde_json::to_vec(manifest).unwrap(), Duration::ZERO),
        );
        for (path, bytes) in files {
            routes.insert(
                format!("/ui/{}{path}", manifest.asset_base),
                (200, bytes.to_vec(), Duration::ZERO),
            );
        }
    }

    fn delay(&self, path: &str, delay: Duration) {
        self.routes.lock().unwrap().get_mut(path).unwrap().2 = delay;
    }

    fn publish_manifest_only(&self, manifest: &AssetManifest) {
        self.publish(manifest, &[]);
    }
}

impl Drop for AssetServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_asset_request(mut stream: TcpStream, routes: &AssetRoutes) {
    let mut request_line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut header = String::new();
        loop {
            header.clear();
            if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
                break;
            }
        }
    }
    let path = request_line.split_ascii_whitespace().nth(1).unwrap_or("/");
    let (status, body, delay) =
        routes
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or((404, Vec::new(), Duration::ZERO));
    thread::sleep(delay);
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let location = if status == 307 {
        format!("Location: {}\r\n", String::from_utf8_lossy(&body))
    } else {
        String::new()
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

fn asset_manifest(version: &str, files: &[(&str, &[u8])]) -> AssetManifest {
    AssetManifest {
        version: version.into(),
        bridge_version: ASSET_BRIDGE_VERSION,
        asset_base: String::new(),
        files: files
            .iter()
            .map(|(path, bytes)| AssetManifestFile {
                path: (*path).into(),
                sha256: revision(bytes),
                size: bytes.len() as u64,
            })
            .collect(),
    }
}

#[test]
fn remote_assets_accept_canonical_index_redirect_and_verify_contents() {
    let temp = TempDir::new().unwrap();
    let server = AssetServer::start();
    let files: &[(&str, &[u8])] = &[("index.html", b"release one")];
    let mut manifest = asset_manifest("1111111", files);
    manifest.asset_base = "1111111/".into();
    server.publish(&manifest, files);
    {
        let mut routes = server.routes.lock().unwrap();
        routes.insert(
            "/ui/1111111/index.html".into(),
            (307, b"/ui/1111111/".to_vec(), Duration::ZERO),
        );
        routes.insert(
            "/ui/1111111/".into(),
            (200, files[0].1.to_vec(), Duration::ZERO),
        );
    }
    let assets = resolve_remote_assets(&server.base_url(), temp.path()).unwrap();
    assert_eq!(
        fs::read(assets.path.join("index.html")).unwrap(),
        files[0].1
    );

    // A canonical redirect does not authorize different bytes.
    server
        .routes
        .lock()
        .unwrap()
        .get_mut("/ui/1111111/")
        .unwrap()
        .1 = b"release bad".to_vec();
    let empty_cache = TempDir::new().unwrap();
    let error = resolve_remote_assets(&server.base_url(), empty_cache.path())
        .err()
        .unwrap();
    assert!(error.contains("SHA-256 verification"), "{error}");
}

#[test]
fn remote_assets_reject_redirects_outside_exact_index_directory() {
    let server = AssetServer::start();
    let files: &[(&str, &[u8])] = &[("index.html", b"release")];
    for location in [
        "https://example.com/ui/",
        "/other/",
        "/ui/?changed=1",
        "/ui/index.html",
    ] {
        let temp = TempDir::new().unwrap();
        server.publish(&asset_manifest("1111111", files), files);
        server.routes.lock().unwrap().insert(
            "/ui/index.html".into(),
            (307, location.as_bytes().to_vec(), Duration::ZERO),
        );
        let error = resolve_remote_assets(&server.base_url(), temp.path())
            .err()
            .unwrap();
        assert!(error.contains("307"), "{location}: {error}");
    }
}

#[test]
fn remote_assets_download_concurrently_within_shared_budget() {
    let temp = TempDir::new().unwrap();
    let server = AssetServer::start();
    let files: &[(&str, &[u8])] = &[
        ("a.js", b"a"),
        ("b.js", b"b"),
        ("c.js", b"c"),
        ("d.js", b"d"),
        ("e.js", b"e"),
        ("f.js", b"f"),
        ("g.js", b"g"),
        ("index.html", b"index"),
    ];
    server.publish(&asset_manifest("1111111", files), files);
    for (path, _) in files {
        server.delay(&format!("/ui/{path}"), Duration::from_millis(250));
    }
    // Sequential requests need at least two seconds; concurrent downloads fit.
    let assets = resolve_remote_assets_with_budget(
        &server.base_url(),
        temp.path(),
        Duration::from_millis(1500),
    )
    .unwrap();
    for (path, bytes) in files {
        assert_eq!(fs::read(assets.path.join(path)).unwrap(), *bytes);
    }
}

#[test]
#[ignore = "downloads the current production UI; run explicitly for release validation"]
fn remote_assets_live_published_ui() {
    let temp = TempDir::new().unwrap();
    let remote = parse_remote_asset_url("https://app.sidequery.dev/sqlnb/").unwrap();
    let assets = resolve_remote_assets(&remote, temp.path()).unwrap();
    validate_cached_generation(&assets.path, None).unwrap();
    assert!(assets.path.join("index.html").is_file());
}

fn install_legacy_generation(
    cache: &Path,
    manifest: &AssetManifest,
    files: &[(&str, &[u8])],
) -> PathBuf {
    let generation = cache.join("generations").join(&manifest.version);
    fs::create_dir_all(&generation).unwrap();
    for (path, bytes) in files {
        let destination = generation.join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, bytes).unwrap();
    }
    fs::write(
        generation.join("manifest.json"),
        serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
    fs::write(cache.join("current"), &manifest.version).unwrap();
    generation
}

#[test]
fn remote_asset_cache_updates_pins_generations_and_survives_offline_start() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let server = AssetServer::start();
    let remote = server.base_url();
    let v1_files: &[(&str, &[u8])] = &[
        ("assets/app.js", b"console.log('one')"),
        ("index.html", b"version one"),
    ];
    let mut v1 = asset_manifest("1111111", v1_files);
    v1.asset_base = "1111111/".into();
    server.publish(&v1, v1_files);

    let pinned_v1 = resolve_remote_assets(&remote, &cache).unwrap();
    assert_eq!(
        fs::read(pinned_v1.join("index.html")).unwrap(),
        b"version one"
    );
    assert_eq!(
        fs::read_to_string(cache.join("managed-v1/current")).unwrap(),
        "1111111"
    );

    let v2_files: &[(&str, &[u8])] = &[
        ("assets/app.js", b"console.log('two')"),
        ("index.html", b"version two"),
    ];
    let mut v2 = asset_manifest("2222222", v2_files);
    v2.asset_base = "2222222/".into();
    server.publish(&v2, v2_files);
    let pinned_v2 = resolve_remote_assets(&remote, &cache).unwrap();
    assert_ne!(pinned_v1, pinned_v2);
    assert_eq!(
        fs::read(pinned_v2.join("index.html")).unwrap(),
        b"version two"
    );
    assert_eq!(
        fs::read(pinned_v1.join("index.html")).unwrap(),
        b"version one"
    );
    assert_eq!(
        fs::read_to_string(cache.join("managed-v1/current")).unwrap(),
        "2222222"
    );

    drop(server);
    let offline = resolve_remote_assets(&remote, &cache).unwrap();
    assert_eq!(offline, pinned_v2);
    assert_eq!(
        fs::read(offline.join("index.html")).unwrap(),
        b"version two"
    );
}

#[test]
fn remote_asset_cache_falls_back_on_partial_hash_and_incompatible_updates() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let server = AssetServer::start();
    let remote = server.base_url();
    let stable_files: &[(&str, &[u8])] = &[("index.html", b"stable")];
    let stable = asset_manifest("aaaaaaa", stable_files);
    server.publish(&stable, stable_files);
    let stable_path = resolve_remote_assets(&remote, &cache).unwrap();

    let intended_files: &[(&str, &[u8])] = &[("index.html", b"complete update")];
    let partial = asset_manifest("bbbbbbb", intended_files);
    server.publish(&partial, &[("index.html", b"partial")]);
    assert_eq!(resolve_remote_assets(&remote, &cache).unwrap(), stable_path);
    assert!(!cache.join("managed-v1/generations/bbbbbbb").exists());

    let hash_mismatch = asset_manifest("ccccccc", intended_files);
    server.publish(&hash_mismatch, &[("index.html", b"same sized bad!")]);
    assert_eq!(resolve_remote_assets(&remote, &cache).unwrap(), stable_path);
    assert!(!cache.join("managed-v1/generations/ccccccc").exists());

    let mut incompatible = asset_manifest("ddddddd", intended_files);
    incompatible.bridge_version = 2;
    server.publish_manifest_only(&incompatible);
    assert_eq!(resolve_remote_assets(&remote, &cache).unwrap(), stable_path);
    assert_eq!(
        fs::read_to_string(cache.join("managed-v1/current")).unwrap(),
        "aaaaaaa"
    );
    assert!(fs::read_dir(cache.join("managed-v1"))
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")));
}

#[test]
fn remote_asset_cache_rejects_invalid_first_generation_and_unsafe_paths() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let server = AssetServer::start();
    let remote = server.base_url();
    let mut incompatible = asset_manifest("eeeeeee", &[("index.html", b"no")]);
    incompatible.bridge_version = 9;
    server.publish_manifest_only(&incompatible);
    assert!(resolve_remote_assets(&remote, &cache).is_err());

    for path in [
        "../index.html",
        "foo//index.html",
        "foo/./index.html",
        "https:evil.example",
        "app.js?x",
        "app.js#x",
        "percent%2ejs",
    ] {
        assert!(validate_asset_path(path).is_err(), "path {path}");
    }
    assert!(scoped_remote_url(&remote, "../escape.js").is_err());
}

#[test]
fn remote_asset_cache_repairs_corrupt_version_without_mutating_pinned_directory() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let server = AssetServer::start();
    let remote = server.base_url();
    let files: &[(&str, &[u8])] = &[("index.html", b"verified")];
    let manifest = asset_manifest("abcdef0", files);
    server.publish(&manifest, files);
    let original = resolve_remote_assets(&remote, &cache).unwrap();
    fs::write(original.join("index.html"), b"corrupt!").unwrap();

    let repaired = resolve_remote_assets(&remote, &cache).unwrap();
    assert_ne!(repaired, original);
    assert_eq!(fs::read(original.join("index.html")).unwrap(), b"corrupt!");
    assert_eq!(fs::read(repaired.join("index.html")).unwrap(), b"verified");
    let cache_id = fs::read_to_string(cache.join("managed-v1/current")).unwrap();
    assert!(cache_id.starts_with("abcdef0-"));
    assert_eq!(cache_id.len(), "abcdef0-".len() + 32);

    // The original remains corrupt, but a subsequent online start reuses the
    // active repair without downloading the assets again.
    server.publish_manifest_only(&manifest);
    assert_eq!(resolve_remote_assets(&remote, &cache).unwrap(), repaired);

    // Reusing a version for different valid contents violates the immutable
    // manifest contract. Keep serving the active verified generation.
    let changed_files: &[(&str, &[u8])] = &[("index.html", b"different")];
    let changed_manifest = asset_manifest("abcdef0", changed_files);
    server.publish(&changed_manifest, changed_files);
    assert_eq!(resolve_remote_assets(&remote, &cache).unwrap(), repaired);
    assert_eq!(fs::read(repaired.join("index.html")).unwrap(), b"verified");
    assert_eq!(
        fs::read_dir(cache.join("managed-v1/generations"))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn remote_asset_cache_uses_fallback_when_overall_update_budget_expires() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let server = AssetServer::start();
    let remote = server.base_url();
    let stable_files: &[(&str, &[u8])] = &[("index.html", b"stable")];
    let stable = asset_manifest("1111111", stable_files);
    server.publish(&stable, stable_files);
    let stable_path = resolve_remote_assets(&remote, &cache).unwrap();

    let update_files: &[(&str, &[u8])] = &[("index.html", b"update")];
    let update = asset_manifest("2222222", update_files);
    server.publish(&update, update_files);
    server.delay("/ui/manifest.json", Duration::from_millis(250));
    let started = std::time::Instant::now();
    let selected =
        resolve_remote_assets_with_budget(&remote, &cache, Duration::from_millis(25)).unwrap();
    assert_eq!(selected, stable_path);
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(
        fs::read_to_string(cache.join("managed-v1/current")).unwrap(),
        "1111111"
    );
}

#[test]
fn remote_asset_cache_recovers_from_corrupt_active_generation_while_offline() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let server = AssetServer::start();
    let remote = server.base_url();

    let v1_files: &[(&str, &[u8])] = &[("index.html", b"older valid")];
    server.publish(&asset_manifest("1111111", v1_files), v1_files);
    let v1 = resolve_remote_assets(&remote, &cache).unwrap();
    let v1_path = v1.path.clone();

    let v2_files: &[(&str, &[u8])] = &[("index.html", b"new current")];
    server.publish(&asset_manifest("2222222", v2_files), v2_files);
    let v2 = resolve_remote_assets(&remote, &cache).unwrap();
    let v2_path = v2.path.clone();
    drop(v1);
    fs::write(v2_path.join("index.html"), b"corrupted!!").unwrap();
    drop(v2);
    drop(server);

    let recovered = resolve_remote_assets(&remote, &cache).unwrap();
    assert_eq!(recovered.path.as_path(), v1_path.as_path());
    assert_eq!(
        fs::read(recovered.join("index.html")).unwrap(),
        b"older valid"
    );
    assert_eq!(
        fs::read_to_string(cache.join("managed-v1/current")).unwrap(),
        "1111111"
    );
    assert!(!v2_path.exists());
}

#[test]
fn remote_asset_cache_leases_bound_gc_without_deleting_pinned_generations() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let server = AssetServer::start();
    let remote = server.base_url();

    let publish = |version: &str, body: &'static [u8]| {
        let files: &[(&str, &[u8])] = &[("index.html", body)];
        let manifest = asset_manifest(version, files);
        server.publish(&manifest, files);
        resolve_remote_assets(&remote, &cache).unwrap()
    };
    let v1 = publish("1111111", b"one");
    let v1_path = v1.path.clone();
    let v2 = publish("2222222", b"two");
    let v2_path = v2.path.clone();
    let v3 = publish("3333333", b"three");
    let v3_path = v3.path.clone();

    let lease_probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(v1_path.join(ASSET_LEASE_FILE))
        .unwrap();
    assert!(!try_lock_file(&lease_probe, libc::LOCK_EX, "test lease").unwrap());
    assert!(v1_path.exists() && v2_path.exists() && v3_path.exists());
    drop(lease_probe);

    drop(v1);
    assert!(!v1_path.exists());
    drop(v2);
    assert!(v2_path.exists());

    let v4 = publish("4444444", b"four");
    assert!(!v2_path.exists());
    assert!(v3_path.exists() && v4.path.exists());
    assert_eq!(
        fs::read_dir(cache.join("managed-v1/generations"))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn remote_asset_cache_reads_but_never_prunes_legacy_namespace_offline() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();
    let files: &[(&str, &[u8])] = &[("index.html", b"legacy offline")];
    let legacy = install_legacy_generation(&cache, &asset_manifest("aaaaaaa", files), files);
    let server = AssetServer::start();
    let remote = server.base_url();
    drop(server);

    let selected = resolve_remote_assets(&remote, &cache).unwrap();
    assert_eq!(selected.path, legacy.canonicalize().unwrap());
    assert_eq!(
        fs::read(selected.join("index.html")).unwrap(),
        b"legacy offline"
    );
    assert!(legacy.exists());
    assert!(!cache.join("managed-v1/current").exists());

    let online = AssetServer::start();
    let update_files: &[(&str, &[u8])] = &[("index.html", b"managed update")];
    online.publish(&asset_manifest("bbbbbbb", update_files), update_files);
    let managed = resolve_remote_assets(&online.base_url(), &cache).unwrap();
    assert!(managed
        .path
        .starts_with(cache.join("managed-v1/generations").canonicalize().unwrap()));
    assert_eq!(
        fs::read_to_string(cache.join("current")).unwrap(),
        "aaaaaaa"
    );
    assert!(legacy.exists());
}

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
        let mut state = build_state(config, 4777).unwrap();
        Arc::get_mut(&mut state.notebooks).unwrap().recent_path =
            Some(temp.path().join("preferences/recent.json"));
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
async fn remote_assets_are_served_through_a_symlinked_cache() {
    let mut fixture = Fixture::new();
    let cache = fixture._temp.path().join("cache");
    let alias = fixture._temp.path().join("cache-link");
    fs::create_dir_all(&cache).unwrap();
    symlink(&cache, &alias).unwrap();
    let server = AssetServer::start();
    let files: &[(&str, &[u8])] = &[("index.html", b"cached UI through a symlink")];
    server.publish(&asset_manifest("1111111", files), files);
    let remote = server.base_url();
    let assets = tokio::task::spawn_blocking(move || resolve_remote_assets(&remote, &alias))
        .await
        .unwrap()
        .unwrap();
    fixture.state.assets = Arc::new(assets);

    let response = router(fixture.state.clone())
        .oneshot(fixture.request("GET", "/", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_QUACK_MESSAGE_BYTES)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"cached UI through a symlink");
}

#[tokio::test]
async fn recent_notebook_survives_new_store_and_port_and_tracks_new_selection() {
    let mut fixture = Fixture::new();
    fs::write(fixture.root.join("first.sqlnb"), "select 1;").unwrap();
    fs::write(fixture.root.join("second.sql"), "select 2;").unwrap();
    let app = router(fixture.state.clone());
    let response = app
        .clone()
        .oneshot(fixture.request("GET", "/api/v1/recent-notebook", None))
        .await
        .unwrap();
    assert_eq!(json_body(response).await, json!({"path": null}));
    for path in ["first.sqlnb", "second.sql"] {
        let response = app
            .clone()
            .oneshot(fixture.request(
                "PUT",
                "/api/v1/recent-notebook",
                Some(json!({"path": path})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await, json!({"path": path}));
    }
    let preference = fixture.state.notebooks.recent_path.clone();
    drop(app);
    let mut reopened =
        NotebookStore::open(notebook_root(&fixture.root.display().to_string()).unwrap()).unwrap();
    reopened.recent_path = preference;
    fixture.state.notebooks = Arc::new(reopened);
    fixture.state.port = 4888;
    fixture.state.origin = "http://127.0.0.1:4888".into();
    let mut request = fixture.request("GET", "/api/v1/recent-notebook", None);
    request
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("127.0.0.1:4888"));
    request.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("http://127.0.0.1:4888"),
    );
    let response = router(fixture.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({"path": "second.sql"}));
    assert_eq!(
        fs::read_to_string(fixture.state.notebooks.recent_path.as_ref().unwrap()).unwrap(),
        r#"{"path":"second.sql"}"#
    );
}

#[tokio::test]
async fn recent_notebook_rejects_unsafe_paths_and_unauthenticated_access() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("valid.sql"), "select 1;").unwrap();
    let outside = fixture._temp.path().join("outside.sql");
    fs::write(&outside, "select 9;").unwrap();
    symlink(&outside, fixture.root.join("link.sql")).unwrap();
    symlink(fixture._temp.path(), fixture.root.join("escape")).unwrap();
    let app = router(fixture.state.clone());
    for path in [
        "../outside.sql",
        "/outside.sql",
        "bad.txt",
        "",
        "link.sql",
        "escape/outside.sql",
        "missing.sql",
    ] {
        let response = app
            .clone()
            .oneshot(fixture.request(
                "PUT",
                "/api/v1/recent-notebook",
                Some(json!({"path": path})),
            ))
            .await
            .unwrap();
        assert!(!response.status().is_success(), "{path}");
    }
    for method in ["GET", "PUT"] {
        let mut request = fixture.request(
            method,
            "/api/v1/recent-notebook",
            (method == "PUT").then(|| json!({"path":"valid.sql"})),
        );
        request.headers_mut().remove(header::AUTHORIZATION);
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(fixture.state.notebooks.recent(), None);
}

#[test]
fn recent_notebook_falls_back_for_corruption_deleted_files_and_write_failure() {
    let mut fixture = Fixture::new();
    let store = Arc::get_mut(&mut fixture.state.notebooks).unwrap();
    fs::write(fixture.root.join("valid.sql"), "select 1;").unwrap();
    store.remember(Path::new("valid.sql")).unwrap();
    let preference = store.recent_path.as_ref().unwrap().clone();
    for bytes in [
        "bad json",
        r#"{"path":"../outside.sql"}"#,
        r#"{"path":"missing.sql"}"#,
    ] {
        fs::write(&preference, bytes).unwrap();
        assert_eq!(store.recent(), None);
    }
    store.remember(Path::new("valid.sql")).unwrap();
    fs::remove_file(fixture.root.join("valid.sql")).unwrap();
    assert_eq!(store.recent(), None);
    fs::write(fixture.root.join("valid.sql"), "select 1;").unwrap();
    store.recent_path = Some(preference.join("unwritable.json"));
    assert!(store.remember(Path::new("valid.sql")).is_err());
    assert_eq!(store.read(Path::new("valid.sql")).unwrap().0, "select 1;");
    assert!(store.write(Path::new("new.sql"), b"select 2;", "").is_ok());
}

#[test]
fn recent_notebook_scope_uses_canonical_root_not_server_identity() {
    let first = Fixture::new();
    let second = Fixture::new();
    let first_root = notebook_root(&first.root.display().to_string()).unwrap();
    let equivalent = notebook_root(&first.root.join(".").display().to_string()).unwrap();
    let second_root = notebook_root(&second.root.display().to_string()).unwrap();
    assert_eq!(
        cache_location::notebook_state_path(&first_root).unwrap(),
        cache_location::notebook_state_path(&equivalent).unwrap()
    );
    assert_ne!(
        cache_location::notebook_state_path(&first_root).unwrap(),
        cache_location::notebook_state_path(&second_root).unwrap()
    );
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

#[tokio::test]
async fn history_api_auth_read_restore_conflict_and_settings() {
    let fixture = Fixture::new();
    let path = Path::new("history.sql");
    let first = fixture
        .state
        .notebooks
        .write(path, b"select 1", "")
        .unwrap();
    let second = fixture
        .state
        .notebooks
        .write(path, b"select 2", &first)
        .unwrap();
    let app = router(fixture.state.clone());
    let response = app
        .clone()
        .oneshot(fixture.request("GET", "/api/v1/history?path=history.sql", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["revisions"].as_array().unwrap().len(), 2);
    let id = body["revisions"][1]["id"].as_str().unwrap();
    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/history/restore",
            Some(json!({"path":"history.sql","id":id,"expectedRevision":"stale"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let response = app
        .clone()
        .oneshot(fixture.request(
            "POST",
            "/api/v1/history/restore",
            Some(json!({"path":"history.sql","id":id,"expectedRevision":second})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fs::read(fixture.root.join(path)).unwrap(), b"select 1");
    let response = app
        .clone()
        .oneshot(fixture.request(
            "PUT",
            "/api/v1/history/settings",
            Some(json!({"maxAgeDays":30,"maxRevisions":2,"maxBytes":16777216})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut request = fixture.request("GET", "/api/v1/history?path=history.sql", None);
    request.headers_mut().remove(header::AUTHORIZATION);
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let response = app
        .oneshot(fixture.request("GET", "/api/v1/history?path=../history.sql", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn history_polling_captures_external_changes_and_stops_with_server() {
    let temp = tempfile::tempdir().unwrap();
    let assets = temp.path().join("assets");
    let notebooks = temp.path().join("notebooks");
    fs::create_dir(&assets).unwrap();
    fs::create_dir(&notebooks).unwrap();
    fs::write(assets.join("index.html"), "ok").unwrap();
    fs::write(notebooks.join("external.sql"), "first").unwrap();
    let mut handle = start(ServerConfig {
        port: 0,
        local_asset_directory: assets.display().to_string(),
        remote_asset_url: String::new(),
        quack_url: "http://127.0.0.1:49999/quack".into(),
        token: "test".into(),
        duckdb_version: "test".into(),
        notebook_root: notebooks.display().to_string(),
    })
    .unwrap();
    let store = NotebookStore::open(notebooks.canonicalize().unwrap()).unwrap();
    let await_source = |expected: &[u8]| {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let transaction = store.history.lock().unwrap();
            if transaction
                .list("external.sql")
                .first()
                .is_some_and(|r| transaction.read("external.sql", &r.id).unwrap() == expected)
            {
                break;
            }
            drop(transaction);
            assert!(
                Instant::now() < deadline,
                "external snapshot was not captured"
            );
            thread::sleep(Duration::from_millis(40));
        }
    };
    await_source(b"first");
    fs::write(store.root.join("external.sql"), "second").unwrap();
    await_source(b"second");
    handle.stop();
    let count = store.history.lock().unwrap().list("external.sql").len();
    fs::write(store.root.join("external.sql"), "after stop").unwrap();
    thread::sleep(Duration::from_millis(2100));
    assert_eq!(
        store.history.lock().unwrap().list("external.sql").len(),
        count
    );
}
