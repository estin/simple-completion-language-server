use aho_corasick::AhoCorasick;
use anyhow::Result;
use etcetera::base_strategy::{choose_base_strategy, BaseStrategy};
use ropey::Rope;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::prelude::*;
use tokio::sync::{mpsc, oneshot};
use tower_lsp::lsp_types::*;

pub mod server;
pub mod snippets;

use snippets::Snippet;

pub fn config_dir() -> std::path::PathBuf {
    let strategy = choose_base_strategy().expect("Unable to find the config directory!");
    let mut path = strategy.config_dir();
    path.push("helix");
    path
}

pub struct StartOptions {
    pub external_snippets_config_path: std::path::PathBuf,
    pub snippets_path: std::path::PathBuf,
    pub unicode_input_path: std::path::PathBuf,
}

#[derive(Deserialize)]
pub struct BackendSettings {
    pub max_completion_items: usize,
    pub snippets_first: bool,
    // feature flags
    pub feature_words: bool,
    pub feature_snippets: bool,
    pub feature_unicode_input: bool,
}

impl Default for BackendSettings {
    fn default() -> Self {
        BackendSettings {
            max_completion_items: 20,
            snippets_first: false,
            feature_words: true,
            feature_snippets: true,
            feature_unicode_input: true,
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
    snippets: Vec<Snippet>,
    unicode_input: HashMap<String, String>,
    max_unicude_input_prefix: usize,
    rx: mpsc::UnboundedReceiver<BackendRequest>,
}

impl BackendState {
    pub async fn new(
        snippets: Vec<Snippet>,
        unicode_input: HashMap<String, String>,
    ) -> (mpsc::UnboundedSender<BackendRequest>, Self) {
        let (request_tx, request_rx) = mpsc::unbounded_channel::<BackendRequest>();

        (
            request_tx,
            BackendState {
                settings: BackendSettings::default(),
                docs: HashMap::new(),
                snippets,
                max_unicude_input_prefix: unicode_input
                    .keys()
                    .map(|s| s.len())
                    .max()
                    .unwrap_or_default(),
                unicode_input,
                rx: request_rx,
            },
        )
    }

    fn change_doc(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        if let Some(doc) = self.docs.get_mut(&params.text_document.uri) {
            for change in params.content_changes {
                let Some(range) = change.range else { continue };
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

    fn get_prefix(&self, params: &CompletionParams) -> Result<(Option<&str>, &Document)> {
        let Some(doc) = self
            .docs
            .get(&params.text_document_position.text_document.uri)
        else {
            anyhow::bail!(
                "Document {} not found",
                params.text_document_position.text_document.uri
            )
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

    fn get_prefix_as_chars(
        &self,
        params: &CompletionParams,
        max_chars: usize,
    ) -> Result<(Option<&str>, &Document)> {
        let Some(doc) = self
            .docs
            .get(&params.text_document_position.text_document.uri)
        else {
            anyhow::bail!(
                "Document {} not found",
                params.text_document_position.text_document.uri
            )
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
        let offset = iter
            .enumerate()
            .take_while(|(i, ch)| *i < max_chars && *ch != ' ' && *ch != '\n')
            .count();
        let start_offset = cursor.saturating_sub(offset);
        tracing::debug!("Cursor: {cursor} offset: {offset} start_offset: {start_offset}",);

        if cursor == start_offset {
            return Ok((None, doc));
        }

        let len_chars = doc.text.len_chars();
        if start_offset > len_chars || cursor > len_chars {
            return Ok((None, doc));
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

    fn words(&self, prefix: &str, doc: &Document) -> impl Iterator<Item = CompletionItem> {
        match self.completion(prefix, doc) {
            Ok(words) => words.into_iter(),
            Err(e) => {
                tracing::error!("On complete by words: {e}");
                HashSet::new().into_iter()
            }
        }
        .map(|word| CompletionItem {
            label: word,
            kind: Some(CompletionItemKind::TEXT),
            ..Default::default()
        })
    }

    fn snippets<'a>(
        &'a self,
        prefix: &'a str,
        doc: &'a Document,
    ) -> impl Iterator<Item = CompletionItem> + 'a {
        self.snippets
            .iter()
            .filter(move |s| {
                s.prefix.starts_with(prefix)
                    && if let Some(scope) = &s.scope {
                        scope.is_empty() | scope.contains(&doc.language_id)
                    } else {
                        true
                    }
            })
            .map(move |s| CompletionItem {
                label: s.prefix.to_owned(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some(if let Some(description) = &s.description {
                    format!("{description}\n{}", s.body)
                } else {
                    s.body.to_string()
                }),
                insert_text: Some(s.body.to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            })
            .take(self.settings.max_completion_items)
    }

    fn unicode_input(
        &self,
        word_prefix: &str,
        params: &CompletionParams,
    ) -> impl Iterator<Item = CompletionItem> {
        let Ok((chars, _doc)) = self.get_prefix_as_chars(params, self.max_unicude_input_prefix)
        else {
            tracing::error!("Failed to get prefix as sequence of chars");
            return Vec::new().into_iter();
        };

        let Some(chars) = chars else {
            return Vec::new().into_iter();
        };

        let mut chars_snippets: Vec<CompletionItem> = Vec::new();
        tracing::debug!("Chars: {chars}");
        let l = chars.len();
        for count in 1..l + 1 {
            tracing::trace!("Chars from {} to {}", l - count, l);
            let Some(start) = chars.char_indices().map(|(i, _)| i).nth(l - count) else {
                continue;
            };
            let char_prefix = &chars[start..];

            if char_prefix.contains('\n') {
                continue;
            }
            tracing::trace!("Chars from {} ({start}) to {}: {char_prefix}", l - count, l);
            let items = self
                .unicode_input
                .iter()
                .filter_map(|(prefix, body)| {
                    if !prefix.starts_with(char_prefix) {
                        return None;
                    }
                    let line = params.text_document_position.position.line;
                    let start =
                        params.text_document_position.position.character - char_prefix.len() as u32;
                    let replace_end = params.text_document_position.position.character;
                    let range = Range {
                        start: Position {
                            line,
                            character: start,
                        },
                        end: Position {
                            line,
                            character: replace_end,
                        },
                    };
                    Some(CompletionItem {
                        label: body.to_string(),
                        filter_text: Some(format!("{word_prefix}{prefix}")),
                        kind: Some(CompletionItemKind::TEXT),
                        text_edit: Some(CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
                            replace: range,
                            insert: range,
                            new_text: body.to_string(),
                        })),
                        ..Default::default()
                    })
                })
                .take(self.settings.max_completion_items - chars_snippets.len());
            chars_snippets.extend(items);
            if chars_snippets.len() >= self.settings.max_completion_items {
                break;
            }
        }

        chars_snippets.into_iter()
    }

    pub async fn start(mut self) {
        loop {
            let Some(cmd) = self.rx.recv().await else {
                continue;
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

                    let Ok((prefix, doc)) = self.get_prefix(&params) else {
                        if tx
                            .send(Err(anyhow::anyhow!("Failed to get prefix")))
                            .is_err()
                        {
                            tracing::error!("Error on send completion response");
                        }
                        continue;
                    };

                    let Some(prefix) = prefix else {
                        tracing::debug!("Empty prefix by completion request");
                        let response = Ok(BackendResponse::CompletionResponse(
                            CompletionResponse::Array(Vec::new()),
                        ));
                        if tx.send(response).is_err() {
                            tracing::error!("Error on send completion response");
                        }
                        continue;
                    };

                    let results: Vec<CompletionItem> = match (
                        self.settings.feature_words,
                        self.settings.feature_snippets,
                        self.settings.feature_unicode_input,
                    ) {
                        (true, true, true) => {
                            let words = self.words(prefix, doc);
                            let snippets = self.snippets(prefix, doc);
                            let unicode_input = self.unicode_input(prefix, &params);

                            if self.settings.snippets_first {
                                unicode_input.chain(snippets.chain(words)).collect()
                            } else {
                                words.chain(unicode_input.chain(snippets)).collect()
                            }
                        }
                        (true, true, false) => {
                            let words = self.words(prefix, doc);
                            let snippets = self.snippets(prefix, doc);

                            if self.settings.snippets_first {
                                snippets.chain(words).collect()
                            } else {
                                words.chain(snippets).collect()
                            }
                        }
                        (true, false, true) => {
                            let words = self.words(prefix, doc);
                            let unicode_input = self.unicode_input(prefix, &params);

                            if self.settings.snippets_first {
                                unicode_input.chain(words).collect()
                            } else {
                                words.chain(unicode_input).collect()
                            }
                        }
                        (false, true, true) => {
                            let snippets = self.snippets(prefix, doc);
                            let unicode_input = self.unicode_input(prefix, &params);

                            if self.settings.snippets_first {
                                snippets.chain(unicode_input).collect()
                            } else {
                                unicode_input.chain(snippets).collect()
                            }
                        }
                        (true, false, false) => self.words(prefix, doc).collect(),
                        (false, true, false) => self.snippets(prefix, doc).collect(),
                        (false, false, true) => self.unicode_input(prefix, &params).collect(),
                        (false, false, false) => {
                            tracing::error!("All features disabled by settings...");
                            Vec::new()
                        }
                    };

                    tracing::debug!(
                        "completion request took {:.2}ms with {} result items",
                        now.elapsed().as_millis(),
                        results.len(),
                    );

                    let response =
                        BackendResponse::CompletionResponse(CompletionResponse::Array(results));

                    if tx.send(Ok(response)).is_err() {
                        tracing::error!("Error on send completion response");
                    }
                }
            };
        }
    }
}
