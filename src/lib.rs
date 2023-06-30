use aho_corasick::AhoCorasick;
use anyhow::Result;
use ropey::Rope;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::prelude::*;
use tokio::sync::{mpsc, oneshot};
use tower_lsp::lsp_types::*;

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

pub struct BackendState {
    settings: BackendSettings,
    docs: HashMap<Url, Rope>,
    rx: mpsc::UnboundedReceiver<BackendRequest>,
}

impl BackendState {
    pub async fn new() -> (mpsc::UnboundedSender<BackendRequest>, Self) {
        let (request_tx, request_rx) = mpsc::unbounded_channel::<BackendRequest>();

        (
            request_tx,
            BackendState {
                settings: BackendSettings::default(),
                docs: HashMap::new(),
                rx: request_rx,
            },
        )
    }

    fn change_doc(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        if let Some(text) = self.docs.get_mut(&params.text_document.uri) {
            for change in params.content_changes {
                let Some(range) = change.range else {
                    continue
                };
                let start_idx = text
                    .try_line_to_char(range.start.line as usize)
                    .map(|idx| idx + range.start.character as usize);
                let end_idx = text
                    .try_line_to_char(range.end.line as usize)
                    .map(|idx| idx + range.end.character as usize);

                match (start_idx, end_idx) {
                    (Ok(start_idx), Err(_)) => {
                        text.remove(start_idx..);
                        text.insert(start_idx, &change.text);
                    }
                    (Ok(start_idx), Ok(end_idx)) => {
                        text.remove(start_idx..end_idx);
                        text.insert(start_idx, &change.text);
                    }
                    (Err(_), _) => {
                        *text = Rope::from(change.text);
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

    fn completion(&self, params: CompletionParams) -> Result<HashSet<String>> {
        let mut result: HashSet<String> = HashSet::new();
        if let Some(text) = self
            .docs
            .get(&params.text_document_position.text_document.uri)
        {
            // word prefix
            let cursor = text
                .try_line_to_char(params.text_document_position.position.line as usize)?
                + params.text_document_position.position.character as usize;
            let mut iter = text
                .get_chars_at(cursor)
                .ok_or_else(|| anyhow::anyhow!("bounds error"))?;
            iter.reverse();
            let offset = iter.take_while(|ch| char_is_word(*ch)).count();
            let start_offset = cursor.saturating_sub(offset);

            if cursor == start_offset {
                return Ok(HashSet::new());
            }

            let len_chars = text.len_chars();
            if start_offset > len_chars || cursor > len_chars {
                anyhow::bail!("bounds error")
            }

            let prefix = text.slice(start_offset..cursor).to_string();

            // prepare search pattern
            let ac = AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([&prefix])
                .map_err(|e| anyhow::anyhow!("error {e}"))?;

            for (url, text) in &self.docs {
                let len_chars = text.len_chars();
                tracing::debug!(
                    "Try complete prefix {} by doc {} (chars: {len_chars})",
                    prefix,
                    url.as_str()
                );

                let searcher = ac.try_stream_find_iter(RopeReader::new(text))?;

                for mat in searcher.take(self.settings.max_completion_items) {
                    let mat = mat?;
                    let word_end = text
                        .chars()
                        .skip(mat.end())
                        .take_while(|ch| char_is_word(*ch))
                        .count();

                    if mat.end() + word_end > len_chars {
                        continue;
                    }

                    let item = text.slice(mat.start()..(mat.end() + word_end));
                    if item != prefix {
                        result.insert(item.to_string());
                        if result.len() >= self.settings.max_completion_items {
                            return Ok(result);
                        }
                    }
                }
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
                        params.text_document.uri,
                        Rope::from_str(&params.text_document.text),
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

                    let response = self.completion(params).map(|result| {
                        tracing::info!(
                            "completion request took {:.2}ms with {} result items",
                            now.elapsed().as_millis(),
                            result.len(),
                        );
                        BackendResponse::CompletionResponse(CompletionResponse::Array(
                            result
                                .into_iter()
                                .map(|word| CompletionItem {
                                    label: word,
                                    kind: Some(CompletionItemKind::TEXT),
                                    ..Default::default()
                                })
                                .collect(),
                        ))
                    });

                    if tx.send(response).is_err() {
                        tracing::error!("Error on send completion response");
                    }
                }
            };
        }
    }
}
