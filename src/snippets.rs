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
#[serde(untagged)]
pub enum VSCodeSnippetPrefix {
    Single(String),
    List(Vec<String>),
}

#[derive(Deserialize)]
pub struct VSCodeSnippet {
    pub scope: Option<String>,
    pub prefix: VSCodeSnippetPrefix,
    pub body: Vec<String>,
    pub description: Option<String>,
}

impl From<VSCodeSnippet> for Vec<Snippet> {
    fn from(value: VSCodeSnippet) -> Vec<Snippet> {
        let scope = value
            .scope
            .map(|v| v.split(',').map(String::from).collect());
        let body = value.body.join("\n");

        match value.prefix {
            VSCodeSnippetPrefix::Single(prefix) => {
                vec![Snippet {
                    scope,
                    prefix,
                    body,
                    description: value.description,
                }]
            }
            VSCodeSnippetPrefix::List(prefixes) => prefixes
                .into_iter()
                .map(|prefix| Snippet {
                    scope: scope.clone(),
                    prefix,
                    body: body.clone(),
                    description: value.description.clone(),
                })
                .collect(),
        }
    }
}
