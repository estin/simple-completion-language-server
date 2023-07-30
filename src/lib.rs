use aho_corasick::AhoCorasick;
use anyhow::Result;
use etcetera::base_strategy::{choose_base_strategy, BaseStrategy};
use ropey::Rope;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::prelude::*;
use tokio::sync::{mpsc, oneshot};
use tower_lsp::lsp_types::*;

pub mod snippets;

use snippets::{Snippet, SnippetsConfig};

pub fn config_dir() -> std::path::PathBuf {
    let strategy = choose_base_strategy().expect("Unable to find the config directory!");
    let mut path = strategy.config_dir();
    path.push("helix");
    path
}

#[derive(Deserialize)]
pub struct BackendSettings {
    pub max_completion_items: usize,
}

impl Default for BackendSettings {
    fn default() -> Self {
        BackendSettings {
            max_completion_items: 20,
        }
    }
}

#[inline]
pub fn char_is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

pub struct RopeReader<'a> {
    chunks: ropey::iter::Chunks<'a>,
}

impl<'a> RopeReader<'a> {
    pub fn new(rope: &'a ropey::Rope) -> Self {
        RopeReader {
            chunks: rope.chunks(),
        }
    }
}

impl<'a> std::io::Read for RopeReader<'a> {
    fn read(&mut self, mut buf: &mut [u8]) -> std::io::Result<usize> {
        match self.chunks.next() {
            Some(chunk) => buf.write(chunk.as_bytes()),
            _ => Ok(0),
        }
    }
}

#[derive(Debug)]
pub enum BackendRequest {
    NewDoc(DidOpenTextDocumentParams),
    ChangeDoc(DidChangeTextDocumentParams),
    ChangeConfiguration(DidChangeConfigurationParams),
    CompletionRequest(
        (
            oneshot::Sender<anyhow::Result<BackendResponse>>,
            CompletionParams,
        ),
    ),
}

#[derive(Debug)]
pub enum BackendResponse {
    CompletionResponse(CompletionResponse),
}

pub struct Document {
    uri: Url,
    text: Rope,
    language_id: String,
}

pub struct BackendState {
    settings: BackendSettings,
    docs: HashMap<Url, Document>,
    snippets: HashMap<String, Snippet>,
    rx: mpsc::UnboundedReceiver<BackendRequest>,
}

impl BackendState {
    pub async fn new(
        snippets_file: &std::path::PathBuf,
    ) -> (mpsc::UnboundedSender<BackendRequest>, Self) {
        tracing::info!("Try load snippets from: {snippets_file:?}");
        let snippets = match std::fs::read_to_string(snippets_file) {
            Ok(content) => match toml::from_str::<SnippetsConfig>(&content) {
                Ok(sc) => {
                    HashMap::from_iter(sc.snippets.into_iter().map(|s| (s.prefix.clone(), s)))
                }
                Err(e) => {
                    tracing::error!("Failed to parse {snippets_file:?}: {e}");
                    HashMap::new()
                }
            },

            Err(e) => {
                tracing::warn!("Failed to read snippets: {e}");
                HashMap::new()
            }
        };

        let (request_tx, request_rx) = mpsc::unbounded_channel::<BackendRequest>();

        (
            request_tx,
            BackendState {
                settings: BackendSettings::default(),
                docs: HashMap::new(),
                snippets,
                rx: request_rx,
            },
        )
    }

    fn change_doc(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        if let Some(doc) = self.docs.get_mut(&params.text_document.uri) {
            for change in params.content_changes {
                let Some(range) = change.range else {
                    continue
                };
                let start_idx = doc
                    .text
                    .try_line_to_char(range.start.line as usize)
                    .map(|idx| idx + range.start.character as usize);
                let end_idx = doc
                    .text
                    .try_line_to_char(range.end.line as usize)
                    .map(|idx| idx + range.end.character as usize);

                match (start_idx, end_idx) {
                    (Ok(start_idx), Err(_)) => {
                        doc.text.remove(start_idx..);
                        doc.text.insert(start_idx, &change.text);
                    }
                    (Ok(start_idx), Ok(end_idx)) => {
                        doc.text.remove(start_idx..end_idx);
                        doc.text.insert(start_idx, &change.text);
                    }
                    (Err(_), _) => {
                        *doc = Document {
                            uri: doc.uri.clone(),
                            text: Rope::from(change.text),
                            language_id: doc.language_id.clone(),
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn change_configuration(&mut self, params: DidChangeConfigurationParams) -> Result<()> {
        self.settings = serde_json::from_value(params.settings)?;
        Ok(())
    }

    fn get_prefix(&self, params: CompletionParams) -> Result<(Option<&str>, &Document)> {
        let Some(doc) = self
            .docs
            .get(&params.text_document_position.text_document.uri)
        else {
            anyhow::bail!("Document {} not found", params.text_document_position.text_document.uri)
        };

        // word prefix
        let cursor = doc
            .text
            .try_line_to_char(params.text_document_position.position.line as usize)?
            + params.text_document_position.position.character as usize;
        let mut iter = doc
            .text
            .get_chars_at(cursor)
            .ok_or_else(|| anyhow::anyhow!("bounds error"))?;
        iter.reverse();
        let offset = iter.take_while(|ch| char_is_word(*ch)).count();
        let start_offset = cursor.saturating_sub(offset);

        if cursor == start_offset {
            return Ok((None, doc));
        }

        let len_chars = doc.text.len_chars();
        if start_offset > len_chars || cursor > len_chars {
            anyhow::bail!("bounds error")
        }

        let prefix = doc.text.slice(start_offset..cursor).as_str();
        Ok((prefix, doc))
    }

    fn search(
        &self,
        ac: &AhoCorasick,
        prefix: &str,
        doc: &Document,
        to_take: usize,
    ) -> Result<HashSet<String>> {
        let mut result: HashSet<String> = HashSet::new();
        let len_bytes = doc.text.len_bytes();

        let searcher = ac.try_stream_find_iter(RopeReader::new(&doc.text))?;

        for mat in searcher.take(to_take) {
            let mat = mat?;
            let mat_end = doc.text.byte_to_char(mat.end());

            let word_end = doc
                .text
                .chars()
                .skip(mat_end)
                .take_while(|ch| char_is_word(*ch))
                .count();

            let word_end = doc.text.char_to_byte(mat_end + word_end);

            if word_end > len_bytes {
                continue;
            }

            let item = doc.text.byte_slice(mat.start()..word_end);
            if item != prefix {
                result.insert(item.to_string());
                if result.len() >= self.settings.max_completion_items {
                    return Ok(result);
                }
            }
        }

        Ok(result)
    }

    fn completion(&self, prefix: &str, current_doc: &Document) -> Result<HashSet<String>> {
        // prepare search pattern
        let ac = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build([&prefix])
            .map_err(|e| anyhow::anyhow!("error {e}"))?;

        // search in current doc at first
        let mut result =
            self.search(&ac, prefix, current_doc, self.settings.max_completion_items)?;
        if result.len() >= self.settings.max_completion_items {
            return Ok(result);
        }

        for doc in self.docs.values().filter(|doc| doc.uri != current_doc.uri) {
            result.extend(self.search(
                &ac,
                prefix,
                doc,
                self.settings.max_completion_items - result.len(),
            )?);
            if result.len() >= self.settings.max_completion_items {
                return Ok(result);
            }
        }

        Ok(result)
    }

    pub async fn start(mut self) {
        loop {
            let Some(cmd) = self.rx.recv().await else {
                continue
            };

            match cmd {
                BackendRequest::NewDoc(params) => {
                    self.docs.insert(
                        params.text_document.uri.clone(),
                        Document {
                            uri: params.text_document.uri,
                            text: Rope::from_str(&params.text_document.text),
                            language_id: params.text_document.language_id,
                        },
                    );
                }
                BackendRequest::ChangeDoc(params) => {
                    if let Err(e) = self.change_doc(params) {
                        tracing::error!("Error on change doc: {e}");
                    }
                }
                BackendRequest::ChangeConfiguration(params) => {
                    if let Err(e) = self.change_configuration(params) {
                        tracing::error!("Error on change configuration: {e}");
                    }
                }
                BackendRequest::CompletionRequest((tx, params)) => {
                    let now = std::time::Instant::now();

                    let Ok((prefix, doc)) = self.get_prefix(params) else {
                        if tx.send(Err(anyhow::anyhow!("Failed to get prefix"))).is_err() {
                            tracing::error!("Error on send completion response");
                        }
                        continue
                    };

                    let Some(prefix) = prefix else {
                        let response = Ok(BackendResponse::CompletionResponse(CompletionResponse::Array(Vec::new())));
                        if tx.send(response).is_err() {
                            tracing::error!("Error on send completion response");
                        }
                        continue
                    };

                    // words
                    let words = match self.completion(prefix, doc) {
                        Ok(words) => {
                            tracing::info!(
                                "completion request took {:.2}ms with {} result items",
                                now.elapsed().as_millis(),
                                words.len(),
                            );

                            words.into_iter().map(|word| CompletionItem {
                                label: word,
                                kind: Some(CompletionItemKind::TEXT),
                                ..Default::default()
                            })
                        }

                        Err(e) => {
                            tracing::error!("On completion request: {e}");

                            if tx.send(Err(e)).is_err() {
                                tracing::error!("Error on send completion response");
                            }
                            continue;
                        }
                    };

                    // snippets
                    let snippets = self
                        .snippets
                        .iter()
                        .filter(|(p, s)| {
                            p.starts_with(prefix)
                                && if let Some(scope) = &s.scope {
                                    scope.is_empty() | scope.contains(&doc.language_id)
                                } else {
                                    true
                                }
                        })
                        .map(|(p, s)| CompletionItem {
                            label: p.to_owned(),
                            kind: Some(CompletionItemKind::SNIPPET),
                            detail: Some(if let Some(description) = &s.description {
                                format!("{description}\n{}", s.body)
                            } else {
                                s.body.to_string()
                            }),
                            insert_text: Some(s.body.to_string()),
                            insert_text_format: Some(InsertTextFormat::SNIPPET),
                            ..Default::default()
                        });

                    let response = BackendResponse::CompletionResponse(CompletionResponse::Array(
                        words.chain(snippets).collect(),
                    ));

                    if tx.send(Ok(response)).is_err() {
                        tracing::error!("Error on send completion response");
                    }
                }
            };
        }
    }
}
