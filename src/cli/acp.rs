use super::dispatch;
use super::provider_init::ProviderChoice;
use crate::protocol::{Request, ServerEvent};
use crate::transport::{ReadHalf, WriteHalf};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

const ACP_PROTOCOL_VERSION: u64 = 1;

const JSONRPC_PARSE_ERROR: i64 = -32700;
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_PARAMS: i64 = -32602;
const JSONRPC_INTERNAL_ERROR: i64 = -32603;
const JSONRPC_SERVER_ERROR: i64 = -32000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcpProfile {
    Standard,
    Extended,
    Full,
}

impl AcpProfile {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "extended" => Self::Extended,
            "full" => Self::Full,
            _ => Self::Standard,
        }
    }

    fn is_extended(self) -> bool {
        matches!(self, Self::Extended | Self::Full)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Extended => "extended",
            Self::Full => "full",
        }
    }
}

#[derive(Debug)]
struct JsonRpcMessage {
    id: Option<Value>,
    method: Option<String>,
    params: Value,
    /// Present only on a *response* to a request this adapter itself sent
    /// to the ACP host (Phase 3, client-callback plumbing) -- a real ACP
    /// *request* from the host always has `method` set instead. Previously
    /// dropped entirely by this parser, which is exactly why
    /// `handle_message` used to treat every response as a malformed
    /// request ("missing method").
    result: Option<Value>,
    #[allow(dead_code)] // read by route_response; kept for parity with `result`, not yet surfaced elsewhere
    error: Option<Value>,
}

impl JsonRpcMessage {
    fn parse(line: &str) -> std::result::Result<Self, (i64, String)> {
        let value: Value =
            serde_json::from_str(line).map_err(|err| (JSONRPC_PARSE_ERROR, err.to_string()))?;
        let object = value.as_object().ok_or_else(|| {
            (
                JSONRPC_INVALID_REQUEST,
                "JSON-RPC message must be an object".to_string(),
            )
        })?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err((
                JSONRPC_INVALID_REQUEST,
                "JSON-RPC message must include jsonrpc=\"2.0\"".to_string(),
            ));
        }
        Ok(Self {
            id: object.get("id").cloned(),
            method: object
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_string),
            params: object.get("params").cloned().unwrap_or(Value::Null),
            result: object.get("result").cloned(),
            error: object.get("error").cloned(),
        })
    }

    /// True if this message is shaped like a JSON-RPC *response* (has
    /// `result` or `error`, not `method`) rather than a request/notification.
    fn is_response(&self) -> bool {
        self.method.is_none() && (self.result.is_some() || self.error.is_some())
    }
}

struct DaemonSession {
    session_id: String,
    reader: Mutex<BufReader<ReadHalf>>,
    writer: Mutex<WriteHalf>,
    next_request_id: AtomicU64,
    active_prompt_id: Mutex<Option<u64>>,
    prompt_running: AtomicBool,
    ui_state: Mutex<SessionUiState>,
}

/// Session-scoped provider/model state used to surface ACP `configOptions`
/// (model selector, reasoning effort) and `usage_update` notifications.
#[derive(Clone, Debug, Default)]
struct SessionUiState {
    provider_name: Option<String>,
    model: Option<String>,
    available_models: Vec<String>,
    reasoning_effort: Option<String>,
}

#[derive(Debug, Default)]
struct TurnUsage {
    reported: bool,
    input_tokens: u64,
    output_tokens: u64,
    cached_read_tokens: Option<u64>,
    cached_write_tokens: Option<u64>,
}

impl TurnUsage {
    fn add(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cached_read_tokens: Option<u64>,
        cached_write_tokens: Option<u64>,
    ) {
        self.reported = true;
        self.input_tokens = self.input_tokens.saturating_add(input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(output_tokens);
        add_optional_tokens(&mut self.cached_read_tokens, cached_read_tokens);
        add_optional_tokens(&mut self.cached_write_tokens, cached_write_tokens);
    }

    fn to_acp(&self) -> Option<Value> {
        if !self.reported {
            return None;
        }

        let total_tokens = self
            .input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cached_read_tokens.unwrap_or(0))
            .saturating_add(self.cached_write_tokens.unwrap_or(0));
        let mut usage = json!({
            "totalTokens": total_tokens,
            "inputTokens": self.input_tokens,
            "outputTokens": self.output_tokens,
        });
        let object = usage.as_object_mut().expect("usage is an object");
        if let Some(tokens) = self.cached_read_tokens {
            object.insert("cachedReadTokens".to_string(), json!(tokens));
        }
        if let Some(tokens) = self.cached_write_tokens {
            object.insert("cachedWriteTokens".to_string(), json!(tokens));
        }
        Some(usage)
    }
}

fn add_optional_tokens(total: &mut Option<u64>, tokens: Option<u64>) {
    if let Some(tokens) = tokens {
        *total = Some(total.unwrap_or(0).saturating_add(tokens));
    }
}

fn prompt_response(stop_reason: &str, usage: &TurnUsage) -> Value {
    let mut response = json!({ "stopReason": stop_reason });
    if let Some(usage) = usage.to_acp() {
        response
            .as_object_mut()
            .expect("prompt response is an object")
            .insert("usage".to_string(), usage);
    }
    response
}

impl SessionUiState {
    fn from_history_fields(
        provider_name: Option<String>,
        provider_model: Option<String>,
        available_models: Vec<String>,
        reasoning_effort: Option<String>,
    ) -> Self {
        Self {
            provider_name,
            model: provider_model,
            available_models,
            reasoning_effort,
        }
    }

    fn context_limit(&self) -> u64 {
        self.model
            .as_deref()
            .and_then(|model| {
                crate::provider::context_limit_for_model_with_provider(
                    model,
                    self.provider_name.as_deref(),
                )
            })
            .unwrap_or(crate::provider::DEFAULT_CONTEXT_LIMIT) as u64
    }
}

impl DaemonSession {
    fn new(session_id: String, reader: ReadHalf, writer: WriteHalf, next_request_id: u64) -> Self {
        Self {
            session_id,
            reader: Mutex::new(BufReader::new(reader)),
            writer: Mutex::new(writer),
            next_request_id: AtomicU64::new(next_request_id),
            active_prompt_id: Mutex::new(None),
            prompt_running: AtomicBool::new(false),
            ui_state: Mutex::new(SessionUiState::default()),
        }
    }

    fn with_ui_state(self, state: SessionUiState) -> Self {
        Self {
            ui_state: Mutex::new(state),
            ..self
        }
    }

    fn next_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send(&self, request: &Request) -> Result<()> {
        let mut json = serde_json::to_string(request)?;
        json.push('\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(json.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn read_event(&self) -> Result<ServerEvent> {
        let mut line = String::new();
        let mut reader = self.reader.lock().await;
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("Jcode daemon disconnected");
        }
        let event = serde_json::from_str(&line)
            .with_context(|| format!("failed to decode Jcode daemon event: {}", line.trim_end()))?;
        Ok(event)
    }
}

/// One outbound request this adapter sent to the ACP *host* (not the jcode
/// daemon -- see `DaemonSession` for that direction), still awaiting a
/// response. `Ok`/`Err` mirror the JSON-RPC `result`/`error` fields the
/// eventual response line will carry.
type PendingClientRequests = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<std::result::Result<Value, Value>>>>>;

/// Removes `id` from `pending` when dropped, however `send_client_request`'s
/// scope ends -- normal completion, an early `?`-return, or the caller
/// cancelling the whole `Future` (e.g. racing it in a `tokio::select!`).
/// Gemini review, 2026-08-30: without this, a cancelled call leaked its
/// entry forever, since neither the timeout arm nor `handle_message`'s
/// response routing (the two other cleanup paths) ever get to run for a
/// future that was simply dropped mid-await.
///
/// Uses `try_lock` (synchronous -- `Drop::drop` can't be `async`) on
/// `tokio::sync::Mutex`, which supports it directly. Best-effort: if the
/// lock is contested at the exact moment of drop (another task is
/// concurrently completing this same id via `handle_message`, which is
/// itself harmless -- that path already removes the entry), this simply
/// does nothing rather than blocking a destructor, which is the correct
/// tradeoff since the entry is either already gone or about to be.
struct PendingRequestGuard {
    pending: PendingClientRequests,
    id: u64,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.try_lock() {
            pending.remove(&self.id);
        }
    }
}

#[derive(Clone)]
struct AcpRuntime {
    stdout: Arc<Mutex<tokio::io::Stdout>>,
    sessions: Arc<Mutex<HashMap<String, Arc<DaemonSession>>>>,
    profile: AcpProfile,
    provider_choice: ProviderChoice,
    model: Option<String>,
    provider_profile: Option<String>,
    /// Phase 3, ACP client-callback plumbing: lets this adapter send a
    /// request *to* the ACP host (`fs/read_text_file` etc., not yet wired
    /// to any real caller in this slice) and correlate the eventual
    /// response back to the right waiter. Own id space (`next_client_request_id`),
    /// separate from `DaemonSession`'s own ids -- these two directions
    /// (adapter->host, adapter->daemon) never share a wire.
    pending_client_requests: PendingClientRequests,
    next_client_request_id: Arc<AtomicU64>,
}

impl AcpRuntime {
    fn new(
        profile: AcpProfile,
        provider_choice: ProviderChoice,
        model: Option<String>,
        provider_profile: Option<String>,
    ) -> Self {
        Self {
            stdout: Arc::new(Mutex::new(tokio::io::stdout())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            profile,
            provider_choice,
            model,
            provider_profile,
            pending_client_requests: Arc::new(Mutex::new(HashMap::new())),
            next_client_request_id: Arc::new(AtomicU64::new(1)),
        }
    }

    async fn run(self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(());
            }
            if line.trim().is_empty() {
                continue;
            }

            let message = match JsonRpcMessage::parse(&line) {
                Ok(message) => message,
                Err((code, message)) => {
                    self.write_error_value(
                        Value::Null,
                        code,
                        format!("Invalid JSON-RPC request: {message}"),
                    )
                    .await?;
                    continue;
                }
            };

            self.handle_message(message).await?;
        }
    }

    async fn handle_message(&self, message: JsonRpcMessage) -> Result<()> {
        // No `method` means this is either a response to a request *we*
        // sent the host (`send_client_request`), or genuinely malformed.
        // Checked as its own branch, *before* the method match below --
        // Gemini review, 2026-08-30: a response whose id this adapter
        // didn't recognize (e.g. a non-integer id) previously still fell
        // through to "JSON-RPC request missing method", a real reply sent
        // back to the host for something that was never a request at all.
        // Now anything response-shaped is fully handled here -- routed if
        // there's a waiter, silently dropped otherwise (see
        // `route_response`'s own doc) -- and never reaches the error path
        // below, regardless of whether routing actually found a match.
        if message.is_response() {
            if let Some((id, payload)) = route_response(message) {
                let waiter = self.pending_client_requests.lock().await.remove(&id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(payload);
                }
            }
            return Ok(());
        }

        let Some(method) = message.method.as_deref() else {
            if let Some(id) = message.id {
                self.write_error_value(
                    id,
                    JSONRPC_INVALID_REQUEST,
                    "JSON-RPC request missing method".to_string(),
                )
                .await?;
            }
            return Ok(());
        };

        match method {
            "initialize" => {
                if let Some(id) = message.id {
                    self.write_result(id, initialize_result(&message.params, self.profile))
                        .await?;
                }
            }
            "session/new" => self.handle_session_new(message).await?,
            "session/load" => self.handle_session_load(message, true).await?,
            "session/resume" => self.handle_session_load(message, false).await?,
            "session/prompt" => self.handle_session_prompt(message).await?,
            "session/cancel" => self.handle_session_cancel(message).await?,
            "session/close" => self.handle_session_close(message).await?,
            "session/set_config_option" => self.handle_set_config_option(message).await?,
            "session/set_model" => {
                self.handle_compat_config_option(
                    message,
                    CONFIG_ID_MODEL,
                    &["modelId", "model"],
                    "session/set_model",
                )
                .await?
            }
            "session/set_reasoning_effort" => {
                self.handle_compat_config_option(
                    message,
                    CONFIG_ID_EFFORT,
                    &["effort", "reasoningEffort"],
                    "session/set_reasoning_effort",
                )
                .await?
            }
            _ if method.starts_with('_') => {
                if let Some(id) = message.id {
                    self.write_error_value(
                        id,
                        JSONRPC_METHOD_NOT_FOUND,
                        format!("Unsupported Jcode ACP extension method: {method}"),
                    )
                    .await?;
                }
            }
            _ => {
                if let Some(id) = message.id {
                    self.write_error_value(
                        id,
                        JSONRPC_METHOD_NOT_FOUND,
                        format!("Unsupported ACP method: {method}"),
                    )
                    .await?;
                }
            }
        }

        Ok(())
    }

    async fn handle_session_new(&self, message: JsonRpcMessage) -> Result<()> {
        let Some(id) = message.id else {
            return Ok(());
        };
        let cwd = match cwd_from_params(&message.params) {
            Ok(cwd) => cwd,
            Err(err) => {
                self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                    .await?;
                return Ok(());
            }
        };
        if let Err(err) = validate_acp_mcp_servers(&message.params) {
            self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                .await?;
            return Ok(());
        }

        let mcp_servers = parse_acp_mcp_servers(&message.params);
        match self.create_new_session(cwd, mcp_servers).await {
            Ok(session) => {
                let session_id = session.session_id.clone();
                let state = session.ui_state.lock().await.clone();
                self.sessions
                    .lock()
                    .await
                    .insert(session_id.clone(), Arc::new(session));
                let mut result = json!({ "sessionId": session_id });
                insert_session_configuration(&mut result, &state);
                self.write_result(id, result).await?;
                self.write_available_commands(&session_id).await?;
            }
            Err(err) => {
                self.write_error_value(
                    id,
                    JSONRPC_INTERNAL_ERROR,
                    format!("Failed to create Jcode session: {err:#}"),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn handle_session_load(
        &self,
        message: JsonRpcMessage,
        replay_history: bool,
    ) -> Result<()> {
        let Some(id) = message.id else {
            return Ok(());
        };
        let session_id = match required_session_id(&message.params) {
            Ok(session_id) => session_id,
            Err(err) => {
                self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                    .await?;
                return Ok(());
            }
        };
        let cwd = match cwd_from_params(&message.params) {
            Ok(cwd) => cwd,
            Err(err) => {
                self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                    .await?;
                return Ok(());
            }
        };
        if let Err(err) = validate_acp_mcp_servers(&message.params) {
            self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                .await?;
            return Ok(());
        }

        match self
            .attach_existing_session(session_id.clone(), cwd, replay_history)
            .await
        {
            Ok(session) => {
                let state = session.ui_state.lock().await.clone();
                self.sessions
                    .lock()
                    .await
                    .insert(session.session_id.clone(), Arc::new(session));
                let mut result = json!({});
                insert_session_configuration(&mut result, &state);
                self.write_result(id, result).await?;
                self.write_available_commands(&session_id).await?;
            }
            Err(err) => {
                self.write_error_value(
                    id,
                    JSONRPC_INTERNAL_ERROR,
                    format!("Failed to attach Jcode session '{session_id}': {err:#}"),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn handle_session_prompt(&self, message: JsonRpcMessage) -> Result<()> {
        let Some(id) = message.id else {
            return Ok(());
        };
        let session_id = match required_session_id(&message.params) {
            Ok(session_id) => session_id,
            Err(err) => {
                self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                    .await?;
                return Ok(());
            }
        };
        let (text, images) = match prompt_from_params(&message.params) {
            Ok(prompt) => prompt,
            Err(err) => {
                self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                    .await?;
                return Ok(());
            }
        };
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_id).cloned()
        };
        let Some(session) = session else {
            self.write_error_value(
                id,
                JSONRPC_INVALID_PARAMS,
                format!("Unknown ACP session id: {session_id}"),
            )
            .await?;
            return Ok(());
        };

        if session
            .prompt_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.write_error_value(
                id,
                JSONRPC_SERVER_ERROR,
                format!("Session {session_id} is already processing a prompt"),
            )
            .await?;
            return Ok(());
        }

        let runtime = self.clone();
        tokio::spawn(async move {
            let result = runtime.run_prompt(id.clone(), session, text, images).await;
            if let Err(err) = result {
                let _ = runtime
                    .write_error_value(
                        id,
                        JSONRPC_INTERNAL_ERROR,
                        format!("Prompt failed: {err:#}"),
                    )
                    .await;
            }
        });
        Ok(())
    }

    async fn handle_session_cancel(&self, message: JsonRpcMessage) -> Result<()> {
        let session_id = match required_session_id(&message.params) {
            Ok(session_id) => session_id,
            Err(err) => {
                if let Some(id) = message.id {
                    self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                        .await?;
                }
                return Ok(());
            }
        };
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_id).cloned()
        };
        if let Some(session) = session {
            let cancel_id = session.next_id();
            let _ = session.send(&Request::Cancel { id: cancel_id }).await;
        }
        if let Some(id) = message.id {
            self.write_result(id, json!({})).await?;
        }
        Ok(())
    }

    async fn handle_session_close(&self, message: JsonRpcMessage) -> Result<()> {
        let Some(id) = message.id else {
            return Ok(());
        };
        let session_id = match required_session_id(&message.params) {
            Ok(session_id) => session_id,
            Err(err) => {
                self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                    .await?;
                return Ok(());
            }
        };
        if let Some(session) = self.sessions.lock().await.remove(&session_id) {
            let cancel_id = session.next_id();
            let _ = session.send(&Request::Cancel { id: cancel_id }).await;
        }
        self.write_result(id, json!({})).await?;
        Ok(())
    }

    async fn handle_set_config_option(&self, message: JsonRpcMessage) -> Result<()> {
        let Some(id) = message.id else {
            return Ok(());
        };
        let session_id = match required_session_id(&message.params) {
            Ok(session_id) => session_id,
            Err(err) => {
                self.write_error_value(id, JSONRPC_INVALID_PARAMS, err)
                    .await?;
                return Ok(());
            }
        };
        let config_id = message
            .params
            .get("configId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let value = message
            .params
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string);
        let (Some(config_id), Some(value)) = (config_id, value) else {
            self.write_error_value(
                id,
                JSONRPC_INVALID_PARAMS,
                "session/set_config_option requires string configId and value".to_string(),
            )
            .await?;
            return Ok(());
        };

        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_id).cloned()
        };
        let Some(session) = session else {
            self.write_error_value(
                id,
                JSONRPC_INVALID_PARAMS,
                format!("Unknown ACP session id: {session_id}"),
            )
            .await?;
            return Ok(());
        };
        if session.prompt_running.load(Ordering::SeqCst) {
            self.write_error_value(
                id,
                JSONRPC_SERVER_ERROR,
                format!("Session {session_id} is processing a prompt; retry when it finishes"),
            )
            .await?;
            return Ok(());
        }

        let request_id = session.next_id();
        let apply_result = match config_id.as_str() {
            CONFIG_ID_MODEL => {
                session
                    .send(&Request::SetModel {
                        id: request_id,
                        model: value.clone(),
                    })
                    .await?;
                wait_for_model_changed(&session, request_id).await
            }
            CONFIG_ID_EFFORT => {
                session
                    .send(&Request::SetReasoningEffort {
                        id: request_id,
                        effort: value.clone(),
                        target_session_id: None,
                    })
                    .await?;
                wait_for_effort_changed(&session, request_id).await
            }
            other => Err(anyhow::anyhow!("Unknown config option id: {other}")),
        };

        match apply_result {
            Ok(()) => {
                let config_options = session_config_options(&*session.ui_state.lock().await);
                // The spec requires the full option set in the response itself.
                self.write_result(id, json!({ "configOptions": config_options }))
                    .await?;
                self.write_notification(
                    "session/update",
                    json!({
                        "sessionId": session.session_id,
                        "update": {
                            "sessionUpdate": "config_option_update",
                            "configOptions": config_options,
                        }
                    }),
                )
                .await?;
            }
            Err(err) => {
                self.write_error_value(
                    id,
                    JSONRPC_SERVER_ERROR,
                    format!("Failed to set {config_id}: {err:#}"),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Compatibility entry points used by ACP hosts that implemented the
    /// pre-configOptions model and reasoning controls. Normalize them through
    /// the standard config option path so both interfaces stay in sync.
    async fn handle_compat_config_option(
        &self,
        mut message: JsonRpcMessage,
        config_id: &str,
        value_fields: &[&str],
        method: &str,
    ) -> Result<()> {
        let value = match compatibility_option_value(&message.params, value_fields, method) {
            Ok(value) => value,
            Err(error) => {
                if let Some(id) = message.id {
                    self.write_error_value(id, JSONRPC_INVALID_PARAMS, error)
                        .await?;
                }
                return Ok(());
            }
        };
        let Some(params) = message.params.as_object_mut() else {
            if let Some(id) = message.id {
                self.write_error_value(
                    id,
                    JSONRPC_INVALID_PARAMS,
                    format!("{method} params must be an object"),
                )
                .await?;
            }
            return Ok(());
        };
        params.insert("configId".to_string(), Value::String(config_id.to_string()));
        params.insert("value".to_string(), Value::String(value));
        self.handle_set_config_option(message).await
    }

    async fn write_available_commands(&self, session_id: &str) -> Result<()> {
        self.write_notification(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": acp_available_commands(),
                }
            }),
        )
        .await
    }

    async fn ensure_daemon(&self) -> Result<()> {
        if dispatch::server_is_running().await {
            return Ok(());
        }
        dispatch::spawn_server(
            &self.provider_choice,
            self.model.as_deref(),
            self.provider_profile.as_deref(),
        )
        .await
    }

    async fn connect_daemon(&self) -> Result<(ReadHalf, WriteHalf)> {
        self.ensure_daemon().await?;
        let stream = crate::server::connect_socket(&crate::server::socket_path()).await?;
        Ok(stream.into_split())
    }

    async fn create_new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<AcpMcpServerSpec>,
    ) -> Result<DaemonSession> {
        let (reader, writer) = self.connect_daemon().await?;
        let session = DaemonSession::new(String::new(), reader, writer, 2);
        let subscribe_id = 1;
        session
            .send(&Request::Subscribe {
                crash_on_disconnect: false,
                id: subscribe_id,
                working_dir: Some(cwd.display().to_string()),
                selfdev: None,
                target_session_id: None,
                client_instance_id: Some("acp".to_string()),
                client_has_local_history: false,
                allow_session_takeover: false,
                terminal_env: crate::terminal_launch::snapshot_client_terminal_env(),
            })
            .await?;
        wait_for_done(&session, subscribe_id).await?;
        let history = request_history(&session).await?;
        let (session_id, ui_state) = match history {
            ServerEvent::History {
                session_id,
                provider_name,
                provider_model,
                available_models,
                reasoning_effort,
                ..
            } => (
                session_id,
                SessionUiState::from_history_fields(
                    provider_name,
                    provider_model,
                    available_models,
                    reasoning_effort,
                ),
            ),
            other => anyhow::bail!("expected history after session creation, got {other:?}"),
        };
        // Session-scoped MCP servers (ACP's own `mcpServers`, Phase 3 item
        // #7): connect each on this same daemon connection, before handing
        // the session back to the client, so a tool call the client makes
        // immediately after `session/new` returns can already see them.
        // Fail-soft per server, matching `validate_acp_mcp_servers`'s own
        // "don't reject the session over this" stance -- one bad server
        // config shouldn't take down session creation for the rest.
        //
        // Gemini review, 2026-08-30: sequential + unbounded `wait_for_done`
        // meant one hanging MCP server subprocess (bad handshake, waiting on
        // stdin, etc.) would block `session/new` indefinitely -- likely
        // outlasting the ACP host's own request timeout. Each connect now
        // gets a bounded wait; a timeout is reported and skipped, not left
        // to hang the whole session. Sequential dispatch (not concurrent)
        // is kept deliberately: the daemon processes one client request at
        // a time per connection, so true concurrency here would need the
        // server-side handler to be spawned rather than awaited -- a larger
        // change than this slice's scope.
        const MCP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
        for server in mcp_servers {
            let request_id = session.next_id();
            let name = server.name.clone();
            let send_result = session
                .send(&Request::McpConnectServer {
                    id: request_id,
                    server: server.name,
                    command: server.command,
                    args: server.args,
                    env: server.env,
                })
                .await;
            if let Err(err) = send_result {
                crate::logging::warn(&format!(
                    "ACP session/new: failed to send mcp connect request for '{name}': {err:#}"
                ));
                continue;
            }
            match tokio::time::timeout(MCP_CONNECT_TIMEOUT, wait_for_done(&session, request_id))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    crate::logging::warn(&format!(
                        "ACP session/new: mcp server '{name}' failed to connect: {err:#}"
                    ));
                }
                Err(_) => {
                    crate::logging::warn(&format!(
                        "ACP session/new: mcp server '{name}' timed out connecting after {MCP_CONNECT_TIMEOUT:?}, skipping"
                    ));
                }
            }
        }
        Ok(DaemonSession::new(
            session_id,
            session.reader.into_inner().into_inner(),
            session.writer.into_inner(),
            session.next_request_id.load(Ordering::Relaxed),
        )
        .with_ui_state(ui_state))
    }

    async fn attach_existing_session(
        &self,
        target_session_id: String,
        cwd: PathBuf,
        replay_history: bool,
    ) -> Result<DaemonSession> {
        let (reader, writer) = self.connect_daemon().await?;
        let session = DaemonSession::new(String::new(), reader, writer, 2);
        let resume_id = 1;
        session
            .send(&Request::Subscribe {
                crash_on_disconnect: false,
                id: resume_id,
                working_dir: Some(cwd.display().to_string()),
                selfdev: None,
                target_session_id: Some(target_session_id.clone()),
                client_instance_id: Some("acp".to_string()),
                client_has_local_history: false,
                allow_session_takeover: false,
                terminal_env: crate::terminal_launch::snapshot_client_terminal_env(),
            })
            .await?;

        let mut attached_id = target_session_id;
        let mut ui_state = SessionUiState::default();
        loop {
            let event = session.read_event().await?;
            match event {
                ServerEvent::Ack { .. } => {}
                ServerEvent::History {
                    session_id,
                    messages,
                    provider_name,
                    provider_model,
                    available_models,
                    reasoning_effort,
                    ..
                } => {
                    attached_id = session_id.clone();
                    ui_state = SessionUiState::from_history_fields(
                        provider_name,
                        provider_model,
                        available_models,
                        reasoning_effort,
                    );
                    if replay_history {
                        self.replay_history(&session_id, messages).await?;
                    }
                }
                ServerEvent::Done { id } if id == resume_id => break,
                ServerEvent::Error { id, message, .. } if id == resume_id => {
                    anyhow::bail!(message);
                }
                other => {
                    if self.profile.is_extended() {
                        self.write_jcode_extension_event(&attached_id, &other)
                            .await?;
                    }
                }
            }
        }

        Ok(DaemonSession::new(
            attached_id,
            session.reader.into_inner().into_inner(),
            session.writer.into_inner(),
            session.next_request_id.load(Ordering::Relaxed),
        )
        .with_ui_state(ui_state))
    }

    async fn replay_history(
        &self,
        session_id: &str,
        messages: Vec<crate::protocol::HistoryMessage>,
    ) -> Result<()> {
        for message in messages {
            let update_name = match message.role.as_str() {
                "user" => "user_message_chunk",
                "assistant" => "agent_message_chunk",
                _ => "agent_message_chunk",
            };
            self.write_notification(
                "session/update",
                json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": update_name,
                        "content": {
                            "type": "text",
                            "text": message.content,
                        }
                    }
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn run_prompt(
        &self,
        rpc_id: Value,
        session: Arc<DaemonSession>,
        text: String,
        images: Vec<(String, String)>,
    ) -> Result<()> {
        if let Some(command) = parse_acp_slash_command(&text) {
            let response = match command {
                Ok(command) => self.run_session_command(&session, command).await,
                Err(err) => Err(err),
            };
            cleanup_prompt_state(&session).await;
            let response = response?;
            self.write_notification(
                "session/update",
                json!({
                    "sessionId": session.session_id,
                    "update": agent_message_chunk(response),
                }),
            )
            .await?;
            self.write_result(rpc_id, prompt_response("end_turn", &TurnUsage::default()))
                .await?;
            return Ok(());
        }

        let prompt_id = session.next_id();
        {
            let mut active = session.active_prompt_id.lock().await;
            *active = Some(prompt_id);
        }

        let send_result = session
            .send(&Request::Message {
                id: prompt_id,
                content: text,
                images,
                system_reminder: None,
                active_skill: None,
                no_reply: false,
            })
            .await;
        if let Err(err) = send_result {
            cleanup_prompt_state(&session).await;
            return Err(err);
        }

        let mut mapper = EventMapper::new(session.session_id.clone(), self.profile);
        let mut stop_reason = "end_turn".to_string();
        let mut turn_usage = TurnUsage::default();
        loop {
            let event = match session.read_event().await {
                Ok(event) => event,
                Err(err) => {
                    cleanup_prompt_state(&session).await;
                    return Err(err);
                }
            };
            if self.profile.is_extended() {
                self.write_jcode_extension_event(&session.session_id, &event)
                    .await?;
            }
            match event {
                ServerEvent::Ack { .. } => {}
                ServerEvent::Done { id } if id == prompt_id => break,
                ServerEvent::Interrupted => {
                    stop_reason = "cancelled".to_string();
                }
                ServerEvent::Error { id, message, .. } if id == prompt_id => {
                    cleanup_prompt_state(&session).await;
                    self.write_error_value(rpc_id, JSONRPC_SERVER_ERROR, message)
                        .await?;
                    return Ok(());
                }
                ServerEvent::TokenUsage {
                    input,
                    output,
                    cache_read_input,
                    cache_creation_input,
                } => {
                    turn_usage.add(input, output, cache_read_input, cache_creation_input);
                    let (provider_name, context_limit) = {
                        let state = session.ui_state.lock().await;
                        (
                            state.provider_name.clone().unwrap_or_default(),
                            state.context_limit(),
                        )
                    };
                    let used = crate::compaction::effective_context_tokens_from_usage(
                        &provider_name,
                        input,
                        cache_read_input,
                        cache_creation_input,
                    );
                    self.write_notification(
                        "session/update",
                        json!({
                            "sessionId": session.session_id,
                            "update": {
                                "sessionUpdate": "usage_update",
                                "used": used,
                                "size": context_limit,
                            }
                        }),
                    )
                    .await?;
                }
                ServerEvent::ModelChanged {
                    model,
                    provider_name,
                    error,
                    ..
                } => {
                    // Mid-prompt model changes happen on provider failover;
                    // keep the selector in sync.
                    if error.is_none() {
                        let config_options = {
                            let mut state = session.ui_state.lock().await;
                            state.model = Some(model);
                            if provider_name.is_some() {
                                state.provider_name = provider_name;
                            }
                            session_config_options(&state)
                        };
                        if !config_options.is_empty() {
                            self.write_notification(
                                "session/update",
                                json!({
                                    "sessionId": session.session_id,
                                    "update": {
                                        "sessionUpdate": "config_option_update",
                                        "configOptions": config_options,
                                    }
                                }),
                            )
                            .await?;
                        }
                    }
                }
                other => {
                    for update in mapper.map_event(other) {
                        self.write_notification(
                            "session/update",
                            json!({
                                "sessionId": session.session_id,
                                "update": update,
                            }),
                        )
                        .await?;
                    }
                }
            }
        }

        cleanup_prompt_state(&session).await;
        self.write_result(rpc_id, prompt_response(&stop_reason, &turn_usage))
            .await?;
        Ok(())
    }

    async fn run_session_command(
        &self,
        session: &DaemonSession,
        command: AcpSlashCommand,
    ) -> Result<String> {
        match command {
            AcpSlashCommand::Model(None) => {
                let state = session.ui_state.lock().await;
                Ok(match state.model.as_deref() {
                    Some(model) => format!("Current model: `{model}`"),
                    None => "The daemon did not report a current model.".to_string(),
                })
            }
            AcpSlashCommand::Model(Some(model)) => {
                let id = session.next_id();
                session
                    .send(&Request::SetModel {
                        id,
                        model: model.clone(),
                    })
                    .await?;
                wait_for_model_changed(session, id).await?;
                self.write_config_option_update(session).await?;
                let selected = session.ui_state.lock().await.model.clone().unwrap_or(model);
                Ok(format!("Switched model to `{selected}`."))
            }
            AcpSlashCommand::Models => {
                let event = request_model_catalog(session).await?;
                let ServerEvent::History {
                    provider_name,
                    provider_model,
                    available_models,
                    ..
                } = event
                else {
                    unreachable!("request_model_catalog only returns history")
                };
                let (current, models) = {
                    let mut state = session.ui_state.lock().await;
                    if provider_name.is_some() {
                        state.provider_name = provider_name;
                    }
                    if provider_model.is_some() {
                        state.model = provider_model;
                    }
                    state.available_models = available_models;
                    (state.model.clone(), state.available_models.clone())
                };
                self.write_config_option_update(session).await?;
                Ok(format_model_catalog(current.as_deref(), &models))
            }
            AcpSlashCommand::Effort(None) => {
                let state = session.ui_state.lock().await;
                let current = state
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("provider default");
                let available = available_efforts(&state);
                if available.is_empty() {
                    Ok(format!("Current reasoning effort: `{current}`."))
                } else {
                    Ok(format!(
                        "Current reasoning effort: `{current}`. Available: {}.",
                        available
                            .iter()
                            .map(|effort| format!("`{effort}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
            AcpSlashCommand::Effort(Some(effort)) => {
                let id = session.next_id();
                session
                    .send(&Request::SetReasoningEffort {
                        id,
                        effort: effort.clone(),
                        target_session_id: None,
                    })
                    .await?;
                wait_for_effort_changed(session, id).await?;
                self.write_config_option_update(session).await?;
                let selected = session
                    .ui_state
                    .lock()
                    .await
                    .reasoning_effort
                    .clone()
                    .unwrap_or(effort);
                Ok(format!("Set reasoning effort to `{selected}`."))
            }
        }
    }

    async fn write_config_option_update(&self, session: &DaemonSession) -> Result<()> {
        let config_options = session_config_options(&*session.ui_state.lock().await);
        self.write_notification(
            "session/update",
            json!({
                "sessionId": session.session_id,
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": config_options,
                }
            }),
        )
        .await
    }

    async fn write_result(&self, id: Value, result: Value) -> Result<()> {
        self.write_value(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .await
    }

    async fn write_error_value(&self, id: Value, code: i64, message: String) -> Result<()> {
        self.write_value(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            }
        }))
        .await
    }

    async fn write_notification(&self, method: &str, params: Value) -> Result<()> {
        self.write_value(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    /// Send a request *to the ACP host* (not the jcode daemon) and await
    /// its response -- the plumbing Phase 3's client-callback delegation
    /// (`fs/read_text_file`, `session/request_permission`, `terminal/*`)
    /// needs, built here as its own isolated, tested slice with **no real
    /// caller wired up yet** (deliberately -- see `PROGRESS.md`'s scoping
    /// note on why the actual `WriteTool`/`ReadTool` delegation is a
    /// separate, larger slice: those tools run in the daemon process, a
    /// different process from this adapter).
    ///
    /// Safe to call while other messages are being handled: `run()`'s main
    /// read loop already spawns long-running handlers (`handle_session_prompt`
    /// spawns `run_prompt`) rather than blocking on them, precisely so it
    /// stays free to read the next line -- including this request's
    /// eventual response -- while a caller awaits this method. Times out
    /// after `timeout` rather than hanging forever if the host never
    /// replies, the same "don't trust an external party to always answer"
    /// discipline the daemon-side MCP-connect timeout already applies in
    /// the other direction (`create_new_session`).
    ///
    /// **Known gap, not fixed here (Gemini review, 2026-08-30)**: `timeout`
    /// only wraps waiting for the response, not the initial `write_value`
    /// call below -- if the host stops reading its stdin entirely (pipe
    /// buffer fills), that write could itself block past `timeout`. Not
    /// specific to this function: every `write_value` call anywhere in this
    /// file has the same exposure (`write_result`, `write_notification`,
    /// etc.), all pre-existing. A real fix means timing out writes
    /// file-wide, a larger, separate change -- not narrowly special-cased
    /// for just this one caller.
    #[allow(
        dead_code,
        reason = "no real caller yet by design this slice -- exercised directly by this module's own tests; wiring an actual fs/permission/terminal callback through it is deliberately separate follow-up work, see PROGRESS.md"
    )]
    async fn send_client_request(
        &self,
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let id = self.next_client_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = self.pending_client_requests.lock().await;
            pending.insert(id, tx);
        }
        // Gemini review, 2026-08-30: without this, a caller that drops this
        // method's own `Future` before it resolves (e.g. racing it inside
        // an outer `tokio::select!` or an unrelated cancellation) would
        // leak this entry forever -- neither the timeout branch below nor
        // `handle_message`'s response-routing would ever run to clean it
        // up, since the future doing that cleanup is exactly what got
        // dropped. The guard's `Drop` runs regardless of *how* this
        // function's scope ends, cancellation included.
        let _cleanup_guard = PendingRequestGuard {
            pending: self.pending_client_requests.clone(),
            id,
        };

        self.write_value(build_client_request(id, method, params))
            .await?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(anyhow::anyhow!(
                "ACP host returned an error for '{method}': {error}"
            )),
            Ok(Err(_)) => Err(anyhow::anyhow!(
                "internal: pending request for '{method}' (id {id}) was dropped without a response"
            )),
            Err(_) => Err(anyhow::anyhow!(
                "ACP host did not respond to '{method}' within {timeout:?}"
            )),
        }
    }

    async fn write_jcode_extension_event(
        &self,
        session_id: &str,
        event: &ServerEvent,
    ) -> Result<()> {
        self.write_notification(
            "_jcode/server_event",
            json!({
                "sessionId": session_id,
                "event": serde_json::to_value(event).unwrap_or(Value::Null),
            }),
        )
        .await
    }

    async fn write_value(&self, value: Value) -> Result<()> {
        let mut stdout = self.stdout.lock().await;
        let mut line = serde_json::to_string(&value)?;
        line.push('\n');
        stdout.write_all(line.as_bytes()).await?;
        stdout.flush().await?;
        Ok(())
    }
}

async fn cleanup_prompt_state(session: &DaemonSession) {
    {
        let mut active = session.active_prompt_id.lock().await;
        *active = None;
    }
    session.prompt_running.store(false, Ordering::SeqCst);
}

async fn wait_for_done(session: &DaemonSession, request_id: u64) -> Result<()> {
    loop {
        match session.read_event().await? {
            ServerEvent::Ack { .. } => {}
            ServerEvent::Done { id } if id == request_id => return Ok(()),
            ServerEvent::Error { id, message, .. } if id == request_id => anyhow::bail!(message),
            _ => {}
        }
    }
}

async fn request_history(session: &DaemonSession) -> Result<ServerEvent> {
    let id = session.next_id();
    session.send(&Request::GetHistory { id }).await?;
    loop {
        match session.read_event().await? {
            ServerEvent::Ack { .. } => {}
            event @ ServerEvent::History { id: event_id, .. } if event_id == id => {
                return Ok(event);
            }
            ServerEvent::Error {
                id: event_id,
                message,
                ..
            } if event_id == id => anyhow::bail!(message),
            _ => {}
        }
    }
}

async fn request_model_catalog(session: &DaemonSession) -> Result<ServerEvent> {
    let id = session.next_id();
    session.send(&Request::GetModelCatalog { id }).await?;
    loop {
        match session.read_event().await? {
            ServerEvent::Ack { .. } => {}
            event @ ServerEvent::History { id: event_id, .. } if event_id == id => {
                return Ok(event);
            }
            ServerEvent::Error {
                id: event_id,
                message,
                ..
            } if event_id == id => anyhow::bail!(message),
            _ => {}
        }
    }
}

const CONFIG_ID_MODEL: &str = "model";
const CONFIG_ID_EFFORT: &str = "reasoning_effort";

fn acp_available_commands() -> Vec<Value> {
    vec![
        json!({
            "name": "model",
            "description": "Switch the model for this session, or show the current model",
            "input": { "hint": "model id (optional)" },
        }),
        json!({
            "name": "models",
            "description": "List models available from the active provider",
        }),
        json!({
            "name": "effort",
            "description": "Set reasoning effort, or show the current effort",
            "input": { "hint": "none|minimal|low|medium|high|xhigh|max (optional)" },
        }),
    ]
}

fn insert_session_configuration(result: &mut Value, state: &SessionUiState) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    let config_options = session_config_options(state);
    if !config_options.is_empty() {
        object.insert("configOptions".to_string(), Value::Array(config_options));
    }
    if let Some(models) = session_models(state) {
        object.insert("models".to_string(), models);
    }
}

fn session_models(state: &SessionUiState) -> Option<Value> {
    let current = state.model.as_deref()?;
    let mut models = state.available_models.clone();
    if !models.iter().any(|candidate| candidate == current) {
        models.insert(0, current.to_string());
    }
    Some(json!({
        "availableModels": models
            .into_iter()
            .map(|model| json!({ "modelId": model, "name": model }))
            .collect::<Vec<_>>(),
        "currentModelId": current,
    }))
}

fn available_efforts(state: &SessionUiState) -> Vec<&'static str> {
    crate::provider::inferred_reasoning_efforts(
        state.provider_name.as_deref(),
        state.model.as_deref(),
    )
    .into_iter()
    // `swarm`/`swarm-deep` are TUI sentinels, not provider effort levels.
    .filter(|effort| !effort.starts_with("swarm"))
    .collect()
}

/// Build the ACP `configOptions` array (model selector plus reasoning effort)
/// from the current session provider state. Empty when the daemon reported no
/// usable model state.
fn session_config_options(state: &SessionUiState) -> Vec<Value> {
    let mut options = Vec::new();

    if let Some(model) = state.model.as_deref() {
        let mut models = state.available_models.clone();
        if !models.iter().any(|candidate| candidate == model) {
            models.insert(0, model.to_string());
        }
        let select_options: Vec<Value> = models
            .iter()
            .map(|name| json!({ "value": name, "name": name }))
            .collect();
        options.push(json!({
            "type": "select",
            "id": CONFIG_ID_MODEL,
            "name": "Model",
            "category": "model",
            "currentValue": model,
            "options": select_options,
        }));
    }

    let efforts = available_efforts(state);
    if !efforts.is_empty() {
        let current = state
            .reasoning_effort
            .as_deref()
            .filter(|effort| efforts.contains(effort))
            .unwrap_or_else(|| {
                if efforts.contains(&"medium") {
                    "medium"
                } else {
                    efforts[0]
                }
            });
        let select_options: Vec<Value> = efforts
            .iter()
            .map(|name| json!({ "value": name, "name": name }))
            .collect();
        options.push(json!({
            "type": "select",
            "id": CONFIG_ID_EFFORT,
            "name": "Reasoning effort",
            "category": "thought_level",
            "currentValue": current,
            "options": select_options,
        }));
    }

    options
}

async fn wait_for_model_changed(session: &DaemonSession, request_id: u64) -> Result<()> {
    loop {
        match session.read_event().await? {
            ServerEvent::Ack { .. } => {}
            ServerEvent::ModelChanged {
                id,
                model,
                provider_name,
                error,
            } if id == request_id => {
                if let Some(error) = error {
                    anyhow::bail!(error);
                }
                let mut state = session.ui_state.lock().await;
                state.model = Some(model);
                if provider_name.is_some() {
                    state.provider_name = provider_name;
                }
                return Ok(());
            }
            ServerEvent::Error { id, message, .. } if id == request_id => {
                anyhow::bail!(message)
            }
            _ => {}
        }
    }
}

async fn wait_for_effort_changed(session: &DaemonSession, request_id: u64) -> Result<()> {
    loop {
        match session.read_event().await? {
            ServerEvent::Ack { .. } => {}
            ServerEvent::ReasoningEffortChanged { id, effort, error } if id == request_id => {
                if let Some(error) = error {
                    anyhow::bail!(error);
                }
                let mut state = session.ui_state.lock().await;
                state.reasoning_effort = effort;
                return Ok(());
            }
            ServerEvent::Error { id, message, .. } if id == request_id => {
                anyhow::bail!(message)
            }
            _ => {}
        }
    }
}

struct EventMapper {
    session_id: String,
    profile: AcpProfile,
    current_tool_id: Option<String>,
    tool_inputs: HashMap<String, String>,
}

impl EventMapper {
    fn new(session_id: String, profile: AcpProfile) -> Self {
        Self {
            session_id,
            profile,
            current_tool_id: None,
            tool_inputs: HashMap::new(),
        }
    }

    fn map_event(&mut self, event: ServerEvent) -> Vec<Value> {
        match event {
            ServerEvent::TextDelta { text } => vec![agent_message_chunk(text)],
            ServerEvent::TextReplace { text } => vec![agent_message_chunk(text)],
            ServerEvent::ToolStart { id, name } => {
                self.current_tool_id = Some(id.clone());
                self.tool_inputs.entry(id.clone()).or_default();
                vec![json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "title": tool_title(&name),
                    "kind": tool_kind(&name),
                    "status": "pending",
                })]
            }
            ServerEvent::ToolInput { delta } => {
                let Some(tool_id) = self.current_tool_id.clone() else {
                    return Vec::new();
                };
                let buffer = self.tool_inputs.entry(tool_id.clone()).or_default();
                buffer.push_str(&delta);
                let mut update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_id,
                });
                if let Some(raw_input) = parse_json_object(buffer)
                    && let Some(object) = update.as_object_mut()
                {
                    object.insert("rawInput".to_string(), raw_input);
                }
                vec![update]
            }
            ServerEvent::ToolExec { id, name } => {
                self.current_tool_id = Some(id.clone());
                let mut update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "title": tool_title(&name),
                    "kind": tool_kind(&name),
                    "status": "in_progress",
                });
                if let Some(input) = self
                    .tool_inputs
                    .get(update["toolCallId"].as_str().unwrap_or_default())
                    && let Some(raw_input) = parse_json_object(input)
                    && let Some(object) = update.as_object_mut()
                {
                    object.insert("rawInput".to_string(), raw_input);
                }
                vec![update]
            }
            ServerEvent::ToolDone {
                id,
                name,
                output,
                error,
            } => vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": id,
                "title": tool_title(&name),
                "kind": tool_kind(&name),
                "status": if error.is_some() { "failed" } else { "completed" },
                "content": [{
                    "type": "content",
                    "content": {
                        "type": "text",
                        "text": output,
                    }
                }],
                "rawOutput": {
                    "output": output,
                    "error": error,
                }
            })],
            ServerEvent::GeneratedImage {
                id,
                path,
                output_format,
                revised_prompt,
                ..
            } => vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": id,
                "status": "completed",
                "content": [{
                    "type": "content",
                    "content": {
                        "type": "text",
                        "text": format!("Generated image: {path} ({output_format}){}", revised_prompt.map(|prompt| format!("\nRevised prompt: {prompt}")).unwrap_or_default()),
                    }
                }]
            })],
            ServerEvent::Compaction { trigger, .. } if self.profile.is_extended() => vec![json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {
                    "type": "text",
                    "text": format!("\n[Jcode compacted context: {trigger}]\n"),
                }
            })],
            ServerEvent::SessionRenamed { display_title, .. } => vec![json!({
                "sessionUpdate": "session_info_update",
                "title": display_title,
            })],
            ServerEvent::McpStatus { servers } if self.profile.is_extended() => vec![json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {
                    "type": "text",
                    "text": format!("\n[Jcode MCP status: {}]\n", servers.join(", ")),
                }
            })],
            _ => {
                let _ = &self.session_id;
                Vec::new()
            }
        }
    }
}

fn parse_json_object(input: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(input).ok()?;
    value.as_object()?;
    Some(value)
}

fn compatibility_option_value(
    params: &Value,
    value_fields: &[&str],
    method: &str,
) -> std::result::Result<String, String> {
    if !params.is_object() {
        return Err(format!("{method} params must be an object"));
    }
    value_fields
        .iter()
        .find_map(|field| {
            params
                .get(*field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| {
            format!(
                "{method} requires a non-empty string {}",
                value_fields.join(" or ")
            )
        })
}

fn initialize_result(params: &Value, profile: AcpProfile) -> Value {
    // We only speak exactly ACP_PROTOCOL_VERSION; the response pins to our
    // version regardless of the `protocolVersion` the client requested.
    let _ = params;
    let protocol_version = ACP_PROTOCOL_VERSION;
    let mut agent_capabilities = json!({
        "loadSession": true,
        "promptCapabilities": {
            "image": true,
            "audio": false,
            "embeddedContext": true,
        },
        "mcpCapabilities": {
            "http": false,
            "sse": false,
        },
        "sessionCapabilities": {
            "close": {},
            "resume": {},
        }
    });

    if profile.is_extended()
        && let Some(object) = agent_capabilities.as_object_mut()
    {
        object.insert(
            "_meta".to_string(),
            json!({
                "jcode": {
                    "profile": profile.as_str(),
                    "extensions": ["raw_server_event"]
                }
            }),
        );
    }

    json!({
        "protocolVersion": protocol_version,
        "agentCapabilities": agent_capabilities,
        "agentInfo": {
            "name": "jcode",
            "title": "Jcode",
            "version": jcode_build_meta::pkg_version(),
        },
        "authMethods": [],
    })
}

fn cwd_from_params(params: &Value) -> std::result::Result<PathBuf, String> {
    let cwd = match params.get("cwd").and_then(Value::as_str) {
        Some(cwd) if !cwd.trim().is_empty() => PathBuf::from(cwd),
        _ => std::env::current_dir().map_err(|err| err.to_string())?,
    };
    if !cwd.is_absolute() {
        return Err(format!("ACP cwd must be absolute: {}", cwd.display()));
    }
    Ok(cwd)
}

fn required_session_id(params: &Value) -> std::result::Result<String, String> {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Missing required sessionId".to_string())
}

/// Build the JSON-RPC request `send_client_request` writes to the host.
/// Pure and separate from the actual write so it's testable without stdout.
#[allow(
    dead_code,
    reason = "only called from send_client_request, itself not yet called outside tests -- see that function's own allow"
)]
fn build_client_request(id: u64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// If `message` is response-shaped (see [`JsonRpcMessage::is_response`])
/// and its `id` is a plain non-negative integer (the only kind
/// [`build_client_request`] ever sends), return the id and the
/// `Ok(result)`/`Err(error)` payload to route to a pending waiter.
/// Anything else (a real request/notification, or a response whose id
/// this adapter couldn't have generated) returns `None` -- not an error,
/// since an unrecognized response id is the host's problem, not this
/// adapter's, and is deliberately just ignored by the caller rather than
/// breaking the connection over it.
/// Takes `message` by value rather than borrowing (Gemini review,
/// 2026-08-30): `result`/`error` used to be cloned out of a `&JsonRpcMessage`
/// -- harmless for a small ack, but `fs/read_text_file`'s own result is
/// exactly the kind of payload (a whole file's contents) where that clone
/// stops being free. `handle_message` already owns `message` outright and
/// has nothing left to do with it after this call, so moving the fields out
/// is free.
fn route_response(message: JsonRpcMessage) -> Option<(u64, std::result::Result<Value, Value>)> {
    if !message.is_response() {
        return None;
    }
    let id = message.id?.as_u64()?;
    match (message.result, message.error) {
        (Some(result), _) => Some((id, Ok(result))),
        (None, Some(error)) => Some((id, Err(error))),
        (None, None) => None,
    }
}

fn validate_acp_mcp_servers(params: &Value) -> std::result::Result<(), String> {
    match params.get("mcpServers") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Array(_)) => Ok(()),
        Some(_) => Err("ACP mcpServers must be an array".to_string()),
    }
}

/// One entry of ACP's `session/new` `mcpServers` array, ready to connect.
/// Assumed shape: `{name, command, args?, env?}` with `env` as a plain
/// string->string object -- the same convention jcode's own `.mcp.json`/
/// `McpServerConfig` and the `mcp` tool's own ad-hoc `connect` action
/// already use. **Not yet verified against a real ACP host** (no live
/// client has exercised this path) -- if a real host sends `env` as an
/// array of `{name, value}` pairs instead, this will silently see an empty
/// env for that server rather than fail the session; worth revisiting once
/// a real host is tested against.
struct AcpMcpServerSpec {
    name: String,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
}

/// Parse `mcpServers` into connectable specs, tolerating individual bad
/// entries rather than failing the whole `session/new` call -- matches
/// `validate_acp_mcp_servers`'s own "don't reject the session over this"
/// stance. An entry missing `name` or `command` (the two fields jcode's
/// `mcp connect` action actually requires) is skipped, not an error.
fn parse_acp_mcp_servers(params: &Value) -> Vec<AcpMcpServerSpec> {
    let Some(Value::Array(entries)) = params.get("mcpServers") else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.trim();
            let command = entry.get("command")?.as_str()?.trim();
            if name.is_empty() || command.is_empty() {
                return None;
            }
            let args = entry
                .get("args")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let env = entry
                .get("env")
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(AcpMcpServerSpec {
                name: name.to_string(),
                command: command.to_string(),
                args,
                env,
            })
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum AcpSlashCommand {
    Model(Option<String>),
    Models,
    Effort(Option<String>),
}

fn parse_acp_slash_command(text: &str) -> Option<Result<AcpSlashCommand>> {
    // A leading space is the ACP client convention for escaping slash command
    // interpretation and sending the text to the model literally.
    let trimmed = text.trim_end();
    let body = trimmed.strip_prefix('/')?;
    let mut parts = body.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let argument = parts
        .next()
        .map(str::trim)
        .filter(|argument| !argument.is_empty())
        .map(str::to_string);
    match name {
        "model" => Some(Ok(AcpSlashCommand::Model(argument))),
        "models" if argument.is_none() => Some(Ok(AcpSlashCommand::Models)),
        "models" => Some(Err(anyhow::anyhow!("/models does not accept an argument"))),
        "effort" => Some(Ok(AcpSlashCommand::Effort(argument))),
        _ => None,
    }
}

fn format_model_catalog(current: Option<&str>, models: &[String]) -> String {
    if models.is_empty() {
        return match current {
            Some(current) => format!("Current model: `{current}`. No model catalog was reported."),
            None => "The active provider did not report a model catalog.".to_string(),
        };
    }
    let mut output = String::from("Available models:\n");
    for model in models {
        let selected = if Some(model.as_str()) == current {
            " (current)"
        } else {
            ""
        };
        output.push_str(&format!("- `{model}`{selected}\n"));
    }
    output.pop();
    output
}

fn prompt_from_params(
    params: &Value,
) -> std::result::Result<(String, Vec<(String, String)>), String> {
    let prompt = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| "Missing required prompt array".to_string())?;
    let mut text_parts = Vec::new();
    let mut images = Vec::new();

    for block in prompt {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(text.to_string());
                }
            }
            Some("image") => {
                let mime_type = block
                    .get("mimeType")
                    .or_else(|| block.get("mime_type"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Image content block missing mimeType".to_string())?;
                let data = block
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Image content block missing data".to_string())?;
                images.push((mime_type.to_string(), data.to_string()));
            }
            Some("resource") => {
                if let Some(resource) = block.get("resource") {
                    text_parts.push(format_resource_block(resource));
                }
            }
            Some("resource_link") => {
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or("");
                let name = block.get("name").and_then(Value::as_str).unwrap_or(uri);
                text_parts.push(format!("[Resource link: {name} <{uri}>]"));
            }
            Some(other) => {
                return Err(format!(
                    "Unsupported ACP prompt content block type: {other}"
                ));
            }
            None => return Err("Prompt content block missing type".to_string()),
        }
    }

    Ok((text_parts.join("\n\n"), images))
}

fn format_resource_block(resource: &Value) -> String {
    let uri = resource
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or("resource");
    if let Some(text) = resource.get("text").and_then(Value::as_str) {
        format!("[Embedded resource: {uri}]\n{text}")
    } else if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
        let mime = resource
            .get("mimeType")
            .or_else(|| resource.get("mime_type"))
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream");
        format!(
            "[Embedded binary resource: {uri} ({mime}, {} base64 bytes)]",
            blob.len()
        )
    } else {
        format!("[Embedded resource: {uri}]")
    }
}

fn agent_message_chunk(text: String) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {
            "type": "text",
            "text": text,
        }
    })
}

fn tool_title(name: &str) -> String {
    match name {
        "bash" => "Running shell command".to_string(),
        "read" => "Reading file".to_string(),
        "write" => "Writing file".to_string(),
        "edit" | "multiedit" | "patch" | "apply_patch" => "Editing files".to_string(),
        "agentgrep" | "grep" | "glob" | "ls" => "Searching workspace".to_string(),
        "webfetch" | "websearch" => "Fetching web content".to_string(),
        other => other.replace('_', " "),
    }
}

pub(crate) fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" => "read",
        "write" | "edit" | "multiedit" | "patch" | "apply_patch" => "edit",
        "bash" | "bg" | "selfdev" => "execute",
        "agentgrep" | "grep" | "glob" | "ls" | "session_search" | "conversation_search" => "search",
        "webfetch" | "websearch" | "codesearch" => "fetch",
        _ => "other",
    }
}

pub(crate) async fn run_acp_command(
    provider_choice: ProviderChoice,
    model: Option<String>,
    provider_profile: Option<String>,
    explicit_tool_profile: bool,
) -> Result<()> {
    crate::env::set_var("JCODE_NON_INTERACTIVE", "1");
    let acp_config = crate::config::config().acp.clone();
    if !explicit_tool_profile {
        crate::env::set_var("JCODE_TOOL_PROFILE", acp_config.tool_profile.trim());
        crate::config::invalidate_config_cache();
    }
    let profile = AcpProfile::parse(&acp_config.profile);
    AcpRuntime::new(profile, provider_choice, model, provider_profile)
        .run()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn acp_tool_kind_maps_core_tools() {
        assert_eq!(tool_kind("read"), "read");
        assert_eq!(tool_kind("apply_patch"), "edit");
        assert_eq!(tool_kind("bash"), "execute");
        assert_eq!(tool_kind("agentgrep"), "search");
        assert_eq!(tool_kind("webfetch"), "fetch");
        assert_eq!(tool_kind("swarm"), "other");
    }

    #[test]
    fn json_rpc_parse_errors_use_standard_codes() {
        let (code, _) = JsonRpcMessage::parse("not json").unwrap_err();
        assert_eq!(code, JSONRPC_PARSE_ERROR);

        let (code, message) = JsonRpcMessage::parse(r#"{"method":"initialize"}"#).unwrap_err();
        assert_eq!(code, JSONRPC_INVALID_REQUEST);
        assert!(message.contains("jsonrpc"));
    }

    #[test]
    fn prompt_from_params_accepts_text_images_and_resources() {
        let params = json!({
            "sessionId": "s1",
            "prompt": [
                {"type": "text", "text": "hello"},
                {"type": "image", "mimeType": "image/png", "data": "abc"},
                {"type": "resource", "resource": {"uri": "file:///tmp/a.rs", "text": "fn main(){}"}},
                {"type": "resource_link", "uri": "file:///tmp/b.rs", "name": "b.rs"}
            ]
        });
        let (text, images) = prompt_from_params(&params).unwrap();
        assert!(text.contains("hello"));
        assert!(text.contains("Embedded resource: file:///tmp/a.rs"));
        assert!(text.contains("Resource link: b.rs"));
        assert_eq!(images, vec![("image/png".to_string(), "abc".to_string())]);
    }

    #[test]
    fn prompt_response_reports_usage_accumulated_across_the_turn() {
        let mut usage = TurnUsage::default();
        usage.add(10, 2, Some(4), Some(5));
        usage.add(20, 3, Some(6), Some(7));

        assert_eq!(
            prompt_response("end_turn", &usage),
            json!({
                "stopReason": "end_turn",
                "usage": {
                    "totalTokens": 57,
                    "inputTokens": 30,
                    "outputTokens": 5,
                    "cachedReadTokens": 10,
                    "cachedWriteTokens": 12,
                }
            })
        );
    }

    #[test]
    fn prompt_response_omits_unreported_usage_and_cache_fields() {
        assert_eq!(
            prompt_response("end_turn", &TurnUsage::default()),
            json!({ "stopReason": "end_turn" })
        );

        let mut usage = TurnUsage::default();
        usage.add(10, 2, None, None);
        assert_eq!(
            prompt_response("cancelled", &usage),
            json!({
                "stopReason": "cancelled",
                "usage": {
                    "totalTokens": 12,
                    "inputTokens": 10,
                    "outputTokens": 2,
                }
            })
        );
    }

    #[test]
    fn initialize_standard_omits_jcode_meta() {
        let result = initialize_result(&json!({"protocolVersion": 1}), AcpProfile::Standard);
        assert_eq!(result["protocolVersion"], 1);
        assert!(result["agentCapabilities"].get("_meta").is_none());
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
    }

    #[test]
    fn initialize_full_advertises_jcode_extension_meta() {
        let result = initialize_result(&json!({"protocolVersion": 1}), AcpProfile::Full);
        assert_eq!(
            result["agentCapabilities"]["_meta"]["jcode"]["profile"],
            "full"
        );
    }

    #[test]
    fn event_mapper_maps_tool_lifecycle() {
        let mut mapper = EventMapper::new("session1".to_string(), AcpProfile::Standard);
        let start = mapper.map_event(ServerEvent::ToolStart {
            id: "tool1".to_string(),
            name: "bash".to_string(),
        });
        assert_eq!(start[0]["sessionUpdate"], "tool_call");
        assert_eq!(start[0]["kind"], "execute");

        let input = mapper.map_event(ServerEvent::ToolInput {
            delta: "{\"command\":\"true\"}".to_string(),
        });
        assert_eq!(input[0]["rawInput"]["command"], "true");

        let done = mapper.map_event(ServerEvent::ToolDone {
            id: "tool1".to_string(),
            name: "bash".to_string(),
            output: "ok".to_string(),
            error: None,
        });
        assert_eq!(done[0]["status"], "completed");
        assert_eq!(done[0]["content"][0]["content"]["text"], "ok");
    }

    #[test]
    fn validate_acp_mcp_servers_accepts_any_array_shape() {
        // validate_acp_mcp_servers only checks "is this an array at all" --
        // per-entry validity is parse_acp_mcp_servers's job (below), which
        // tolerates bad entries by skipping them rather than by accepting
        // anything here. A malformed entry (missing "command") is still a
        // valid *array element* as far as this function is concerned.
        let params = json!({"mcpServers": [{"name": "fs"}]});
        assert!(validate_acp_mcp_servers(&params).is_ok());

        let params = json!({"mcpServers": []});
        assert!(validate_acp_mcp_servers(&params).is_ok());
    }

    #[test]
    fn validate_acp_mcp_servers_rejects_a_non_array() {
        let params = json!({"mcpServers": "fs"});
        assert!(validate_acp_mcp_servers(&params).is_err());
    }

    #[test]
    fn parse_acp_mcp_servers_extracts_valid_entries() {
        let params = json!({
            "mcpServers": [
                {
                    "name": "fs",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem"],
                    "env": {"DEBUG": "1"}
                },
                {"name": "bare", "command": "bare-server"}
            ]
        });
        let servers = parse_acp_mcp_servers(&params);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "fs");
        assert_eq!(servers[0].command, "npx");
        assert_eq!(
            servers[0].args,
            vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string()
            ]
        );
        assert_eq!(servers[0].env.get("DEBUG"), Some(&"1".to_string()));
        // No args/env at all is fine -- both default to empty, not an error.
        assert_eq!(servers[1].name, "bare");
        assert!(servers[1].args.is_empty());
        assert!(servers[1].env.is_empty());
    }

    #[test]
    fn parse_acp_mcp_servers_skips_entries_missing_required_fields() {
        let params = json!({
            "mcpServers": [
                {"name": "no-command"},
                {"command": "no-name"},
                {"name": "", "command": "blank-name"},
                {"name": "blank-command", "command": "  "},
                {"name": "good", "command": "good-server"}
            ]
        });
        // Only the one fully-valid entry survives -- the four malformed
        // ones are silently skipped, not treated as an error (matches
        // validate_acp_mcp_servers's own "don't reject the session" stance).
        let servers = parse_acp_mcp_servers(&params);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "good");
    }

    #[test]
    fn parse_acp_mcp_servers_returns_empty_for_absent_or_non_array_field() {
        assert!(parse_acp_mcp_servers(&json!({})).is_empty());
        assert!(parse_acp_mcp_servers(&json!({"mcpServers": null})).is_empty());
        assert!(parse_acp_mcp_servers(&json!({"mcpServers": "not-an-array"})).is_empty());
    }

    #[test]
    fn advertised_commands_cover_all_acp_daemon_model_controls() {
        let commands = acp_available_commands();
        let names: Vec<&str> = commands
            .iter()
            .map(|command| command["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["model", "models", "effort"]);
        assert_eq!(commands[0]["input"]["hint"], "model id (optional)");
        assert!(commands[1].get("input").is_none());
        assert!(
            commands[2]["input"]["hint"]
                .as_str()
                .unwrap()
                .contains("high")
        );
    }

    #[test]
    fn advertised_commands_parse_to_real_dispatch_variants() {
        assert_eq!(
            parse_acp_slash_command("/model claude-sonnet-4-5")
                .unwrap()
                .unwrap(),
            AcpSlashCommand::Model(Some("claude-sonnet-4-5".to_string()))
        );
        assert_eq!(
            parse_acp_slash_command("/model ").unwrap().unwrap(),
            AcpSlashCommand::Model(None)
        );
        assert_eq!(
            parse_acp_slash_command("/models").unwrap().unwrap(),
            AcpSlashCommand::Models
        );
        assert_eq!(
            parse_acp_slash_command("/effort xhigh").unwrap().unwrap(),
            AcpSlashCommand::Effort(Some("xhigh".to_string()))
        );
        assert!(parse_acp_slash_command("/models now").unwrap().is_err());
        assert!(parse_acp_slash_command("/not-advertised").is_none());
        assert!(parse_acp_slash_command(" /model literal").is_none());
        assert!(parse_acp_slash_command("ordinary prompt").is_none());
    }

    #[test]
    fn compatibility_methods_accept_host_field_names_and_aliases() {
        assert_eq!(
            compatibility_option_value(
                &json!({"modelId": "deepseek-v4-flash"}),
                &["modelId", "model"],
                "session/set_model"
            )
            .unwrap(),
            "deepseek-v4-flash"
        );
        assert_eq!(
            compatibility_option_value(
                &json!({"reasoningEffort": "high"}),
                &["effort", "reasoningEffort"],
                "session/set_reasoning_effort"
            )
            .unwrap(),
            "high"
        );
        assert!(
            compatibility_option_value(
                &json!({"effort": ""}),
                &["effort", "reasoningEffort"],
                "session/set_reasoning_effort"
            )
            .unwrap_err()
            .contains("non-empty")
        );
    }

    #[test]
    fn cwd_must_be_absolute() {
        let params = json!({"cwd": "relative"});
        assert!(cwd_from_params(&params).is_err());
        let params = json!({"cwd": "/tmp"});
        assert_eq!(cwd_from_params(&params).unwrap(), Path::new("/tmp"));
    }

    #[test]
    fn config_options_include_model_selector_and_effort_ladder() {
        let state = SessionUiState {
            provider_name: Some("openai".to_string()),
            model: Some("gpt-5.2".to_string()),
            available_models: vec!["gpt-5.2".to_string(), "gpt-5.2-codex".to_string()],
            reasoning_effort: Some("high".to_string()),
        };
        let options = session_config_options(&state);
        assert_eq!(options.len(), 2);

        let model = &options[0];
        assert_eq!(model["id"], CONFIG_ID_MODEL);
        assert_eq!(model["category"], "model");
        assert_eq!(model["type"], "select");
        assert_eq!(model["currentValue"], "gpt-5.2");
        assert_eq!(model["options"].as_array().unwrap().len(), 2);

        let effort = &options[1];
        assert_eq!(effort["id"], CONFIG_ID_EFFORT);
        assert_eq!(effort["category"], "thought_level");
        assert_eq!(effort["currentValue"], "high");
        let effort_values: Vec<&str> = effort["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["value"].as_str().unwrap())
            .collect();
        assert!(effort_values.contains(&"medium"));
        assert!(
            !effort_values.iter().any(|value| value.starts_with("swarm")),
            "swarm sentinels are TUI-only and must not leak over ACP: {effort_values:?}"
        );
    }

    #[test]
    fn config_options_current_model_prepended_when_not_listed() {
        let state = SessionUiState {
            provider_name: Some("anthropic".to_string()),
            model: Some("claude-opus-4-6".to_string()),
            available_models: vec!["claude-sonnet-4-5".to_string()],
            reasoning_effort: None,
        };
        let options = session_config_options(&state);
        let model_values: Vec<&str> = options[0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["value"].as_str().unwrap())
            .collect();
        assert_eq!(model_values[0], "claude-opus-4-6");
        assert!(model_values.contains(&"claude-sonnet-4-5"));
    }

    #[test]
    fn legacy_models_catalog_is_emitted_alongside_config_options() {
        let state = SessionUiState {
            provider_name: Some("deepseek".to_string()),
            model: Some("deepseek-v4-flash".to_string()),
            available_models: vec!["deepseek-v4-pro".to_string()],
            reasoning_effort: Some("high".to_string()),
        };
        let mut result = json!({"sessionId": "s1"});
        insert_session_configuration(&mut result, &state);

        assert!(result["configOptions"].is_array());
        assert_eq!(result["models"]["currentModelId"], "deepseek-v4-flash");
        let ids: Vec<&str> = result["models"]["availableModels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["modelId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["deepseek-v4-flash", "deepseek-v4-pro"]);
    }

    #[test]
    fn config_options_empty_without_model_state() {
        let options = session_config_options(&SessionUiState::default());
        assert!(options.is_empty());
    }

    #[test]
    fn context_limit_falls_back_to_default_for_unknown_models() {
        let state = SessionUiState {
            provider_name: Some("mystery".to_string()),
            model: Some("mystery-model-9000".to_string()),
            available_models: Vec::new(),
            reasoning_effort: None,
        };
        assert_eq!(
            state.context_limit(),
            crate::provider::DEFAULT_CONTEXT_LIMIT as u64
        );
    }

    // --- Phase 3: ACP client-callback plumbing (send_client_request /
    // route_response) -- the mechanism only, no real caller wired up yet. ---

    fn test_runtime() -> AcpRuntime {
        AcpRuntime::new(AcpProfile::Standard, ProviderChoice::Jcode, None, None)
    }

    #[test]
    fn build_client_request_shapes_a_valid_jsonrpc_request() {
        let request = build_client_request(7, "fs/read_text_file", json!({"path": "/tmp/x"}));
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 7);
        assert_eq!(request["method"], "fs/read_text_file");
        assert_eq!(request["params"]["path"], "/tmp/x");
    }

    #[test]
    fn route_response_extracts_a_result() {
        let message =
            JsonRpcMessage::parse(r#"{"jsonrpc":"2.0","id":3,"result":{"content":"hi"}}"#)
                .expect("parse");
        let (id, payload) = route_response(message).expect("should route");
        assert_eq!(id, 3);
        assert_eq!(payload, Ok(json!({"content": "hi"})));
    }

    #[test]
    fn route_response_extracts_an_error() {
        let message =
            JsonRpcMessage::parse(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-1,"message":"nope"}}"#)
                .expect("parse");
        let (id, payload) = route_response(message).expect("should route");
        assert_eq!(id, 3);
        assert_eq!(payload, Err(json!({"code": -1, "message": "nope"})));
    }

    #[test]
    fn route_response_ignores_a_real_request() {
        let message = JsonRpcMessage::parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .expect("parse");
        assert!(route_response(message).is_none());
    }

    #[test]
    fn route_response_ignores_a_response_with_a_non_integer_id() {
        // This adapter only ever generates plain u64 ids for its own
        // outbound requests (build_client_request) -- a response whose id
        // is a string or float couldn't be one of ours.
        let message =
            JsonRpcMessage::parse(r#"{"jsonrpc":"2.0","id":"acp-host-id","result":{}}"#)
                .expect("parse");
        assert!(route_response(message).is_none());
    }

    #[tokio::test]
    async fn handle_message_routes_a_response_to_the_pending_waiter() {
        let runtime = test_runtime();
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.pending_client_requests.lock().await.insert(42, tx);

        let response =
            JsonRpcMessage::parse(r#"{"jsonrpc":"2.0","id":42,"result":{"ok":true}}"#)
                .expect("parse");
        runtime.handle_message(response).await.expect("handle_message");

        let payload = rx.await.expect("waiter should have been completed");
        assert_eq!(payload, Ok(json!({"ok": true})));
        assert!(
            !runtime.pending_client_requests.lock().await.contains_key(&42),
            "the pending entry must be removed once routed"
        );
    }

    #[tokio::test]
    async fn handle_message_silently_drops_a_response_with_no_matching_waiter() {
        let runtime = test_runtime();
        // Nothing registered for id 99 -- e.g. it already timed out, or
        // this is a stray response. Must not error the connection.
        let response = JsonRpcMessage::parse(r#"{"jsonrpc":"2.0","id":99,"result":{}}"#)
            .expect("parse");
        runtime
            .handle_message(response)
            .await
            .expect("must not error on an unmatched response");
    }

    #[tokio::test]
    async fn handle_message_does_not_treat_a_non_integer_id_response_as_a_bad_request() {
        // Regression for a real bug (Gemini review, 2026-08-30): this is
        // response-shaped (no method, has result) but its id isn't one
        // this adapter could have generated (route_response returns None
        // for it) -- previously fell through to sending back "JSON-RPC
        // request missing method", a spurious reply for something that was
        // never a request. `handle_message.is_response()` is now checked
        // as its own branch *before* that fallback, so this path is now
        // structurally unreachable for anything response-shaped, matched
        // or not. (Asserting *what* got written to stdout isn't checkable
        // here -- `AcpRuntime.stdout` is a real `tokio::io::Stdout`, not an
        // injectable sink -- so this only proves the call still succeeds
        // cleanly with no panic/error propagated, same as the matched
        // case above; the "no error line sent" property is verified by
        // code inspection of the `if message.is_response() { ...; return
        // Ok(()); }` early-return, not by this test alone.)
        let runtime = test_runtime();
        let response =
            JsonRpcMessage::parse(r#"{"jsonrpc":"2.0","id":"acp-host-id","result":{}}"#)
                .expect("parse");
        runtime
            .handle_message(response)
            .await
            .expect("must not error on a response with an unrecognized id shape");
    }

    #[tokio::test]
    async fn send_client_request_times_out_cleanly_when_the_host_never_responds() {
        let runtime = test_runtime();
        let result = runtime
            .send_client_request(
                "fs/read_text_file",
                json!({"path": "/tmp/x"}),
                std::time::Duration::from_millis(50),
            )
            .await;
        assert!(result.is_err(), "must time out, not hang forever");
        assert!(
            runtime.pending_client_requests.lock().await.is_empty(),
            "a timed-out request must clean up its own pending-map entry"
        );
    }

    #[tokio::test]
    async fn send_client_request_resolves_when_the_matching_response_arrives() {
        let runtime = test_runtime();
        let runtime_for_send = runtime.clone();
        let send_task = tokio::spawn(async move {
            runtime_for_send
                .send_client_request(
                    "fs/read_text_file",
                    json!({"path": "/tmp/x"}),
                    std::time::Duration::from_secs(5),
                )
                .await
        });

        // Poll until send_client_request has registered its waiter --
        // deterministic (bounded retries on a real condition), not a fixed
        // sleep guessing how long registration takes.
        let id = loop {
            let pending = runtime.pending_client_requests.lock().await;
            if let Some((&id, _)) = pending.iter().next() {
                break id;
            }
            drop(pending);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };

        let response = JsonRpcMessage::parse(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":"file contents"}}}}"#
        ))
        .expect("parse");
        runtime.handle_message(response).await.expect("handle_message");

        let result = send_task.await.expect("task").expect("should resolve");
        assert_eq!(result, json!({"content": "file contents"}));
    }
}
