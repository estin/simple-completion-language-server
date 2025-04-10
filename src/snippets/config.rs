use crate::snippets::external::ExternalSnippets;
use crate::snippets::vscode::VSSnippetsConfig;
use crate::StartOptions;
use anyhow::Result;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

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

#[derive(Debug, Deserialize)]
pub struct UnicodeInputItem {
    pub prefix: String,
    pub body: String,
}

#[derive(Deserialize)]
pub struct UnicodeInputConfig {
    #[serde(flatten)]
    pub inner: BTreeMap<String, String>,
}

pub fn load_snippets(start_options: &StartOptions) -> Result<Vec<Snippet>> {
    let mut snippets = load_snippets_from_path(&start_options.snippets_path, &None)?;

    tracing::info!(
        "Try read config from: {:?}",
        start_options.external_snippets_config_path
    );

    let path = std::path::Path::new(&start_options.external_snippets_config_path);

    if path.exists() {
        let Some(base_path) = path.parent() else {
            anyhow::bail!("Failed to get base path")
        };

        let base_path = base_path.join("external-snippets");

        let content = std::fs::read_to_string(path)?;

        let sources = toml::from_str::<ExternalSnippets>(&content)
            .map(|sc| sc.sources)
            .map_err(|e| anyhow::anyhow!(e))?;

        for source in sources {
            let source_name = source.name.as_ref().unwrap_or(&source.git);

            for item in &source.paths {
                snippets.extend(
                    load_snippets_from_path(
                        &base_path.join(source.destination_path()?).join(&item.path),
                        &item.scope,
                    )?
                    .into_iter()
                    .map(|mut s| {
                        s.description = Some(format!(
                            "{source_name}\n\n{}",
                            s.description.unwrap_or_default(),
                        ));
                        s
                    })
                    .collect::<Vec<_>>(),
                );
            }
        }
    }

    snippets.sort_unstable_by(|a, b| a.prefix.cmp(&b.prefix));

    Ok(snippets)
}

pub fn load_snippets_from_file(
    path: &std::path::PathBuf,
    scope: &Option<Vec<String>>,
) -> Result<Vec<Snippet>> {
    let scope = if scope.is_none() {
        path.file_stem()
            .and_then(|v| v.to_str())
            .filter(|v| *v != "snippets")
            .map(|v| vec![v.to_string()])
    } else {
        scope.clone()
    };

    tracing::info!("Try load snippets from: {path:?} for scope: {scope:?}");

    let content = std::fs::read_to_string(path)?;

    let result = match path.extension().and_then(|v| v.to_str()) {
        Some("toml") => toml::from_str::<SnippetsConfig>(&content)
            .map(|sc| sc.snippets)
            .map_err(|e| anyhow::anyhow!(e)),
        Some("json") => serde_json::from_str::<VSSnippetsConfig>(&content)
            .map(|s| {
                s.snippets
                    .into_iter()
                    .map(|(prefix, snippet)| {
                        if snippet.prefix.is_some() {
                            return snippet;
                        }
                        snippet.prefix(prefix)
                    })
                    .flat_map(Into::<Vec<Snippet>>::into)
                    .collect()
            })
            .map_err(|e| anyhow::anyhow!(e)),
        _ => {
            anyhow::bail!("Unsupported snipptes format: {path:?}")
        }
    };

    let snippets = result?;

    if let Some(scope) = scope {
        // add global scope to each snippet
        Ok(snippets
            .into_iter()
            .map(|mut s| {
                s.scope = Some(if let Some(mut v) = s.scope {
                    // TODO unique scope items
                    v.extend(scope.clone());
                    v
                } else {
                    scope.clone()
                });
                s
            })
            .collect())
    } else {
        Ok(snippets)
    }
}

pub fn load_snippets_from_path(
    snippets_path: &std::path::PathBuf,
    scope: &Option<Vec<String>>,
) -> Result<Vec<Snippet>> {
    if snippets_path.is_file() {
        return load_snippets_from_file(snippets_path, scope);
    }

    let mut snippets = Vec::new();
    match std::fs::read_dir(snippets_path) {
        Ok(entries) => {
            for entry in entries {
                let Ok(entry) = entry else { continue };

                let path = entry.path();
                if path.is_dir() {
                    continue;
                };

                match load_snippets_from_file(&path, scope) {
                    Ok(r) => snippets.extend(r),
                    Err(e) => {
                        tracing::error!("On read snippets from {path:?}: {e}");
                        continue;
                    }
                }
            }
        }
        Err(e) => tracing::error!("On read dir {snippets_path:?}: {e}"),
    }

    Ok(snippets)
}

pub fn load_unicode_input_from_file(path: &std::path::PathBuf) -> Result<Vec<(String, String)>> {
    tracing::info!("Try load 'unicode input' config from: {path:?}");

    let content = std::fs::read_to_string(path)?;

    let result = match path.extension().and_then(|v| v.to_str()) {
        Some("toml") => toml::from_str::<HashMap<String, String>>(&content)
            .map(|sc| sc.into_iter().collect())?,
        _ => {
            anyhow::bail!("Unsupported unicode format: {path:?}")
        }
    };

    Ok(result)
}

pub fn load_unicode_input_from_path(
    filepath: &std::path::PathBuf,
) -> Result<Vec<UnicodeInputItem>> {
    let result = if filepath.is_file() {
        load_unicode_input_from_file(filepath)?
    } else {
        let mut result = Vec::new();
        match std::fs::read_dir(filepath) {
            Ok(entries) => {
                for entry in entries {
                    let Ok(entry) = entry else { continue };

                    let path = entry.path();
                    if path.is_dir() {
                        continue;
                    };

                    match load_unicode_input_from_file(&path) {
                        Ok(r) => result.extend(r),
                        Err(e) => {
                            tracing::error!("On read 'unicode input' config from {path:?}: {e}");
                            continue;
                        }
                    }
                }
            }
            Err(e) => tracing::error!("On read dir {filepath:?}: {e}"),
        }
        result
    };

    // sort items from largest to smallest by prefix
    let mut items = result.into_iter().collect::<Vec<_>>();
    items.sort_unstable_by(|(a, _), (b, _)| (a.len(), a).cmp(&(b.len(), b)));
    items.reverse();

    Ok(items
        .into_iter()
        .map(|(prefix, body)| UnicodeInputItem { prefix, body })
        .collect())
}
