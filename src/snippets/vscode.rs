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
pub enum VSCodeSnippetValue {
    Single(String),
    List(Vec<String>),
}

#[derive(Deserialize)]
pub struct VSCodeSnippet {
    pub scope: Option<String>,
    pub prefix: Option<VSCodeSnippetValue>,
    pub body: VSCodeSnippetValue,
    pub description: Option<VSCodeSnippetValue>,
}

impl VSCodeSnippet {
    pub fn prefix(self, prefix: String) -> Self {
        Self {
            prefix: Some(VSCodeSnippetValue::Single(prefix)),
            ..self
        }
    }
}

impl std::fmt::Display for VSCodeSnippetValue {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                VSCodeSnippetValue::Single(v) => v.to_owned(),
                VSCodeSnippetValue::List(v) => v.join("\n"),
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
        let description = value.description.map(|v| v.to_string());

        match value.prefix {
            Some(VSCodeSnippetValue::Single(prefix)) => {
                vec![Snippet {
                    scope,
                    prefix,
                    body,
                    description,
                }]
            }
            Some(VSCodeSnippetValue::List(prefixes)) => prefixes
                .into_iter()
                .map(|prefix| Snippet {
                    scope: scope.clone(),
                    prefix,
                    body: body.clone(),
                    description: description.clone(),
                })
                .collect(),
            None => Vec::new(),
        }
    }
}
