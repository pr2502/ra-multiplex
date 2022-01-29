use crate::server::async_once_cell::AsyncOnceCell;
use crate::server::lsp::{self, Message};
use anyhow::{bail, Context, Result};
use serde_json::{Number, Value};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::{self, FromStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tokio::{select, task, time};

pub struct InitializeCache {
    request_sent: AtomicBool,
    notif_sent: AtomicBool,
    pub response: AsyncOnceCell<Message>,
}

impl Default for InitializeCache {
    fn default() -> Self {
        InitializeCache {
            request_sent: AtomicBool::new(false),
            notif_sent: AtomicBool::new(false),
            response: AsyncOnceCell::default(),
        }
    }
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

type MessageReaders = RwLock<HashMap<u16, mpsc::Sender<Message>>>;

pub struct RaInstance {
    kill: Notify,
    project_root: PathBuf,
    pub init_cache: InitializeCache,
    pub message_readers: MessageReaders,
    pub message_writer: mpsc::Sender<Message>,
}

#[derive(Clone)]
pub struct InstanceRegistry {
    inner: Arc<InnerRegistry>,
}

#[derive(Default)]
struct InnerRegistry {
    reap_instances: Notify,
    map: Mutex<HashMap<PathBuf, Arc<RaInstance>>>,
}

impl InstanceRegistry {
    pub fn new() -> Self {
        let registry = InstanceRegistry {
            inner: Arc::new(InnerRegistry::default()),
        };
        registry.spawn_reaper_task();
        registry
    }

    fn spawn_reaper_task(&self) {
        let registry = self.clone();
        task::spawn(async move {
            loop {
                // check periodically if there are no clients since the notification only triggers
                // if rust-analyzer tries to send a message to a disconnected client
                select! {
                    _ = registry.inner.reap_instances.notified() => {}
                    _ = time::sleep(Duration::from_secs(10)) => {}
                };
                for (path, instance) in registry.inner.map.lock().await.iter() {
                    if instance.message_readers.read().await.is_empty() {
                        registry.spawn_timeout_task(path);
                        let path = path.display();
                        log::warn!("[{path}] no clients, kill timeout started");
                    }
                }
            }
        });
    }

    fn spawn_timeout_task(&self, path: &Path) {
        let registry = self.clone();
        let path = path.to_owned();
        task::spawn(async move {
            // wait before trying to kill rust-analyzer in case another client connects
            time::sleep(Duration::from_secs(5 * 60)).await;

            if let Some(instance) = registry.inner.map.lock().await.get(&path) {
                if instance.message_readers.read().await.is_empty() {
                    instance.kill.notify_one();
                } else {
                    // a client connected before the timeout finished
                }
            } else {
                let path = path.display();
                log::warn!("[{path}] instance was already removed");
            }
        });
    }

    pub async fn get(&self, project_root: &Path) -> Result<Arc<RaInstance>> {
        let mut map = self.inner.map.lock().await;
        match map.entry(project_root.to_owned()) {
            Entry::Occupied(e) => Ok(Arc::clone(e.get())),
            Entry::Vacant(e) => {
                let new =
                    RaInstance::spawn(project_root, self.clone()).context("spawn ra instance")?;
                e.insert(Arc::clone(&new));
                Ok(new)
            }
        }
    }
}

impl RaInstance {
    fn spawn(project_root: &Path, registry: InstanceRegistry) -> Result<Arc<RaInstance>> {
        let mut child = Command::new("rust-analyzer")
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("couldn't spawn rust-analyzer")?;

        let stderr = child.stderr.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (message_writer, rx) = mpsc::channel(64);

        let instance = Arc::new(RaInstance {
            kill: Notify::new(),
            project_root: project_root.to_owned(),
            init_cache: InitializeCache::default(),
            message_readers: MessageReaders::default(),
            message_writer,
        });

        instance.spawn_wait_task(child, registry.clone());
        instance.spawn_stderr_task(stderr);
        instance.spawn_stdout_task(stdout, registry);
        instance.spawn_stdin_task(rx, stdin);

        let path = project_root.display();
        log::info!("[{path}] spawned rust-analyzer");

        Ok(instance)
    }

    /// read errors from server stderr and log them
    fn spawn_stderr_task(self: &Arc<Self>, stderr: ChildStderr) {
        let path = self.project_root.display().to_string();
        let mut stderr = BufReader::new(stderr);
        let mut buffer = String::new();

        task::spawn(async move {
            loop {
                buffer.clear();
                match stderr.read_line(&mut buffer).await {
                    Ok(0) => {
                        // reached EOF
                        log::info!("[{path}] instance stderr closed");
                        break;
                    }
                    Ok(_) => {
                        let line = buffer.trim_end(); // remove trailing '\n' or possibly '\r\n'
                        log::error!("[{path}] {line}");
                    }
                    Err(err) => {
                        let err = anyhow::Error::from(err);
                        log::error!("[{path}] error reading from stderr: {err:?}");
                    }
                }
            }
        });
    }

    /// read messages from server stdout and distribute them to client channels
    fn spawn_stdout_task(self: &Arc<Self>, stdout: ChildStdout, registry: InstanceRegistry) {
        let stdout = BufReader::new(stdout);
        let instance = Arc::clone(self);
        let path = instance.project_root.display().to_string();

        task::spawn(async move {
            match read_server_socket(
                stdout,
                &instance.message_readers,
                &instance.init_cache,
                &registry,
            )
            .await
            {
                Err(err) => log::error!("[{path}] error reading from stdout: {err:?}"),
                Ok(_) => log::info!("[{path}] instance stdout closed"),
            }
        });
    }

    /// read messages sent by clients from a channel and write them into server stdin
    fn spawn_stdin_task(self: &Arc<Self>, rx: mpsc::Receiver<Message>, stdin: ChildStdin) {
        let path = self.project_root.display().to_string();
        let mut receiver = rx;
        let mut stdin = stdin;

        task::spawn(async move {
            while let Some(message) = receiver.recv().await {
                if let Err(err) = message.to_writer(&mut stdin).await {
                    let err = anyhow::Error::from(err);
                    log::error!("[{path}] error writing to stdin: {err:?}");
                    break;
                }
            }
            log::info!("[{path}] instance stdin closed");
        });
    }

    /// waits for child and logs when it exits
    fn spawn_wait_task(self: &Arc<Self>, child: Child, registry: InstanceRegistry) {
        let project_root = self.project_root.clone();
        let path = project_root.display().to_string();
        let instance = Arc::clone(self);
        task::spawn(async move {
            let mut child = child;
            loop {
                select! {
                    _ = instance.kill.notified() => {
                        if let Err(err) = child.start_kill() {
                            log::error!("[{path}] failed to kill rust-analyzer: {err}");
                        }
                    }
                    exit = child.wait() => {
                        registry.inner.map.lock().await.remove(&project_root);
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
                                if let Some(signal) = status.signal() {
                                    details.write_fmt(format_args!(" signal {signal}")).unwrap();
                                }
                                log::error!("[{path}] child exited {success}{details}");
                            }
                            Err(err) => log::error!("[{path}] error waiting for child: {err}"),
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

async fn read_server_socket<R>(
    mut reader: R,
    senders: &MessageReaders,
    init_cache: &InitializeCache,
    registry: &InstanceRegistry,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut buffer = Vec::new();
    let mut disconnected_clients = Vec::new();

    while let Some((mut json, bytes)) = lsp::read_message(&mut reader, &mut buffer).await? {
        log::trace!("{}", Value::Object(json.clone()));

        if let Some(id) = json.get("id") {
            // we tagged the request id so we expect to only receive tagged responses
            let tagged_id = match id {
                Value::String(string) if string == "initialize_request" => {
                    // this is a response to the InitializeRequest, we need to process it
                    // separately
                    log::debug!("recv InitializeRequest response");
                    // NOTE we just guess the first request had originally the id Number(0)
                    json.insert("id".to_owned(), Value::Number(0.into()));
                    init_cache
                        .response
                        .set(Message::from_json(&json, &mut buffer))
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
                if sender.send(message).await.is_err() {
                    disconnected_clients.push(port);
                }
            } else {
                log::warn!("[{port}] no client");
            }
        } else {
            // notification messages without an id are sent to all clients
            let message = Message::from_bytes(bytes);
            for (port, sender) in senders.read().await.iter() {
                if sender.send(message.clone()).await.is_err() {
                    disconnected_clients.push(*port);
                }
            }
        }

        // drop `Sender`s of disconnected clients
        if !disconnected_clients.is_empty() {
            let mut senders = senders.write().await;
            for port in disconnected_clients.drain(..) {
                senders.remove(&port);
            }
            registry.inner.reap_instances.notify_waiters();
        }
    }
    Ok(())
}
