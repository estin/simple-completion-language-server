use serde::Deserialize;

#[derive(Deserialize)]
pub struct SnippetsConfig {
    pub snippets: Vec<Snippet>,
}

#[derive(Deserialize)]
pub struct Snippet {
    pub scope: Option<Vec<String>>,
    pub prefix: String,
    pub body: String,
    pub description: Option<String>,
}
