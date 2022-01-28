use std::fmt::{self, Debug};
use std::sync::Arc;

#[derive(Clone)]
pub struct Message {
    port: Option<u16>,
    bytes: Arc<[u8]>,
}

impl Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            Some(port) => f.write_fmt(format_args!("Message(port={port:04x})")),
            None => f.write_str("Message()"),
        }
    }
}

impl Message {
    pub fn new(bytes: &[u8]) -> Self {
        Message {
            port: None,
            bytes: Arc::from(bytes),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    pub fn as_bytes(&self) -> &[u8] {
        &*self.bytes
    }
}
