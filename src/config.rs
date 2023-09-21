use anyhow::Context;
use directories::ProjectDirs;
use serde::de::{Error, Unexpected};
use serde::{Deserialize, Deserializer};
use serde_derive::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use tokio::fs;
use tokio::sync::OnceCell;

mod default {
    use super::*;

    pub fn instance_timeout() -> Option<u32> {
        // 5 minutes
        Some(5 * 60)
    }

    pub fn min_available_memory() -> Option<byte_unit::Byte> {
        None
    }

    pub fn gc_interval() -> u32 {
        // 10 seconds
        10
    }

    pub fn listen() -> (IpAddr, u16) {
        // localhost & some random unprivileged port
        (IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 27_631)
    }

    pub fn connect() -> (IpAddr, u16) {
        listen()
    }

    pub fn log_filters() -> String {
        "info".to_owned()
    }

    pub fn workspace_detection() -> bool {
        true
    }
}

mod de {
    use super::*;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOf<T> {
        Bool(bool),
        Value(T),
    }

    /// parse either bool(false) or u32
    pub fn instance_timeout<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match OneOf::deserialize(deserializer) {
            Ok(OneOf::Value(value)) => Ok(Some(value)),
            Ok(OneOf::Bool(false)) => Ok(None),
            Ok(OneOf::Bool(true)) => Err(Error::invalid_value(
                Unexpected::Bool(true),
                &"a non-negative integer or false",
            )),
            Err(_) => Err(Error::custom("invalid type: expected a non-negative integer or false")),
        }
    }

    /// parse either bool(false) or a number in bytes
    pub fn min_available_memory<'de, D>(
        deserializer: D,
    ) -> Result<Option<byte_unit::Byte>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match OneOf::deserialize(deserializer) {
            Ok(OneOf::Value(value)) => Ok(Some(value)),
            Ok(OneOf::Bool(false)) => Ok(None),
            Ok(OneOf::Bool(true)) => Err(Error::invalid_value(
                Unexpected::Bool(true),
                &"a value in bytes (e.g. '100 MB') or false",
            )),
            Err(_) => Err(Error::custom("invalid type: expected a value in bytes (e.g. '100 MB') or false")),
        }
    }

    /// make sure the value is greater than 0 to giver users feedback on invalid configuration
    pub fn gc_interval<'de, D>(deserializer: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        match u32::deserialize(deserializer)? {
            0 => Err(Error::invalid_value(
                Unexpected::Unsigned(0),
                &"an integer 1 or greater",
            )),
            value => Ok(value),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default::instance_timeout")]
    #[serde(deserialize_with = "de::instance_timeout")]
    pub instance_timeout: Option<u32>,

    #[serde(default = "default::min_available_memory")]
    #[serde(deserialize_with = "de::min_available_memory")]
    pub min_available_memory: Option<byte_unit::Byte>,

    #[serde(default = "default::gc_interval")]
    #[serde(deserialize_with = "de::gc_interval")]
    pub gc_interval: u32,

    #[serde(default = "default::listen")]
    pub listen: (IpAddr, u16),

    #[serde(default = "default::connect")]
    pub connect: (IpAddr, u16),

    #[serde(default = "default::log_filters")]
    pub log_filters: String,

    #[serde(default = "default::workspace_detection")]
    pub workspace_detection: bool,
}

#[cfg(test)]
#[test]
fn generate_default_and_check_it_matches_commited_defaults() {
    use std::fs;
    use std::path::Path;

    let generated_defaults = Config::default_values();
    let generated_defaults = toml::to_string(&generated_defaults).expect("failed serialize");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("defaults.toml");
    let saved_defaults = fs::read_to_string(path).expect("failed reading defaults.toml file");

    assert_eq!(generated_defaults, saved_defaults);
}

impl Config {
    fn default_values() -> Self {
        Config {
            instance_timeout: default::instance_timeout(),
            gc_interval: default::gc_interval(),
            listen: default::listen(),
            connect: default::connect(),
            log_filters: default::log_filters(),
            workspace_detection: default::workspace_detection(),
            min_available_memory: default::min_available_memory(),
        }
    }

    async fn try_load() -> anyhow::Result<Self> {
        let pkg_name = env!("CARGO_PKG_NAME");
        let config_path = ProjectDirs::from("", "", pkg_name)
            .context("project config directory not found")?
            .config_dir()
            .join("config.toml");
        let path = config_path.display();
        let config_data = fs::read(&config_path)
            .await
            .with_context(|| format!("cannot read config file `{path}`"))?;
        toml::from_slice(&config_data).with_context(|| format!("cannot parse config file `{path}`"))
    }

    /// panics if called multiple times
    fn init_logger(&self) {
        env_logger::Builder::from_env(env_logger::Env::new().default_filter_or(&self.log_filters))
            .format_timestamp(None)
            .format_module_path(false)
            .format_target(false)
            .init();
    }

    /// tries to load configuration file from the standard location, if it fails it constructs
    /// a configuration with default values
    ///
    /// initializes a global logger based on the configuration
    pub async fn load_or_default() -> &'static Self {
        static GLOBAL: OnceCell<&'static Config> = OnceCell::const_new();
        GLOBAL
            .get_or_init(|| async {
                let (config, load_err) = match Self::try_load().await {
                    Ok(config) => (config, None),
                    Err(err) => (Self::default_values(), Some(err)),
                };
                let global_config = Box::leak(Box::new(config));
                global_config.init_logger();
                if let Some(load_err) = load_err {
                    // log only after the logger has been initialized
                    log::info!("cannot load config, continuing with defaults: {load_err:?}");
                }
                &*global_config
            })
            .await
    }
}
