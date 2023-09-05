use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize)]
pub struct SnippetsConfig {
    pub snippets: Vec<Snippet>,
}

#[derive(Debug, Deserialize)]
pub struct Snippet {
    pub scope: Option<Vec<String>>,
    pub prefix: String,
    pub body: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct VSSnippetsConfig {
    #[serde(flatten)]
    pub snippets: HashMap<String, VSCodeSnippet>,
}

#[derive(Deserialize)]
pub struct VSCodeSnippet {
    pub scope: Option<String>,
    pub prefix: String,
    pub body: Vec<String>,
    pub description: Option<String>,
}

impl From<VSCodeSnippet> for Snippet {
    fn from(value: VSCodeSnippet) -> Snippet {
        Snippet {
            scope: value
                .scope
                .map(|v| v.split(',').map(String::from).collect()),
            prefix: value.prefix,
            body: value.body.join("\n"),
            description: value.description,
        }
    }
}
