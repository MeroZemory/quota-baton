use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub pid: u32,
    pub started_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportMode {
    Linked,
    Adopted,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CpaImportRequest {
    pub source: String,
    pub mode: ImportMode,
    #[serde(default)]
    pub dir: Option<std::path::PathBuf>,
    #[serde(default)]
    pub activate: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
}
