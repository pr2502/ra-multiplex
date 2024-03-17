use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::ops::Deref;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::error::SendError;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::{select, task};
use tracing::{debug, error, field, info, instrument, trace, warn, Instrument};

use crate::client::Client;
use crate::config::Config;
use crate::lsp::ext::Tag;
use crate::lsp::jsonrpc::{Message, Notification, Request, RequestId, ResponseSuccess, Version};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::{self, ext};

/// Specifies server configuration
///
/// If another server with the same configuration is requested we can reuse it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InstanceKey {
    pub server: String,
    pub args: Vec<String>,
    pub workspace_root: String,
}

/// Language server instance
pub struct Instance {
    key: InstanceKey,

    /// Language server child process id
    pid: u32,

    /// Server's response to `initialize` request
    init_result: lsp::InitializeResult,

    /// Handle for sending messages to the language server instance
    server: mpsc::Sender<Message>,

    /// Data of associated clients
    clients: Mutex<HashMap<u16, ClientData>>,

    /// Dynamic capabilities registered by the server
    dynamic_capabilities: Mutex<HashMap<String, lsp::Registration>>,

    /// Wakes up `wait_task` and asks it to send SIGKILL to the instance.
    close: Notify,

    /// Last time a message was sent to this instance
    ///
    /// Uses UTC unix timestamp ([utc_now] function)
    last_used: AtomicI64,
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Make sure we're not leaking anything
        debug!("instance dropped");
    }
}

/// Wrapper around client handle with additional data only the server instance
/// knows about
struct ClientData {
    /// Handle for sending messages to clients
    client: Client,

    /// URIs of files currently opened by this client
    files: HashSet<String>,
}

impl ClientData {
    fn get_status(&self) -> ext::Client {
        ext::Client {
            port: self.client.port(),
            files: self.files.iter().cloned().collect(),
        }
    }
}

impl Deref for ClientData {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

// Current unix timestamp with second precission
fn utc_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl Instance {
    /// Mark the instance as used
    pub fn keep_alive(&self) {
        self.last_used.store(utc_now(), Ordering::Relaxed);
    }

    /// How many seconds is the instance idle for
    pub fn idle(&self) -> i64 {
        i64::max(0, utc_now() - self.last_used.load(Ordering::Relaxed))
    }

    pub fn initialize_result(&self) -> lsp::InitializeResult {
        self.init_result.clone()
    }

    /// Add client to the instance so it can receive traffic from it
    ///
    /// It replays all registered dynamic capabilities to it.
    pub async fn add_client(&self, client: Client) {
        let mut clients = self.clients.lock().await;
        let dyn_capabilities = self.dynamic_capabilities.lock().await;

        if !dyn_capabilities.is_empty() {
            // Register all currently cached dynamic capabilities if there are
            // any. We will drop the client response and we need to make sure
            // the request ID is unique.
            let id = RequestId::String("replay:registerCapabilities".into()).tag(Tag::Drop);
            let params = lsp::RegistrationParams {
                registrations: dyn_capabilities.values().cloned().collect(),
            };
            let req = Request {
                id,
                method: "client/registerCapability".into(),
                params: serde_json::to_value(params).unwrap(),
                jsonrpc: Version,
            };
            debug!(?req, "replaying server request");
            let _ = client.send_message(req.into()).await;
        }

        let client = ClientData {
            client,
            files: HashSet::new(),
        };
        if clients.insert(client.port(), client).is_some() {
            unreachable!("BUG: added two clients with the same port");
        }
    }

    /// Send cleanup messages and remove remove client for client map
    pub async fn cleanup_client(&self, client: Client) -> Result<()> {
        debug!("cleaning up client");

        let mut clients = self.clients.lock().await;

        let Some(client) = clients.remove(&client.port()) else {
            // TODO This happens for example when the language server died while
            // client was still connected, and the client cleanup is attempted
            // with the instance being gone already. We should try notifying
            // these clients immediately and handling the cleanup separately.
            bail!("client was not connected");
        };

        let files = client.files.into_iter().collect::<Vec<_>>();
        self.close_all_files(&clients, files)
            .await
            .context("error closing files")?;

        Ok(())
    }

    /// Send a message to the language server channel
    pub async fn send_message(&self, message: Message) -> Result<(), SendError<Message>> {
        self.server.send(message).await
    }

    /// Save registered capabilities to allow later replaying them to new clients
    async fn register_capabilities(&self, params: Value) -> Result<()> {
        let params =
            serde_json::from_value::<lsp::RegistrationParams>(params).context("parsing params")?;

        let mut dyn_capabilities = self.dynamic_capabilities.lock().await;
        for reg in params.registrations {
            dyn_capabilities.insert(reg.id.clone(), reg);
        }

        Ok(())
    }

    /// Remove cached capability registration to stop replaying them to new clients
    async fn unregister_capabilities(&self, params: Value) -> Result<()> {
        let params = serde_json::from_value::<lsp::UnregistrationParams>(params)
            .context("parsing params")?;

        let mut dyn_capabilities = self.dynamic_capabilities.lock().await;
        for unreg in params.unregistrations {
            dyn_capabilities.remove(&unreg.id);
        }

        Ok(())
    }

    /// Handle `textDocument/didOpen` client notification
    pub async fn open_file(&self, port: u16, params: Value) -> Result<()> {
        let params = serde_json::from_value::<lsp::DidOpenTextDocumentParams>(params)
            .context("parsing params")?;
        let uri = &params.text_document.uri;

        let mut send_notification = true;

        let mut clients = self.clients.lock().await;
        for client in clients.values() {
            if client.files.contains(uri) {
                debug!(?uri, "file is already opened by another client");
                send_notification = false;
                break;
            }
        }

        clients
            .get_mut(&port)
            .expect("no matching client")
            .files
            .insert(uri.clone());

        if send_notification {
            let notif = Notification {
                jsonrpc: Version,
                method: "textDocument/didOpen".into(),
                params: serde_json::to_value(params).unwrap(),
            };
            debug!(?notif, "first client opened file");
            let _ = self.send_message(notif.into()).await;
        }

        Ok(())
    }

    /// Handle `textDocument/didClose` client notification
    pub async fn close_file(&self, port: u16, params: Value) -> Result<()> {
        let params = serde_json::from_value::<lsp::DidCloseTextDocumentParams>(params)
            .context("parsing params")?;

        let mut clients = self.clients.lock().await;

        clients
            .get_mut(&port)
            .context("no matching client")?
            .files
            .remove(&params.text_document.uri);

        self.close_all_files(&clients, vec![params.text_document.uri])
            .await
    }

    /// Handle closing many files at once and sending notifications for
    /// definitely closed files
    async fn close_all_files(
        &self,
        clients: &HashMap<u16, ClientData>,
        files: Vec<String>,
    ) -> Result<()> {
        for uri in files {
            let mut send_notification = true;

            for client in clients.values() {
                if client.files.contains(&uri) {
                    debug!(?uri, "file still opened by another client");
                    send_notification = false;
                    break;
                }
            }

            if send_notification {
                let params = lsp::DidCloseTextDocumentParams {
                    text_document: lsp::TextDocumentIdentifier { uri },
                };
                let notif = Notification {
                    jsonrpc: Version,
                    method: "textDocument/didClose".into(),
                    params: serde_json::to_value(params).unwrap(),
                };
                debug!(?notif, "last client closed file");
                let _ = self.send_message(notif.into()).await;
            }
        }

        Ok(())
    }

    pub fn get_status(&self) -> ext::Instance {
        let clients = self
            .clients
            .blocking_lock()
            .values()
            .map(|client| client.get_status())
            .collect();

        let registered_dyn_capabilities = self
            .dynamic_capabilities
            .blocking_lock()
            .values()
            .map(|reg| reg.method.clone())
            .collect();

        ext::Instance {
            pid: self.pid,
            server: self.key.server.clone(),
            args: self.key.args.clone(),
            workspace_root: self.key.workspace_root.clone(),
            last_used: self.last_used.load(Ordering::Relaxed),
            clients,
            registered_dyn_capabilities,
        }
    }
}

pub struct InstanceMap(HashMap<InstanceKey, Arc<Instance>>);

impl InstanceMap {
    pub async fn new() -> Arc<Mutex<Self>> {
        let instance_map = Arc::new(Mutex::new(InstanceMap(HashMap::new())));
        let config = Config::load_or_default().await;
        task::spawn(gc_task(
            instance_map.clone(),
            config.gc_interval,
            config.instance_timeout,
        ));
        instance_map
    }

    /// Finds an instance with the longest path such as
    /// `cwd.starts_with(workspace_root)` is true
    pub fn get_by_cwd(&self, cwd: &str) -> Option<&Instance> {
        self.0
            .iter()
            .filter(|(key, _)| Path::new(cwd).starts_with(&key.workspace_root))
            .max_by_key(|(key, _)| key.workspace_root.len())
            .map(|(_, inst)| inst.deref())
    }

    pub fn get_status(&self) -> ext::StatusResponse {
        ext::StatusResponse {
            instances: self
                .0
                .values()
                .map(|instance| instance.get_status())
                .collect(),
        }
    }
}

/// Periodically check for for idle language server instances
#[instrument("garbage collector", skip_all)]
async fn gc_task(
    instance_map: Arc<Mutex<InstanceMap>>,
    gc_interval: u32,
    instance_timeout: Option<u32>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(gc_interval.into()));
    loop {
        interval.tick().await;

        for (key, instance) in &instance_map.lock().await.0 {
            let clients = instance.clients.lock().await;

            let idle = instance.idle();
            debug!(path = ?key.workspace_root, idle, clients = clients.len(), "check instance");

            if let Some(instance_timeout) = instance_timeout {
                // Close timed out instance
                if idle > i64::from(instance_timeout) && clients.is_empty() {
                    info!(pid = instance.pid, path = ?key.workspace_root, idle, "instance timed out");
                    instance.close.notify_one();
                }
            }
        }
    }
}

/// Find existing or spawn a new language server instance
///
/// The instance is looked up based on `instance_key`. If an existing one is
/// found then it's returned and `init_req_params` are discarded. If it's
/// not found a new instance is spawned and initialized using the provided
/// `init_req_params`, this insance is then inserted into the map and returned.
pub async fn get_or_spawn(
    map: Arc<Mutex<InstanceMap>>,
    key: InstanceKey,
    init_req_params: lsp::InitializeParams,
) -> Result<Arc<Instance>> {
    // We have locked the whole map so we can assume noone else tries to spawn
    // the same instance again.
    let map_clone = map.clone();
    let mut map_lock = map_clone.lock().await;

    if let Some(instance) = map_lock.0.get(&key) {
        info!("reusing language server instance");
        return Ok(instance.clone());
    }

    let instance = spawn(key.clone(), init_req_params, map)
        .await
        .context("spawning instance")?;
    map_lock.0.insert(key, instance.clone());

    Ok(instance)
}

#[instrument(name = "instance", fields(pid = field::Empty), skip_all, parent = None)]
async fn spawn(
    key: InstanceKey,
    init_req_params: lsp::InitializeParams,
    // Caller `get_or_spawn` is holding a lock to the map, we must not try to
    // lock it within this function to not cause deadlock, only spawned tasks
    // are allowed to lock it again.
    map: Arc<Mutex<InstanceMap>>,
) -> Result<Arc<Instance>> {
    let mut child = Command::new(&key.server)
        .args(&key.args)
        .current_dir(&key.workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "spawning langauge server: server={:?}, args={:?}, cwd={:?}",
                key.server, key.args, key.workspace_root,
            )
        })?;

    let pid = child.id().context("child exited early, couldn't get PID")?;
    tracing::Span::current().record("pid", pid);

    info!(server = ?key.server, args = ?key.args, cwd = ?key.workspace_root, "spawned langauge server");

    let stderr = child.stderr.take().unwrap();
    task::spawn(stderr_task(stderr).in_current_span());

    let stdout = child.stdout.take().unwrap();
    let mut reader = LspReader::new(BufReader::new(stdout), "server");

    let stdin = child.stdin.take().unwrap();
    let mut writer = LspWriter::new(stdin, "server");

    let init_result = initialize_handshake(init_req_params, &mut reader, &mut writer)
        .await
        .context("server handshake")?;

    info!("initialized server");

    let (message_writer, rx) = mpsc::channel(64);

    let instance = Arc::new(Instance {
        key,
        pid,
        init_result,
        server: message_writer,
        clients: Mutex::default(),
        dynamic_capabilities: Mutex::default(),
        close: Notify::new(),
        last_used: AtomicI64::new(utc_now()),
    });

    task::spawn(stdout_task(instance.clone(), reader).in_current_span());
    task::spawn(stdin_task(rx, writer).in_current_span());

    task::spawn(wait_task(instance.clone(), map, child).in_current_span());

    Ok(instance)
}

#[instrument(skip_all)]
async fn initialize_handshake(
    init_req_params: lsp::InitializeParams,
    reader: &mut LspReader<BufReader<ChildStdout>>,
    writer: &mut LspWriter<ChildStdin>,
) -> Result<lsp::InitializeResult> {
    let request_id = "lspmux:initialize_request";

    // Use the first client's `InitializeParams` to initialize server. We assume
    // all subsequent clients configuration will be somewhat compatible with
    // whatever the first client negotiated for the same `workspace_root`,
    // `server` and `args`.
    let req = Request {
        jsonrpc: Version,
        method: "initialize".into(),
        params: serde_json::to_value(init_req_params).unwrap(),
        id: RequestId::String(request_id.into()),
    };
    writer
        .write_message(&req.into())
        .await
        .context("send initialize request")?;

    let res = match reader
        .read_message()
        .await
        .context("receive initialize response")?
        .context("stream ended")?
    {
        Message::ResponseSuccess(res) if res.id == request_id => res,
        _ => bail!("first server message was not initialize response"),
    };
    let result = serde_json::from_value(res.result).context("parse initialize response result")?;

    // Send a "fake" `initialized` notification to the server. We wait for them
    // from each client's `initialized` notification ourselves and they don't
    // contain any data that would need passing on.
    let init_notif = Notification {
        jsonrpc: Version,
        method: "initialized".into(),
        params: json!({}),
    };
    writer
        .write_message(&init_notif.into())
        .await
        .context("send initialized notification")?;

    Ok(result)
}

/// Read errors from langauge server stderr and log them
async fn stderr_task(stderr: ChildStderr) {
    let mut stderr = BufReader::new(stderr);
    let mut buffer = String::new();

    loop {
        buffer.clear();
        match stderr.read_line(&mut buffer).await {
            Ok(0) => {
                // reached EOF
                debug!("stderr closed");
                break;
            }
            Ok(_) => {
                let line = buffer.trim_end(); // remove trailing '\n' or possibly '\r\n'
                error!(%line, "stderr");
            }
            Err(err) => {
                let err = anyhow::Error::from(err);
                error!(?err, "error reading from stderr");
            }
        }
    }
}

/// Receive messages from clients' channel and write them into language server stdin
async fn stdin_task(mut receiver: mpsc::Receiver<Message>, mut writer: LspWriter<ChildStdin>) {
    // Because we (stdin task) don't keep a reference to `self` it will be dropped when the
    // child closes and all the clients disconnect including the sender and this receiver
    // will not keep blocking (unlike in client input task)
    while let Some(message) = receiver.recv().await {
        if let Err(err) = writer.write_message(&message).await {
            match err.kind() {
                // stdin is closed, no need to log an error
                ErrorKind::BrokenPipe => {}
                _ => {
                    let err = anyhow::Error::from(err);
                    error!(?err, "error writing to stdin");
                }
            }
            break;
        }
    }
    debug!("stdin closed");
}

/// Wait for child and log when it exits
async fn wait_task(
    instance: Arc<Instance>,
    instance_map: Arc<Mutex<InstanceMap>>,
    mut child: Child,
) {
    let key = instance.key.clone();
    loop {
        select! {
            _ = instance.close.notified() => {
                if let Err(err) = child.start_kill() {
                    error!(?err, "failed to close child");
                }
            }
            exit = child.wait() => {
                // Remove the closing instance from the map so new clients spawn their own instance
                instance_map.lock().await.0.remove(&key);

                // Disconnect all current clients
                //
                // We'll rely on the editor client to restart the ra-multiplex client,
                // start a new connection and we'll spawn another instance like we'd with
                // any other new client.
                instance.clients.lock().await.clear();

                match exit {
                    Ok(status) => {
                        #[cfg(unix)]
                        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
                        #[cfg(not(unix))]
                        let signal = tracing::field::Empty;

                        error!(
                            success = status.success(),
                            code = status.code(),
                            signal,
                            "child exited",
                        );
                    }
                    Err(err) => error!(?err, "error waiting for child"),
                }
                break;
            }
        }
    }
}

/// Read messages from server stdout and send them to corresponding client channels
async fn stdout_task(instance: Arc<Instance>, mut reader: LspReader<BufReader<ChildStdout>>) {
    loop {
        let message = match reader.read_message().await {
            Ok(Some(message)) => message,
            Ok(None) => {
                debug!("stdout closed");
                break;
            }
            Err(err) => {
                error!(?err, "reading message");
                continue;
            }
        };

        // Lock _after_ we have a message to send, then send and immediately release the lock
        let clients = instance.clients.lock().await;
        match message {
            Message::ResponseSuccess(mut res) => {
                // Forward successful response to the right client based on the
                // Request ID tag.
                if let (Some(Tag::Port(port)), id) = res.id.untag() {
                    res.id = id;
                    if let Some(client) = clients.get(&port) {
                        let _ = client.send_message(res.into()).await;
                    } else {
                        debug!(?port, "no matching client");
                    }
                } else {
                    warn!(?res, "ignoring improperly tagged server response")
                }
            }

            Message::ResponseError(mut res) => {
                // Forward the error response to the right client based on the
                // Request ID tag.
                if let (Some(Tag::Port(port)), id) = res.id.untag() {
                    warn!(?res, "server responded with error");
                    res.id = id;
                    if let Some(client) = clients.get(&port) {
                        let _ = client.send_message(res.into()).await;
                    } else {
                        debug!(?port, "no matching client");
                    }
                } else {
                    warn!(?res, "ignoring improperly tagged server response")
                }
            }

            Message::Request(mut req)
                if [
                    "window/workDoneProgress/create",
                    "workspace/codeLens/refresh",
                    "workspace/semanticTokens/refresh",
                    "workspace/inlayHint/refresh",
                    "workspace/inlineValue/refresh",
                    "workspace/diagnostic/refresh",
                ]
                .contains(&req.method.as_str()) =>
            {
                // All these server requests have null responses and we need
                // to inform all clients. We can forward the request to all
                // clients, send a fake successful response and ignore the real
                // client responses.
                trace!(?req, "server request {}", req.method.as_str());

                let id = req.id;
                req.id = id.tag(Tag::Drop);

                for client in clients.values() {
                    let _ = client.send_message(req.clone().into()).await;
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(id).into())
                    .await;
            }

            Message::Request(mut req) if req.method == "workspace/configuration" => {
                // Response to `workspace/configuration` should be the same from
                // any client. So we'll just pick the first and let it answer.
                debug!(?req, "server request workspace/configuration");

                req.id = req.id.tag(Tag::Forward);

                if let Some(client) = clients.values().next() {
                    let _ = client.send_message(req.into()).await;
                } else {
                    // If there is no client connected at this moment we'll
                    // ignore the request.
                }
            }

            Message::Request(mut req) if req.method == "client/registerCapability" => {
                // These need to be forwarded to every client so they're aware
                // of the capability. The response doesn't contain anything
                // important so we can safely ignore the real answers and send a
                // fake one to the server.
                debug!(?req, "server request client/registerCapability");

                let id = req.id;
                req.id = id.tag(Tag::Drop);

                for client in clients.values() {
                    let _ = client.send_message(req.clone().into()).await;
                }

                // We need to cache the dynamic capabilities registrations for
                // any client that might come later.
                if let Err(err) = instance.register_capabilities(req.params).await {
                    warn!(?err, "error registering capabilities");
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(id).into())
                    .await;
            }

            Message::Request(mut req) if req.method == "client/unregisterCapability" => {
                // These need to be forwarded to every client so they're aware
                // of the capability not being available anymore. The response
                // doesn't contain anything important so we can safely ignore
                // the real answers and send a fake one to the server.
                debug!(?req, "server request client/unregisterCapability");

                let id = req.id;
                req.id = id.tag(Tag::Drop);

                for client in clients.values() {
                    let _ = client.send_message(req.clone().into()).await;
                }

                // We need to remove this registration from the cache so we
                // don't announce it to new clients anymore.
                if let Err(err) = instance.unregister_capabilities(req.params).await {
                    warn!(?err, "error unregistering capabilities");
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(id).into())
                    .await;
            }

            Message::Request(req) => {
                // Unimplemented server -> client requests I've found in the LSP Spec.
                // TODO workspace/workspaceFolders request
                // TODO workspace/applyEdit request
                debug!(message = ?req, "ignoring unknown server request");
            }

            Message::Notification(notif) => {
                // Server notifications don't expect a response. We can forward
                // them to all clients.
                for client in clients.values() {
                    let _ = client.send_message(notif.clone().into()).await;
                }
            }
        }
    }
}
