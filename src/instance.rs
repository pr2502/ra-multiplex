use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::{self, FromStr};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::{select, task};
use tracing::{debug, error, info, instrument, trace, warn, Instrument};

use crate::config::Config;
use crate::lsp::jsonrpc::{Message, RequestId, ResponseSuccess};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::proto;
use crate::wait_cell::WaitCell;

/// keeps track of the initialize/initialized handshake for an instance
#[derive(Default)]
pub struct InitializeCache {
    request_sent: AtomicBool,
    notif_sent: AtomicBool,
    pub response: WaitCell<ResponseSuccess>,
}

impl InitializeCache {
    /// returns true if this is the first thread attempting to send a request
    pub fn attempt_send_request(&self) -> bool {
        let already_sent = self.request_sent.swap(true, Ordering::SeqCst);
        !already_sent
    }

    /// returns true if this is the first thread attempting to send an initialized notification
    pub fn attempt_send_notif(&self) -> bool {
        let already_sent = self.notif_sent.swap(true, Ordering::SeqCst);
        !already_sent
    }
}

/// Dummy id used for the InitializeRequest and expected in the response
pub const INIT_REQUEST_ID: &str = "lspmux:initialize_request";

/// specifies server configuration, if another server with the same configuration is requested we
/// can reuse it
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InstanceKey {
    server: String,
    args: Vec<String>,
    workspace_root: PathBuf,
}

impl InstanceKey {
    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub async fn from_proto_init(proto_init: &proto::Init) -> InstanceKey {
        let config = Config::load_or_default().await;

        /// we assume that the top-most directory from the client_cwd containing a file named `Cargo.toml`
        /// is the project root
        fn find_cargo_workspace_root(client_cwd: &str) -> Option<PathBuf> {
            let client_cwd = Path::new(&client_cwd);
            let mut project_root = None;
            for ancestor in client_cwd.ancestors() {
                let cargo_toml = ancestor.join("Cargo.toml");
                if cargo_toml.exists() {
                    project_root = Some(ancestor.to_owned());
                }
            }
            project_root
        }

        /// for non-cargo projects we use a marker file, the first ancestor directory containing a
        /// marker file `.ra-multiplex-workspace-root` will be considered the workspace root
        fn find_ra_multiplex_workspace_root(client_cwd: &str) -> Option<PathBuf> {
            let client_cwd = Path::new(&client_cwd);
            for ancestor in client_cwd.ancestors() {
                let marker = ancestor.join(".ra-multiplex-workspace-root");
                if marker.exists() {
                    return Some(ancestor.to_owned());
                }
            }
            None
        }

        // workspace root defaults to the client cwd, this is suboptimal because we may spawn more
        // server instances than required but should always be correct at least
        let mut workspace_root = PathBuf::from(&proto_init.cwd);

        if config.workspace_detection {
            // naive detection whether the requested server is likely rust-analyzer
            if proto_init.server.contains("rust-analyzer") {
                if let Some(cargo_root) = find_cargo_workspace_root(&proto_init.cwd) {
                    workspace_root = cargo_root;
                }
            }
            // the ra-multiplex marker file overrides even the cargo workspace detection if present
            if let Some(marked_root) = find_ra_multiplex_workspace_root(&proto_init.cwd) {
                workspace_root = marked_root;
            }
        }

        InstanceKey {
            workspace_root,
            server: proto_init.server.clone(),
            args: proto_init.args.clone(),
        }
    }
}

/// Language server instance
pub struct Instance {
    pid: u32,
    /// wakes up the wait_task and asks it to send SIGKILL to the instance
    close: Notify,
    key: InstanceKey,
    pub init_cache: InitializeCache,
    pub message_readers: Mutex<HashMap<u16, mpsc::Sender<Message>>>,
    pub message_writer: mpsc::Sender<Message>,

    /// Last time a message was sent to this instance
    ///
    /// Uses UTC unix timestamp ([utc_now] function)
    last_used: AtomicI64,
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
}

fn utc_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl Drop for Instance {
    fn drop(&mut self) {
        debug!("instance dropped");
    }
}

pub struct InstanceMap {
    map: HashMap<InstanceKey, Arc<Instance>>,
}

impl InstanceMap {
    pub async fn new() -> Arc<Mutex<Self>> {
        let instance_map = Arc::new(Mutex::new(InstanceMap {
            map: HashMap::new(),
        }));
        let config = Config::load_or_default().await;
        task::spawn(gc_task(
            instance_map.clone(),
            config.gc_interval,
            config.instance_timeout,
        ));
        instance_map
    }
}

/// find or spawn an instance for a `project_root`
pub async fn get(
    instance_map: Arc<Mutex<InstanceMap>>,
    key: &InstanceKey,
) -> Result<Arc<Instance>> {
    match instance_map.lock().await.map.entry(key.to_owned()) {
        Entry::Occupied(e) => Ok(Arc::clone(e.get())),
        Entry::Vacant(e) => {
            let new = Instance::spawn(key, instance_map.clone()).context("spawn ra instance")?;
            e.insert(Arc::clone(&new));
            Ok(new)
        }
    }
}

/// Periodically checks for for idle language server instances
#[instrument("garbage collector", skip_all)]
async fn gc_task(
    instance_map: Arc<Mutex<InstanceMap>>,
    gc_interval: u32,
    instance_timeout: Option<u32>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(gc_interval.into()));
    loop {
        interval.tick().await;

        for (key, instance) in instance_map.lock().await.map.iter() {
            let mut message_readers = instance.message_readers.lock().await;

            // Remove closed senders
            //
            // We have to check here because otherwise the senders only get
            // removed when a message is sent to them which might leave them
            // hanging forever if the language server is quiet and cause the
            // GC to never trigger.
            message_readers.retain(|_port, sender| !sender.is_closed());

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

impl Instance {
    #[instrument(name = "instance", fields(path = ?key.workspace_root), skip_all, parent = None)]
    fn spawn(key: &InstanceKey, instance_map: Arc<Mutex<InstanceMap>>) -> Result<Arc<Instance>> {
        let mut child = Command::new(&key.server)
            .args(&key.args)
            .current_dir(&key.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "couldn't spawn rust-analyzer with command: `{}{}{}`",
                    &key.server,
                    if key.args.is_empty() { "" } else { " " },
                    key.args.join(" ")
                )
            })?;

        let pid = child.id().context("child exited early, couldn't get PID")?;
        let stderr = child.stderr.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (message_writer, rx) = mpsc::channel(64);

        let instance = Arc::new(Instance {
            pid,
            close: Notify::new(),
            key: key.to_owned(),
            init_cache: InitializeCache::default(),
            message_readers: Mutex::default(),
            message_writer,
            last_used: AtomicI64::new(utc_now()),
        });

        task::spawn(wait_task(instance.clone(), instance_map, child).in_current_span());
        task::spawn(stderr_task(stderr).in_current_span());
        task::spawn(stdout_task(instance.clone(), stdout).in_current_span());
        task::spawn(stdin_task(rx, stdin).in_current_span());

        info!(server = ?key.server, args = ?key.args, "spawned");

        Ok(instance)
    }
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

/// Read messages sent by clients from a channel and write them into language server stdin
async fn stdin_task(mut receiver: mpsc::Receiver<Message>, stdin: ChildStdin) {
    let mut writer = LspWriter::new(stdin);

    // Because we (stdin task) don't keep a reference to `self` it will be dropped when the
    // child closes and all the clients disconnect including the sender and this receiver
    // will not keep blocking (unlike in client input task)
    while let Some(message) = receiver.recv().await {
        trace!(?message, "-> server");
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

/// Wait for child and logs when it exits
async fn wait_task(
    instance: Arc<Instance>,
    instance_map: Arc<Mutex<InstanceMap>>,
    mut child: Child,
) {
    loop {
        select! {
            _ = instance.close.notified() => {
                if let Err(err) = child.start_kill() {
                    error!(?err, "failed to close child");
                }
            }
            exit = child.wait() => {
                // Remove the closing instance from the map so new clients spawn their own instance
                instance_map.lock().await.map.remove(&instance.key);

                // Disconnect all current clients
                //
                // We'll rely on the editor client to restart the ra-multiplex client,
                // start a new connection and we'll spawn another instance like we'd with
                // any other new client.
                instance.message_readers.lock().await.clear();

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

/// Read messages from server stdout and distribute them to client channels
async fn stdout_task(instance: Arc<Instance>, stdout: ChildStdout) {
    let mut reader = LspReader::new(BufReader::new(stdout));

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
        trace!(?message, "<- server");

        // Locked after we have a message to send
        let message_readers = instance.message_readers.lock().await;
        match message {
            Message::ResponseSuccess(res) if res.id == INIT_REQUEST_ID => {
                // This is a response to the InitializeRequest, we need to process it
                // separately
                debug!(message = ?Message::from(res.clone()), "recv InitializeRequest response");
                if instance.init_cache.response.set(res).await.is_err() {
                    error!("received multiple InitializeRequest responses from instance")
                }
            }

            Message::ResponseSuccess(mut res) => {
                if let Some((port, id)) = parse_tagged_id(&res.id) {
                    res.id = id;
                    if let Some(sender) = message_readers.get(&port) {
                        let _ignore = sender.send(res.into()).await;
                    } else {
                        debug!("no client");
                    }
                };
            }

            Message::ResponseError(mut res) => {
                if let Some((port, id)) = parse_tagged_id(&res.id) {
                    res.id = id;
                    if let Some(sender) = message_readers.get(&port) {
                        let _ignore = sender.send(res.into()).await;
                    } else {
                        debug!("no client");
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
                // Notification are sent to all clients
                for sender in message_readers.values() {
                    let _ignore = sender.send(notif.clone().into()).await;
                }
            }
        }
    }
}
