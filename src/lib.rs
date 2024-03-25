use aho_corasick::AhoCorasick;
use anyhow::Result;
use ropey::Rope;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::prelude::*;
use tokio::sync::{mpsc, oneshot};
use tower_lsp::lsp_types::*;

pub mod server;
pub mod snippets;

use snippets::Snippet;

pub struct StartOptions {
    pub home_dir: String,
    pub external_snippets_config_path: std::path::PathBuf,
    pub snippets_path: std::path::PathBuf,
    pub unicode_input_path: std::path::PathBuf,
}

#[derive(Deserialize)]
pub struct BackendSettings {
    pub max_completion_items: usize,
    pub max_path_chars: usize,
    pub snippets_first: bool,
    // feature flags
    pub feature_words: bool,
    pub feature_snippets: bool,
    pub feature_unicode_input: bool,
    pub feature_paths: bool,
}

#[derive(Deserialize)]
pub struct PartialBackendSettings {
    pub max_completion_items: Option<usize>,
    pub max_path_chars: Option<usize>,
    pub snippets_first: Option<bool>,
    pub feature_words: Option<bool>,
    pub feature_snippets: Option<bool>,
    pub feature_unicode_input: Option<bool>,
    pub feature_paths: Option<bool>,
}

impl Default for BackendSettings {
    fn default() -> Self {
        BackendSettings {
            max_completion_items: 20,
            max_path_chars: 256,
            snippets_first: false,
            feature_words: true,
            feature_snippets: true,
            feature_unicode_input: true,
            feature_paths: true,
        }
    }
}

impl BackendSettings {
    pub fn apply_partial_settings(&self, settings: PartialBackendSettings) -> Self {
        Self {
            max_completion_items: settings
                .max_completion_items
                .unwrap_or(self.max_completion_items),
            max_path_chars: settings.max_path_chars.unwrap_or(self.max_path_chars),
            snippets_first: settings.snippets_first.unwrap_or(self.snippets_first),
            feature_words: settings.feature_words.unwrap_or(self.feature_words),
            feature_snippets: settings.feature_snippets.unwrap_or(self.feature_snippets),
            feature_unicode_input: settings
                .feature_unicode_input
                .unwrap_or(self.feature_unicode_input),
            feature_paths: settings.feature_words.unwrap_or(self.feature_paths),
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
    SaveDoc(DidSaveTextDocumentParams),
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
    home_dir: String,
    settings: BackendSettings,
    docs: HashMap<Url, Document>,
    snippets: Vec<Snippet>,
    unicode_input: HashMap<String, String>,
    max_unicude_input_prefix: usize,
    rx: mpsc::UnboundedReceiver<BackendRequest>,
}

impl BackendState {
    pub async fn new(
        home_dir: String,
        snippets: Vec<Snippet>,
        unicode_input: HashMap<String, String>,
    ) -> (mpsc::UnboundedSender<BackendRequest>, Self) {
        let (request_tx, request_rx) = mpsc::unbounded_channel::<BackendRequest>();

        (
            request_tx,
            BackendState {
                home_dir,
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

    fn save_doc(&mut self, params: DidSaveTextDocumentParams) -> Result<()> {
        let Some(doc) = self.docs.get_mut(&params.text_document.uri) else {
            anyhow::bail!("Document {} not found", params.text_document.uri)
        };
        doc.text = if let Some(text) = &params.text {
            Rope::from_str(text)
        } else {
            // Sync read content from file
            let file = std::fs::File::open(params.text_document.uri.path())?;
            Rope::from_reader(file)?
        };
        Ok(())
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
                    .map(|idx| idx + range.end.character as usize)
                    .and_then(|c| {
                        if c > doc.text.len_chars() {
                            Err(ropey::Error::CharIndexOutOfBounds(c, doc.text.len_chars()))
                        } else {
                            Ok(c)
                        }
                    });

                match (start_idx, end_idx) {
                    (Ok(start_idx), Err(_)) => {
                        doc.text.try_remove(start_idx..)?;
                        doc.text.try_insert(start_idx, &change.text)?;
                    }
                    (Ok(start_idx), Ok(end_idx)) => {
                        doc.text.try_remove(start_idx..end_idx)?;
                        doc.text.try_insert(start_idx, &change.text)?;
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
        self.settings = self
            .settings
            .apply_partial_settings(serde_json::from_value(params.settings)?);
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

        let l = chars.len();
        for count in 1..l + 1 {
            let Some(start) = chars.char_indices().map(|(i, _)| i).nth(l - count) else {
                continue;
            };
            let char_prefix = &chars[start..];

            if char_prefix.contains('\n') {
                continue;
            }
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

    fn paths(
        &self,
        word_prefix: &str,
        params: &CompletionParams,
    ) -> impl Iterator<Item = CompletionItem> {
        let Ok((chars, _)) = self.get_prefix_as_chars(params, self.settings.max_path_chars) else {
            tracing::error!("Failed to get prefix as sequence of chars");
            return Vec::new().into_iter();
        };

        let Some(chars) = chars else {
            return Vec::new().into_iter();
        };

        // check is it path
        if !chars.contains(std::path::MAIN_SEPARATOR) {
            return Vec::new().into_iter();
        }

        let Some(first_char) = chars.chars().nth(0) else {
            return Vec::new().into_iter();
        };
        let Some(last_char) = chars.chars().last() else {
            return Vec::new().into_iter();
        };

        // sanitize surround chars
        let chars_prefix = if first_char.is_alphabetic()
            || first_char == std::path::MAIN_SEPARATOR
            || first_char == '~'
        {
            chars
        } else {
            &chars[1..]
        };

        let chars_prefix_len = chars_prefix.len() as u32;

        // expand tilde to home dir
        let (is_tilde_exapnded, chars_prefix) = if chars_prefix.starts_with("~/") {
            (
                true,
                Cow::Owned(chars_prefix.replacen('~', &self.home_dir, 1)),
            )
        } else {
            (false, Cow::Borrowed(chars_prefix))
        };

        // build path
        let path = std::path::Path::new(chars_prefix.as_ref());

        // normalize filename
        let (filename, parent_dir) = if last_char == std::path::MAIN_SEPARATOR {
            (String::new(), path)
        } else {
            let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
                return Vec::new().into_iter();
            };
            let Some(parent_dir) = path.parent() else {
                return Vec::new().into_iter();
            };
            (filename.to_lowercase(), parent_dir)
        };

        let items = match parent_dir.read_dir() {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!("On read dir {parent_dir:?}: {e}");
                return Vec::new().into_iter();
            }
        };

        items
            .into_iter()
            .filter_map(|item| item.ok())
            .filter_map(|item| {
                // convert to regular &str
                let fname = item.file_name();
                let Some(item_filename) = fname.to_str() else {
                    return None;
                };
                let item_filename = item_filename.to_lowercase();
                if !filename.is_empty() && !item_filename.starts_with(&filename) {
                    return None;
                }

                // use fullpath
                let path = item.path();
                let Some(full_path) = path.to_str() else {
                    return None;
                };

                // fold back to tilde
                let full_path = if is_tilde_exapnded {
                    Cow::Owned(full_path.replacen(&self.home_dir, "~", 1))
                } else {
                    Cow::Borrowed(full_path)
                };

                let line = params.text_document_position.position.line;
                let start = params.text_document_position.position.character - chars_prefix_len;
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
                    label: full_path.to_string(),
                    filter_text: Some(format!("{word_prefix}{full_path}")),
                    kind: Some(if path.is_dir() {
                        CompletionItemKind::FOLDER
                    } else {
                        CompletionItemKind::FILE
                    }),
                    text_edit: Some(CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
                        replace: range,
                        insert: range,
                        new_text: full_path.to_string(),
                    })),
                    ..Default::default()
                })
            })
            .take(self.settings.max_completion_items)
            .collect::<Vec<_>>()
            .into_iter()
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
                BackendRequest::SaveDoc(params) => {
                    if let Err(e) = self.save_doc(params) {
                        tracing::error!("Error on save doc: {e}");
                    }
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

                    let results: Vec<CompletionItem> = Vec::new()
                        .into_iter()
                        .chain(
                            if let Some(prefix) = &prefix {
                                if self.settings.feature_snippets & self.settings.snippets_first {
                                    Some(self.snippets(prefix, doc))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                            .into_iter()
                            .flatten(),
                        )
                        .chain(
                            if let Some(prefix) = &prefix {
                                if self.settings.feature_words {
                                    Some(self.words(prefix, doc))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                            .into_iter()
                            .flatten(),
                        )
                        .chain(
                            if let Some(prefix) = &prefix {
                                if self.settings.feature_snippets & !self.settings.snippets_first {
                                    Some(self.snippets(prefix, doc))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                            .into_iter()
                            .flatten(),
                        )
                        .chain(
                            if self.settings.feature_unicode_input {
                                Some(self.unicode_input(prefix.unwrap_or_default(), &params))
                            } else {
                                None
                            }
                            .into_iter()
                            .flatten(),
                        )
                        .chain(
                            if self.settings.feature_paths {
                                Some(self.paths(prefix.unwrap_or_default(), &params))
                            } else {
                                None
                            }
                            .into_iter()
                            .flatten(),
                        )
                        .collect();

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
