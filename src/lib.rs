use serde::{Deserialize, Serialize};
use std::env;

pub const PORT: u16 = 27_631;

#[derive(Serialize, Deserialize)]
pub struct ProtoInit {
    pub proto: String,
    pub version: String,
    pub cwd: String,
    pub args: Vec<String>,
}

impl ProtoInit {
    pub fn new() -> ProtoInit {
        ProtoInit {
            proto: env!("CARGO_PKG_NAME").to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            cwd: env::current_dir()
                .expect("cannot access current directory")
                .display()
                .to_string(),
            args: env::args().skip(1).collect(),
        }
    }

    /// returns true if the version matches
    pub fn check_version(&self) -> bool {
        self.proto == env!("CARGO_PKG_NAME") && self.version == env!("CARGO_PKG_VERSION")
    }
}
