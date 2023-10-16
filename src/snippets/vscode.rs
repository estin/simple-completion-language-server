use crate::Snippet;
use serde::Deserialize;
use std::collections::HashMap;

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
#[serde(untagged)]
pub enum VSCodeSnippetBody {
    Single(String),
    List(Vec<String>),
}

#[derive(Deserialize)]
pub struct VSCodeSnippet {
    pub scope: Option<String>,
    pub prefix: VSCodeSnippetPrefix,
    pub body: VSCodeSnippetBody,
    pub description: Option<String>,
}

impl std::fmt::Display for VSCodeSnippetBody {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                VSCodeSnippetBody::Single(v) => v.to_owned(),
                VSCodeSnippetBody::List(v) => v.join("\n"),
            }
        )
    }
}

impl From<VSCodeSnippet> for Vec<Snippet> {
    fn from(value: VSCodeSnippet) -> Vec<Snippet> {
        let scope = value
            .scope
            .map(|v| v.split(',').map(String::from).collect());
        let body = value.body.to_string();

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
