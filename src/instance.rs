use std::collections::HashMap;
use std::io::ErrorKind;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::error::SendError;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::{select, task};
use tracing::{debug, error, field, info, instrument, warn, Instrument};

use crate::client::Client;
use crate::config::Config;
use crate::lsp::jsonrpc::{Message, Notification, Request, RequestId, Version};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::{lspmux, InitializeParams, InitializeResult};

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
    init_result: InitializeResult,

    /// Sending messages to the language server instance
    server: mpsc::Sender<Message>,

    /// Send messages to clients
    clients: Mutex<HashMap<u16, Client>>,

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

    pub fn initialize_result(&self) -> InitializeResult {
        self.init_result.clone()
    }

    pub async fn add_client(&self, client: Client) {
        if self
            .clients
            .lock()
            .await
            .insert(client.port(), client)
            .is_some()
        {
            unreachable!("BUG: added two clients with the same port");
        }
    }

    /// Send a message to the language server channel
    pub async fn send_message(&self, message: Message) -> Result<(), SendError<Message>> {
        self.server.send(message).await
    }

    pub fn get_status(&self) -> lspmux::Instance {
        lspmux::Instance {
            pid: self.pid,
            server: self.key.server.clone(),
            args: self.key.args.clone(),
            workspace_root: self.key.workspace_root.clone(),
            last_used: self.last_used.load(Ordering::Relaxed),
            clients: self
                .clients
                .blocking_lock()
                .values()
                .map(|client| client.get_status())
                .collect(),
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

    pub fn get_status(&self) -> lspmux::StatusResponse {
        lspmux::StatusResponse {
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
            let mut message_readers = instance.clients.lock().await;

            // Remove closed senders
            //
            // We have to check here because otherwise the senders only get
            // removed when a message is sent to them which might leave them
            // hanging forever if the language server is quiet and cause the
            // GC to never trigger.
            message_readers.retain(|_port, client| client.is_connected());

            let idle = instance.idle();
            debug!(path = ?key.workspace_root, idle, readers = message_readers.len(), "check instance");

            if let Some(instance_timeout) = instance_timeout {
                // Close timed out instance
                if idle > i64::from(instance_timeout) && message_readers.is_empty() {
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
    init_req_params: InitializeParams,
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
    init_req_params: InitializeParams,
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
    let mut writer = LspWriter::new(stdin, "-> server");

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
    init_req_params: InitializeParams,
    reader: &mut LspReader<BufReader<ChildStdout>>,
    writer: &mut LspWriter<ChildStdin>,
) -> Result<InitializeResult> {
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

fn parse_tagged_id(tagged: &RequestId) -> Option<(u16, RequestId)> {
    match (|| -> Result<_> {
        let RequestId::String(tagged) = tagged else {
            bail!("tagged id must be a String found `{tagged:?}`");
        };

        let (port, rest) = tagged.split_once(':').context("missing first `:`")?;
        let port = u16::from_str(port)?;
        let (value_type, old_id) = rest.split_once(':').context("missing second `:`")?;
        let old_id = match value_type {
            "n" => RequestId::Number(old_id.parse()?),
            "s" => RequestId::String(old_id.to_owned()),
            _ => bail!("invalid tag type `{value_type}`"),
        };
        Ok((port, old_id))
    })() {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            warn!(?err, "invalid tagged id");
            None
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
        let message_readers = instance.clients.lock().await;
        match message {
            Message::ResponseSuccess(mut res) => {
                if let Some((port, id)) = parse_tagged_id(&res.id) {
                    res.id = id;
                    if let Some(sender) = message_readers.get(&port) {
                        let _ignore = sender.send_message(res.into()).await;
                    } else {
                        debug!(?port, "no client");
                    }
                };
            }

            Message::ResponseError(mut res) => {
                if let Some((port, id)) = parse_tagged_id(&res.id) {
                    res.id = id;
                    if let Some(sender) = message_readers.get(&port) {
                        let _ignore = sender.send_message(res.into()).await;
                    } else {
                        debug!(?port, "no client");
                    }
                };
            }

            Message::Request(_) => {
                debug!(?message, "server request");
                // FIXME uncommenting this crashes rust-analyzer, presumably because the client
                // then sends a confusing response or something? i'm guessing that the response
                // id gets tagged with port but rust-analyzer expects to know the id because
                // it's actually a response and not a request. i'm not sure if we can handle
                // these at all with multiple clients attached
                //
                // ideally we could send these to all clients, but what if there is a matching
                // response from each client? rust-analyzer only expects one (this might
                // actually be why it's crashing)
                //
                // ignoring these might end up being the safest option, they don't seem to
                // matter to neovim anyway
                // ```rust
                // let message = Message::from_bytes(bytes);
                // let senders = senders.read().await;
                // for sender in senders.values() {
                //     sender
                //         .send(message.clone())
                //         .await
                //         .context("forward server notification")?;
                // }
                // ```
            }

            Message::Notification(notif) => {
                // Send notifications to all clients
                for sender in message_readers.values() {
                    let _ignore = sender.send_message(notif.clone().into()).await;
                }
            }
        }
    }
}
