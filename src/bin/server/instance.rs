use crate::async_once_cell::AsyncOnceCell;
use crate::lsp::{self, Message};
use anyhow::{bail, Context, Result};
use ra_multiplex::config::Config;
use ra_multiplex::proto;
use serde_json::{Number, Value};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Write;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::{self, FromStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tokio::{select, task, time};

/// keeps track of the initialize/initialized handshake for an instance
#[derive(Default)]
pub struct InitializeCache {
    request_sent: AtomicBool,
    notif_sent: AtomicBool,
    pub response: AsyncOnceCell<Message>,
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

/// dummy id used for the InitializeRequest and expected in the response
pub const INIT_REQUEST_ID: &str = "ra-multiplex:initialize_request";

pub type MessageReaders = RwLock<HashMap<u16, mpsc::Sender<Message>>>;

/// specifies server configuration, if another server with the same configuration is requested we
/// can reuse it
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InstanceKey {
    server: String,
    args: Vec<String>,
    workspace_root: PathBuf,
}

impl InstanceKey {
    pub async fn from_proto_init(proto_init: &proto::Init) -> InstanceKey {
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

        InstanceKey {
            workspace_root,
            server: proto_init.server.clone(),
            args: proto_init.args.clone(),
        }
    }
}

pub struct RaInstance {
    pid: u32,
    /// wakes up the wait_task and asks it to send SIGKILL to the instance
    close: Notify,
    /// make sure only one timeout_task is running for an instance
    timeout_running: AtomicBool,
    key: InstanceKey,
    pub init_cache: InitializeCache,
    pub message_readers: MessageReaders,
    pub message_writer: mpsc::Sender<Message>,
}

impl RaInstance {
    /// returns true if this is the first thread attempting to start a timeout task
    fn attempt_start_timeout(&self) -> bool {
        let already_running = self.timeout_running.swap(true, Ordering::SeqCst);
        !already_running
    }

    /// lets another timeout task start
    fn timeout_finished(&self) {
        self.timeout_running.store(false, Ordering::SeqCst);
    }
}

impl Drop for RaInstance {
    fn drop(&mut self) {
        let path = self.key.workspace_root.display();
        let pid = self.pid;
        log::debug!("[{path} {pid}] instance dropped");
    }
}

#[derive(Clone)]
pub struct InstanceRegistry {
    map: Arc<Mutex<HashMap<InstanceKey, Arc<RaInstance>>>>,
}

impl InstanceRegistry {
    pub async fn new() -> Self {
        let registry = InstanceRegistry {
            map: Arc::default(),
        };
        registry.spawn_gc_task().await;
        registry
    }
}

impl InstanceRegistry {
    /// periodically checks for closed recv channels for every instance
    ///
    /// if an instance has 0 unclosed channels a timeout task is spawned to clean it up
    async fn spawn_gc_task(&self) {
        let config = Config::load_or_default().await;
        let gc_interval = config.gc_interval.into();
        let instance_timeout = config.instance_timeout.map(<_>::from);

        let registry = self.clone();
        let mut closed_ports = Vec::new();
        task::spawn(async move {
            loop {
                time::sleep(Duration::from_secs(gc_interval)).await;

                for (key, instance) in registry.map.lock().await.iter() {
                    for (port, sender) in instance.message_readers.read().await.iter() {
                        if sender.is_closed() {
                            closed_ports.push(*port);
                        }
                    }
                    let mut message_readers = instance.message_readers.write().await;
                    for port in closed_ports.drain(..) {
                        message_readers.remove(&port);
                    }
                    if let Some(instance_timeout) = instance_timeout {
                        if message_readers.is_empty() && instance.attempt_start_timeout() {
                            registry.spawn_timeout_task(key, instance_timeout);
                            let pid = instance.pid;
                            let path = key.workspace_root.display();
                            log::warn!("[{path} {pid}] no clients, close timeout started ({instance_timeout} seconds)");
                        }
                    } else {
                        // if there is no instance timeout we don't need to spawn the timeout task
                    }
                }
            }
        });
    }

    /// closes a rust-analyzer instance after a timeout if it still doesn't have any
    fn spawn_timeout_task(&self, key: &InstanceKey, instance_timeout: u64) {
        let registry = self.clone();
        let key = key.to_owned();
        task::spawn(async move {
            time::sleep(Duration::from_secs(instance_timeout)).await;

            if let Some(instance) = registry.map.lock().await.get(&key) {
                if instance.message_readers.read().await.is_empty() {
                    instance.close.notify_one();
                }
                instance.timeout_finished();
            } else {
                // instance has already been deleted
            }
        });
    }

    /// find or spawn an instance for a `project_root`
    pub async fn get(&self, key: &InstanceKey) -> Result<Arc<RaInstance>> {
        let mut map = self.map.lock().await;
        match map.entry(key.to_owned()) {
            Entry::Occupied(e) => Ok(Arc::clone(e.get())),
            Entry::Vacant(e) => {
                let new = RaInstance::spawn(key, self.clone()).context("spawn ra instance")?;
                e.insert(Arc::clone(&new));
                Ok(new)
            }
        }
    }
}

impl RaInstance {
    fn spawn(key: &InstanceKey, registry: InstanceRegistry) -> Result<Arc<RaInstance>> {
        let mut child = Command::new(&key.server)
            .args(&key.args)
            .current_dir(&key.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("couldn't spawn rust-analyzer")?;

        let pid = child.id().context("child exited early, couldn't get PID")?;
        let stderr = child.stderr.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (message_writer, rx) = mpsc::channel(64);

        let instance = Arc::new(RaInstance {
            pid,
            close: Notify::new(),
            timeout_running: AtomicBool::new(false),
            key: key.to_owned(),
            init_cache: InitializeCache::default(),
            message_readers: MessageReaders::default(),
            message_writer,
        });

        instance.spawn_wait_task(child, registry);
        instance.spawn_stderr_task(stderr);
        instance.spawn_stdout_task(stdout);
        instance.spawn_stdin_task(rx, stdin);

        let path = key.workspace_root.display();
        log::info!("[{path} {pid}] spawned {server}", server = &key.server);

        Ok(instance)
    }

    /// read errors from server stderr and log them
    fn spawn_stderr_task(self: &Arc<Self>, stderr: ChildStderr) {
        let pid = self.pid;
        let path = self.key.workspace_root.display().to_string();
        let mut stderr = BufReader::new(stderr);
        let mut buffer = String::new();

        task::spawn(async move {
            loop {
                buffer.clear();
                match stderr.read_line(&mut buffer).await {
                    Ok(0) => {
                        // reached EOF
                        log::debug!("[{path} {pid}] instance stderr closed");
                        break;
                    }
                    Ok(_) => {
                        let line = buffer.trim_end(); // remove trailing '\n' or possibly '\r\n'
                        log::error!("[{path} {pid}] {line}");
                    }
                    Err(err) => {
                        let err = anyhow::Error::from(err);
                        log::error!("[{path} {pid}] error reading from stderr: {err:?}");
                    }
                }
            }
        });
    }

    /// read messages from server stdout and distribute them to client channels
    fn spawn_stdout_task(self: &Arc<Self>, stdout: ChildStdout) {
        let stdout = BufReader::new(stdout);
        let instance = Arc::clone(self);
        let pid = self.pid;
        let path = instance.key.workspace_root.display().to_string();

        task::spawn(async move {
            match read_server_socket(stdout, &instance.message_readers, &instance.init_cache).await
            {
                Err(err) => log::error!("[{path} {pid}] error reading from stdout: {err:?}"),
                Ok(_) => log::debug!("[{path} {pid}] instance stdout closed"),
            }
        });
    }

    /// read messages sent by clients from a channel and write them into server stdin
    fn spawn_stdin_task(self: &Arc<Self>, rx: mpsc::Receiver<Message>, stdin: ChildStdin) {
        let pid = self.pid;
        let path = self.key.workspace_root.display().to_string();
        let mut receiver = rx;
        let mut stdin = stdin;

        task::spawn(async move {
            // because we (stdin task) don't keep a reference to `self` it will be dropped when the
            // child closes and all the clients disconnect including the sender and this receiver
            // will not keep blocking (unlike in client input task)
            while let Some(message) = receiver.recv().await {
                if let Err(err) = message.to_writer(&mut stdin).await {
                    match err.kind() {
                        // stdin is closed, no need to log an error
                        ErrorKind::BrokenPipe => {}
                        _ => {
                            let err = anyhow::Error::from(err);
                            log::error!("[{path} {pid}] error writing to stdin: {err:?}");
                        }
                    }
                    break;
                }
            }
            log::debug!("[{path} {pid}] instance stdin closed");
        });
    }

    /// waits for child and logs when it exits
    fn spawn_wait_task(self: &Arc<Self>, child: Child, registry: InstanceRegistry) {
        let key = self.key.clone();
        let pid = self.pid;
        let path = key.workspace_root.display().to_string();
        let instance = Arc::clone(self);
        task::spawn(async move {
            let mut child = child;
            loop {
                select! {
                    _ = instance.close.notified() => {
                        if let Err(err) = child.start_kill() {
                            log::error!("[{path} {pid}] failed to close rust-analyzer: {err}");
                        }
                    }
                    exit = child.wait() => {
                        // remove the closing instance from the registry so new clients spawn new
                        // instance
                        registry.map.lock().await.remove(&key);

                        // disconnect all current clients
                        //
                        // we'll rely on the editor client to restart the ra-multiplex client,
                        // start a new connection and we'll spawn another instance like we'd with
                        // any other new client
                        let _ = instance.message_readers.write().await.drain();

                        match exit {
                            Ok(status) => {
                                let success = if status.success() {
                                    "successfully"
                                } else {
                                    "with error"
                                };
                                let mut details = String::new();
                                if let Some(code) = status.code() {
                                    details.write_fmt(format_args!(" code {code}")).unwrap();
                                }
                                #[cfg(unix)]
                                {
                                    if let Some(signal) = status.signal() {
                                        details.write_fmt(format_args!(" signal {signal}")).unwrap();
                                    }
                                }
                                let pid = instance.pid;
                                log::error!("[{path} {pid}] child exited {success}{details}");
                            }
                            Err(err) => log::error!("[{path} {pid}] error waiting for child: {err}"),
                        }
                        break;
                    }
                }
            }
        });
    }
}

fn parse_tagged_id(tagged: &str) -> Result<(u16, Value)> {
    let (port, rest) = tagged.split_once(':').context("missing first `:`")?;
    let port = u16::from_str_radix(port, 16)?;
    let (value_type, old_id) = rest.split_once(':').context("missing second `:`")?;
    let old_id = match value_type {
        "n" => Value::Number(Number::from_str(old_id)?),
        "s" => Value::String(old_id.to_owned()),
        _ => bail!("invalid tag type `{value_type}`"),
    };
    Ok((port, old_id))
}

async fn read_server_socket(
    mut reader: BufReader<ChildStdout>,
    senders: &MessageReaders,
    init_cache: &InitializeCache,
) -> Result<()> {
    let mut buffer = Vec::new();

    while let Some((mut json, bytes)) = lsp::read_message(&mut reader, &mut buffer).await? {
        log::trace!("{}", Value::Object(json.clone()));

        if let Some(id) = json.get("id") {
            // we tagged the request id so we expect to only receive tagged responses
            let tagged_id = match id {
                Value::String(string) if string == INIT_REQUEST_ID => {
                    // this is a response to the InitializeRequest, we need to process it
                    // separately
                    log::debug!("recv InitializeRequest response");
                    init_cache
                        .response
                        .set(Message::from_bytes(bytes))
                        .await
                        .ok() // throw away the Err(message), we don't need it and it doesn't implement std::error::Error
                        .context("received multiple InitializeRequest responses from instance")?;
                    continue;
                }
                Value::String(string) => string,
                _ => {
                    log::debug!("response to no request {}", Value::Object(json));
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
                    continue;
                }
            };

            let (port, old_id) = match parse_tagged_id(tagged_id) {
                Ok(ok) => ok,
                Err(err) => {
                    log::warn!("invalid tagged id {err:?}");
                    continue;
                }
            };

            json.insert("id".to_owned(), old_id);

            if let Some(sender) = senders.read().await.get(&port) {
                let message = Message::from_json(&json, &mut buffer);
                // ignore closed channels
                let _ignore = sender.send(message).await;
            } else {
                log::warn!("[{port}] no client");
            }
        } else {
            // notification messages without an id are sent to all clients
            let message = Message::from_bytes(bytes);
            for sender in senders.read().await.values() {
                // ignore closed channels
                let _ignore = sender.send(message.clone()).await;
            }
        }
    }
    Ok(())
}
