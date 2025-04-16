use anyhow::Result;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
pub struct ExternalSnippets {
    pub sources: Vec<SnippetSource>,
}

#[derive(Debug, Deserialize)]
pub struct SnippetSource {
    pub name: Option<String>,
    pub git: String,
    #[serde(default)]
    pub paths: Vec<SourcePath>,
}

#[derive(Debug, Deserialize)]
pub struct SourcePath {
    pub scope: Option<Vec<String>>,
    pub path: String,
}

impl SnippetSource {
    pub fn destination_path(&self) -> Result<std::path::PathBuf> {
        // TODO may be use Url crate?
        // normalize url
        let url = self
            .git
            .split('?')
            .nth(0)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", self.git))?;
        let source = url
            .split("://")
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", self.git))?;

        Ok(std::path::PathBuf::from_str(source)?)
    }
}
