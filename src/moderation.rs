//! Retired LockLLM configuration. Accepted only to restore existing checkpoints.
mod report;
pub(crate) use report::UserRecord;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub api_key_ref: String,
    /// Legacy proxy binding; ignored along with the rest of this configuration.
    pub proxy: Option<String>,
    pub sensitivity: Sensitivity,
    pub timeout_secs: u64,
    pub max_input_bytes: usize,
    pub max_concurrent: usize,
    pub rpm: u64,
    #[cfg(test)]
    pub(crate) test_endpoint: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            api_key_ref: String::new(),
            proxy: None,
            sensitivity: Sensitivity::Medium,
            timeout_secs: 30,
            max_input_bytes: 16 * 1024 * 1024,
            max_concurrent: 16,
            rpm: 300,
            #[cfg(test)]
            test_endpoint: None,
        }
    }
}
#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Low,
    #[default]
    Medium,
    High,
}
