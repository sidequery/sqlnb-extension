//! Optional local MCP surface. Its capability is deliberately distinct from the UI capability.
use super::*;
#[path = "mcp_contract.rs"]
mod contract;
use contract::{definitions, Operation};
use rmcp::{model::*, service::RequestContext, ErrorData, RoleServer, ServerHandler};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub type QueryCallback =
    unsafe extern "C" fn(*mut c_void, u64, *const c_char, u64, u64, bool) -> *mut c_char;
pub type InterruptCallback = unsafe extern "C" fn(*mut c_void, u64);
pub type FreeCallback = unsafe extern "C" fn(*mut c_char);
#[derive(Clone)]
pub struct QueryBridge {
    pub context: usize,
    pub query: QueryCallback,
    pub interrupt: InterruptCallback,
    pub free: FreeCallback,
    pub lock: Arc<tokio::sync::Mutex<()>>,
}

impl QueryBridge {
    async fn execute(
        &self,
        sql: String,
        rows: u64,
        read_only: bool,
        ct: CancellationToken,
    ) -> Result<Value, String> {
        let guard = tokio::select! {
            guard = self.lock.clone().lock_owned() => guard,
            _ = ct.cancelled() => return Err("query cancelled before execution".into()),
        };
        static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_QUERY.fetch_add(1, Ordering::Relaxed);
        let sql = CString::new(sql).map_err(|_| "SQL contains NUL")?;
        let bridge = self.clone();
        let cancellation = ct.child_token();
        // Covers protocol cancellation, timeout, and the SDK dropping this future.
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let mut task = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let done = Arc::new(AtomicBool::new(false));
            let watcher_done = done.clone();
            let watcher_bridge = bridge.clone();
            let watchdog = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                while !watcher_done.load(Ordering::Acquire) {
                    if cancellation.is_cancelled() || Instant::now() >= deadline {
                        // Repeat to cover DuckDB resetting its interrupt flag at query start.
                        unsafe {
                            (watcher_bridge.interrupt)(watcher_bridge.context as *mut c_void, id)
                        };
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            });
            let result = unsafe {
                (bridge.query)(
                    bridge.context as *mut c_void,
                    id,
                    sql.as_ptr(),
                    rows,
                    1024 * 1024,
                    read_only,
                )
            };
            done.store(true, Ordering::Release);
            let _ = watchdog.join();
            if result.is_null() {
                return Err("native query failed".into());
            }
            let parsed: Result<Value, String> =
                serde_json::from_slice(unsafe { CStr::from_ptr(result) }.to_bytes())
                    .map_err(|_| "invalid native query result".to_string());
            unsafe { (bridge.free)(result) };
            let value = parsed?;
            if let Some(error) = value.get("error").and_then(Value::as_str) {
                return Err(error.into());
            }
            Ok(value)
        });
        tokio::select! {
            result = &mut task => result.map_err(|_| "native query task failed".to_string())?,
            _ = ct.cancelled() => Err("query cancelled".into()),
            _ = tokio::time::sleep(Duration::from_secs(30)) => Err("query timed out after 30 seconds".into()),
        }
    }
}

#[derive(Default)]
pub struct Broker {
    sessions: Mutex<HashMap<String, BrowserSession>>,
}
struct PendingCommand<'a> {
    broker: &'a Broker,
    session: String,
    id: String,
}
impl Drop for PendingCommand<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.broker.sessions.lock().unwrap().get_mut(&self.session) {
            if session.pending.remove(&self.id).is_some()
                && !session
                    .queue
                    .iter()
                    .any(|command| command["request_id"] == self.id)
            {
                session.cancelled.push(self.id.clone());
            }
            session
                .queue
                .retain(|command| command["request_id"] != self.id);
        }
    }
}
struct BrowserSession {
    path: Option<String>,
    revision: String,
    seen: Instant,
    queue: VecDeque<Value>,
    cancelled: Vec<String>,
    pending: HashMap<String, oneshot::Sender<Result<Value, String>>>,
}
#[derive(Deserialize)]
pub struct Poll {
    contract_version: Option<u32>,
    session_id: String,
    notebook_path: Option<String>,
    revision: String,
}
#[derive(Deserialize)]
pub struct BrowserResult {
    session_id: String,
    request_id: String,
    result: Option<Value>,
    error: Option<String>,
}
#[derive(Deserialize)]
pub struct Disconnect {
    session_id: String,
}
fn unavailable(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "mcp_bridge", message)
}
fn bridge_auth(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    authenticate(headers, state)?;
    if state.mcp_token.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "MCP is disabled",
        ));
    }
    Ok(())
}
impl Broker {
    fn prune(sessions: &mut HashMap<String, BrowserSession>) {
        sessions.retain(|_, session| session.seen.elapsed() < Duration::from_secs(5));
    }
    fn list(&self) -> Value {
        let mut sessions = self.sessions.lock().unwrap();
        Self::prune(&mut sessions);
        Value::Array(
            sessions
                .iter()
                .map(
                    |(id, s)| json!({"session_id":id,"notebook_path":s.path,"revision":s.revision}),
                )
                .collect(),
        )
    }
    async fn dispatch(&self, args: &Value, ct: CancellationToken) -> Result<Value, String> {
        let session_id = string(args, "session_id")?;
        let revision = string(args, "expected_revision")?;
        let operation = string(args, "operation")?;
        if ![
            "notebook.get",
            "cell.list",
            "notebook.run",
            "cell.insert",
            "cell.update",
            "cell.delete",
            "cell.run",
        ]
        .contains(&operation)
        {
            return Err("unsupported live notebook operation".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut sessions = self.sessions.lock().unwrap();
            Self::prune(&mut sessions);
            let session = sessions
                .get_mut(session_id)
                .ok_or("browser session disconnected")?;
            if args
                .get("notebook_path")
                .is_some_and(|path| *path != json!(session.path))
            {
                return Err("CONFLICT: notebook target changed".into());
            }
            if session.revision != revision {
                return Err("notebook revision changed; read live sessions again".into());
            }
            if !session.pending.is_empty() {
                return Err("browser session already has a pending command".into());
            }
            session.queue.push_back(json!({"contract_version":2,"request_id":id,"session_id":session_id,"notebook_path":session.path,"expected_revision":revision,"operation":operation,"arguments":args.get("arguments").cloned().unwrap_or(json!({}))}));
            session.pending.insert(id.clone(), tx);
        }
        let _pending = PendingCommand {
            broker: self,
            session: session_id.into(),
            id: id.clone(),
        };
        let result = tokio::select! {
            value = rx => value.map_err(|_| "browser disconnected".to_string())?,
            _ = ct.cancelled() => Err("request cancelled; an already running browser query may continue".into()),
            _ = tokio::time::sleep(Duration::from_secs(30)) => Err("browser command timed out; an already running query may continue".into()),
        };
        result
    }
}
pub async fn poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Poll>,
) -> Result<Json<Value>, ApiError> {
    bridge_auth(&headers, &state)?;
    if body.contract_version != Some(2) {
        return Err(unavailable(
            "Incompatible notebook MCP bridge; update the native extension and browser UI together",
        ));
    }
    if body.session_id.is_empty() || body.session_id.len() > 128 || body.revision.len() > 256 {
        return Err(unavailable("invalid session identity"));
    }
    let mut sessions = state.mcp_broker.sessions.lock().unwrap();
    Broker::prune(&mut sessions);
    if !sessions.contains_key(&body.session_id) && sessions.len() >= 32 {
        return Err(unavailable("too many browser sessions"));
    }
    let session = sessions
        .entry(body.session_id)
        .or_insert_with(|| BrowserSession {
            path: None,
            revision: String::new(),
            seen: Instant::now(),
            queue: VecDeque::new(),
            cancelled: Vec::new(),
            pending: HashMap::new(),
        });
    session.path = body.notebook_path;
    session.revision = body.revision;
    session.seen = Instant::now();
    Ok(Json(
        json!({"commands":session.queue.drain(..).collect::<Vec<_>>(), "cancelled_request_ids":std::mem::take(&mut session.cancelled)}),
    ))
}
pub async fn browser_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BrowserResult>,
) -> Result<Json<Value>, ApiError> {
    bridge_auth(&headers, &state)?;
    let mut sessions = state.mcp_broker.sessions.lock().unwrap();
    Broker::prune(&mut sessions);
    let pending = sessions
        .get_mut(&body.session_id)
        .and_then(|s| s.pending.remove(&body.request_id))
        .ok_or_else(|| unavailable("unknown or expired command"))?;
    let result = match body.error {
        Some(error) => Err(error),
        None => body.result.ok_or_else(|| "missing browser result".into()),
    };
    let _ = pending.send(result);
    Ok(Json(json!({"ok":true})))
}
pub async fn disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Disconnect>,
) -> Result<Json<Value>, ApiError> {
    bridge_auth(&headers, &state)?;
    state
        .mcp_broker
        .sessions
        .lock()
        .unwrap()
        .remove(&body.session_id);
    Ok(Json(json!({"ok":true})))
}

#[derive(Clone)]
pub struct McpServer(pub AppState);
fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} must be a string"))
}
impl McpServer {
    pub(super) async fn execute(
        &self,
        name: &str,
        args: Value,
        ct: CancellationToken,
    ) -> Result<Value, String> {
        let definition = definitions()
            .into_iter()
            .find(|d| d.tool.name == name)
            .ok_or("NOT_FOUND: unknown tool")?;
        contract::validate(&definition.tool, &args)?;
        match definition.operation {
            Operation::Query { .. } | Operation::Schema => {
                let read_only =
                    !matches!(definition.operation, Operation::Query { read_only: false });
                let sql = if matches!(definition.operation, Operation::Schema) {
                    "select table_catalog,table_schema,table_name,column_name,data_type,ordinal_position from information_schema.columns order by 1,2,3,6".into()
                } else {
                    string(&args, "sql")?.to_string()
                };
                if sql.len() > 1024 * 1024 {
                    return Err("INVALID_ARGUMENT: SQL exceeds 1 MiB".into());
                }
                if args.get("limit").is_some()
                    && args.get("max_rows").is_some()
                    && args["limit"] != args["max_rows"]
                {
                    return Err("INVALID_ARGUMENT: limit and max_rows disagree".into());
                }
                let rows = args
                    .get("limit")
                    .or_else(|| args.get("max_rows"))
                    .and_then(Value::as_u64)
                    .unwrap_or(100);
                let query = self
                    .0
                    .mcp_query
                    .as_ref()
                    .ok_or("native query bridge unavailable")?;
                let mut value = tokio::select! {
                    result = query.execute(sql, rows, read_only, ct) => result?,
                    _ = self.0.mcp_shutdown.cancelled() => return Err("server stopping".into()),
                };
                value["ok"] = json!(true);
                if let Some(rows) = value.get("rows").and_then(Value::as_array) {
                    value["rowCount"] = json!(rows.len());
                }
                Ok(value)
            }
            Operation::Live(operation) => {
                let sessions = self.0.mcp_broker.list();
                let candidates: Vec<_> = sessions
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|session| {
                        args.get("session_id")
                            .is_none_or(|id| *id == session["session_id"])
                            && args
                                .get("notebook_path")
                                .is_none_or(|path| *path == session["notebook_path"])
                    })
                    .collect();
                if candidates.len() != 1 {
                    return Err(if candidates.is_empty() {
                        "NOT_FOUND: no matching live notebook; open it in the browser first"
                    } else {
                        "CONFLICT: multiple matching notebooks; provide session_id"
                    }
                    .into());
                }
                let session = candidates[0];
                let revision = args
                    .get("expected_revision")
                    .unwrap_or(&session["revision"]);
                let mut arguments = args.as_object().unwrap().clone();
                for key in ["session_id", "expected_revision", "notebook_path"] {
                    arguments.remove(key);
                }
                self.0.mcp_broker.dispatch(&json!({"session_id":session["session_id"],"notebook_path":session["notebook_path"],"expected_revision":revision,"operation":operation,"arguments":arguments}), ct).await
            }
            Operation::List | Operation::SourceRead | Operation::SourceWrite => {
                let store = self.0.notebooks.clone();
                let operation = definition.operation;
                let sessions = self.0.mcp_broker.list();
                if matches!(operation, Operation::SourceWrite) {
                    let path = string(&args, "notebook_path")?;
                    if sessions
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s["notebook_path"] == path)
                    {
                        return Err("CONFLICT: notebook is open in a browser; use notebook cell tools for unsaved edits".into());
                    }
                }
                tokio::task::spawn_blocking(move || -> Result<Value, String> {
                    if matches!(operation, Operation::List) {
                        let listing = store.list().map_err(|error| error.message)?;
                        let paths: Vec<_> = listing.files.iter().map(|file| file.path.clone()).collect();
                        return Ok(json!({"ok":true,"kind":"notebook","operation":"notebook.list","cells":[],"count":paths.len(),"notebooks":paths,"sessions":sessions}));
                    }
                    let path = validate_relative_path(string(&args, "notebook_path")?).map_err(|error| error.message)?;
                    let notebook_path = slash_path(&path).map_err(|error| error.message)?;
                    if matches!(operation, Operation::SourceRead) {
                        let (source, revision) = store.read(&path).map_err(|error| error.message)?;
                        Ok(json!({"ok":true,"notebook_path":notebook_path,"source":source,"revision":revision}))
                    } else {
                        let revision = store.write(&path, string(&args, "source")?.as_bytes(), string(&args, "expected_revision")?).map_err(|error| error.message)?;
                        Ok(json!({"ok":true,"notebook_path":notebook_path,"revision":revision,"mutation":{"saved":true}}))
                    }
                }).await.map_err(|_| "notebook I/O failed".to_string())?
            }
        }
    }
}
impl ServerHandler for McpServer {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Local SQLNB. SQL uses a persistent agent connection shared by MCP clients, separate from browser temporary state. Use live session tools for unsaved notebooks. Never retry a timed-out mutation without inspecting state.")
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: definitions().into_iter().map(|d| d.tool).collect(),
            ..Default::default()
        })
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        definitions()
            .into_iter()
            .map(|d| d.tool)
            .find(|tool| tool.name == name)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let result = self
            .execute(
                &request.name,
                Value::Object(request.arguments.unwrap_or_default()),
                context.ct,
            )
            .await;
        Ok(match result {
            Ok(mut value) if value.get("ok") == Some(&json!(false)) => {
                if value.get("error").is_none() {
                    value["error"] = value.get("cells").and_then(Value::as_array)
                        .and_then(|cells| cells.iter().find_map(|cell| cell.get("error").filter(|error| error.is_object())))
                        .cloned().unwrap_or_else(|| json!({"code":"OPERATION_FAILED","message":"Notebook operation failed"}));
                }
                CallToolResult::structured_error(value)
            }
            Ok(value) => CallToolResult::structured(value),
            Err(error) => {
                let code = error
                    .split_once(':')
                    .map(|(code, _)| code)
                    .filter(|code| {
                        [
                            "INVALID_ARGUMENT",
                            "NOT_FOUND",
                            "CONFLICT",
                            "POLICY_BLOCKED",
                        ]
                        .contains(code)
                    })
                    .unwrap_or_else(|| {
                        if error.contains("POLICY_BLOCKED:") { "POLICY_BLOCKED" } else if error.contains("revision") || error.contains("changed") {
                            "CONFLICT"
                        } else if error.contains("timed out") {
                            "TIMEOUT"
                        } else {
                            "OPERATION_FAILED"
                        }
                    });
                CallToolResult::structured_error(
                    json!({"ok":false,"error":{"code":code,"message":error}}),
                )
            }
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn live_commands_are_revision_checked_delivered_once_and_removed_on_cancellation() {
        let broker = Arc::new(Broker::default());
        broker.sessions.lock().unwrap().insert(
            "browser".into(),
            BrowserSession {
                path: Some("test.sqlnb".into()),
                revision: "r1".into(),
                seen: Instant::now(),
                queue: VecDeque::new(),
                cancelled: Vec::new(),
                pending: HashMap::new(),
            },
        );
        let stale =
            json!({"session_id":"browser","expected_revision":"old","operation":"cell.update"});
        assert!(broker
            .dispatch(&stale, CancellationToken::new())
            .await
            .unwrap_err()
            .contains("revision"));
        let args = json!({"session_id":"browser","expected_revision":"r1","operation":"cell.update","arguments":{"sql":"select 1","cell_index":0}});
        let ct = CancellationToken::new();
        let task_broker = broker.clone();
        let task_ct = ct.clone();
        let task = tokio::spawn(async move { task_broker.dispatch(&args, task_ct).await });
        tokio::task::yield_now().await;
        {
            let mut sessions = broker.sessions.lock().unwrap();
            let session = sessions.get_mut("browser").unwrap();
            assert_eq!(session.queue.drain(..).count(), 1);
            assert_eq!(session.queue.drain(..).count(), 0);
            assert_eq!(session.pending.len(), 1);
        }
        ct.cancel();
        assert!(task.await.unwrap().unwrap_err().contains("cancelled"));
        assert!(broker.sessions.lock().unwrap()["browser"]
            .pending
            .is_empty());
        let args =
            json!({"session_id":"browser","expected_revision":"r1","operation":"notebook.get"});
        let task_broker = broker.clone();
        let task =
            tokio::spawn(
                async move { task_broker.dispatch(&args, CancellationToken::new()).await },
            );
        tokio::task::yield_now().await;
        broker.sessions.lock().unwrap().remove("browser");
        assert!(task.await.unwrap().unwrap_err().contains("disconnected"));
    }
    struct TestQueryState {
        started: AtomicBool,
        interrupted: AtomicBool,
    }

    unsafe extern "C" fn blocked_query(
        context: *mut c_void,
        _: u64,
        _: *const c_char,
        _: u64,
        _: u64,
        _: bool,
    ) -> *mut c_char {
        let state = &*(context as *const TestQueryState);
        state.started.store(true, Ordering::Release);
        while !state.interrupted.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }
        CString::new(r#"{"error":"query cancelled"}"#)
            .unwrap()
            .into_raw()
    }

    unsafe extern "C" fn interrupt_test_query(context: *mut c_void, _: u64) {
        (*(context as *const TestQueryState))
            .interrupted
            .store(true, Ordering::Release);
    }

    unsafe extern "C" fn free_test_result(result: *mut c_char) {
        drop(CString::from_raw(result));
    }

    #[tokio::test]
    async fn native_query_cancellation_interrupts_and_holds_connection_until_complete() {
        let state = Box::new(TestQueryState {
            started: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
        });
        let bridge = QueryBridge {
            context: (&*state as *const TestQueryState) as usize,
            query: blocked_query,
            interrupt: interrupt_test_query,
            free: free_test_result,
            lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let ct = CancellationToken::new();
        let query_bridge = bridge.clone();
        let query_ct = ct.clone();
        let task = tokio::spawn(async move {
            query_bridge
                .execute("select 1".into(), 1, true, query_ct)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.started.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        ct.cancel();
        assert!(task.await.unwrap().unwrap_err().contains("cancelled"));
        // Acquiring the connection gate proves the callback and watchdog have exited.
        let _guard = tokio::time::timeout(Duration::from_secs(2), bridge.lock.lock())
            .await
            .unwrap();
        assert!(state.interrupted.load(Ordering::Acquire));
    }
}
