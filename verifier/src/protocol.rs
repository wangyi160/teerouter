use serde::Serialize;
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: String,
    pub detail: String,
}
impl Check {
    pub fn new(
        name: impl Into<String>,
        status: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: status.into(),
            detail: detail.into(),
        }
    }
}
