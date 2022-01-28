use std::fmt::{self, Debug};
use std::sync::Arc;

#[derive(Clone)]
pub struct Message {
    bytes: Arc<[u8]>,
}

impl Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Message")
    }
}

impl Message {
    pub fn new(bytes: &[u8]) -> Self {
        Message {
            bytes: Arc::from(bytes),
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &*self.bytes
    }
}
