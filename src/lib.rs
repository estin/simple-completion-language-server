use aho_corasick::AhoCorasick;
use anyhow::Result;
use ropey::Rope;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::prelude::*;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};
use tower_lsp::lsp_types::*;

#[cfg(feature = "citation")]
use biblatex::Type;
#[cfg(feature = "citation")]
use regex_cursor::{engines::meta::Regex, Input, RopeyCursor};

pub mod server;
pub mod snippets;

use snippets::{Snippet, UnicodeInputItem};

pub struct StartOptions {
    pub home_dir: String,
    pub external_snippets_config_path: std::path::PathBuf,
    pub snippets_path: std::path::PathBuf,
    pub unicode_input_path: std::path::PathBuf,
}

#[derive(Deserialize)]
pub struct BackendSettings {
    pub max_completion_items: usize,
    pub max_chars_prefix_len: usize,
    pub min_chars_prefix_len: usize,
    pub snippets_first: bool,
    pub snippets_inline_by_word_tail: bool,
    // citation
    pub citation_prefix_trigger: String,
    pub citation_bibfile_extract_regexp: String,
    // feature flags
    pub feature_words: bool,
    pub feature_snippets: bool,
    pub feature_unicode_input: bool,
    pub feature_paths: bool,
    pub feature_citations: bool,
}

#[derive(Deserialize)]
pub struct PartialBackendSettings {
    pub max_completion_items: Option<usize>,
    pub max_chars_prefix_len: Option<usize>,
    pub min_chars_prefix_len: Option<usize>,
    pub max_path_chars: Option<usize>,
    pub snippets_first: Option<bool>,
    pub snippets_inline_by_word_tail: Option<bool>,
    // citation
    pub citation_prefix_trigger: Option<String>,
    pub citation_bibfile_extract_regexp: Option<String>,
    // feature flags
    pub feature_words: Option<bool>,
    pub feature_snippets: Option<bool>,
    pub feature_unicode_input: Option<bool>,
    pub feature_paths: Option<bool>,
    pub feature_citations: Option<bool>,

    #[serde(flatten)]
    pub extra: Option<serde_json::Value>,
}

impl Default for BackendSettings {
    fn default() -> Self {
        BackendSettings {
            min_chars_prefix_len: 2,
            max_completion_items: 100,
            max_chars_prefix_len: 64,
            snippets_first: false,
            snippets_inline_by_word_tail: false,
            citation_prefix_trigger: "@".to_string(),
            citation_bibfile_extract_regexp: r#"bibliography:\s*['"\[]*([~\w\./\\-]*)['"\]]*"#
                .to_string(),
            feature_words: true,
            feature_snippets: true,
            feature_unicode_input: false,
            feature_paths: false,
            feature_citations: false,
        }
    }
}

impl BackendSettings {
    pub fn apply_partial_settings(&self, settings: PartialBackendSettings) -> Self {
        Self {
            max_completion_items: settings
                .max_completion_items
                .unwrap_or(self.max_completion_items),
            max_chars_prefix_len: settings.max_path_chars.unwrap_or(self.max_chars_prefix_len),
            min_chars_prefix_len: settings
                .min_chars_prefix_len
                .unwrap_or(self.min_chars_prefix_len),
            snippets_first: settings.snippets_first.unwrap_or(self.snippets_first),
            snippets_inline_by_word_tail: settings
                .snippets_inline_by_word_tail
                .unwrap_or(self.snippets_inline_by_word_tail),
            citation_prefix_trigger: settings
                .citation_prefix_trigger
                .clone()
                .unwrap_or_else(|| self.citation_prefix_trigger.to_owned()),
            citation_bibfile_extract_regexp: settings
                .citation_prefix_trigger
                .clone()
                .unwrap_or_else(|| self.citation_bibfile_extract_regexp.to_owned()),
            feature_words: settings.feature_words.unwrap_or(self.feature_words),
            feature_snippets: settings.feature_snippets.unwrap_or(self.feature_snippets),
            feature_unicode_input: settings
                .feature_unicode_input
                .unwrap_or(self.feature_unicode_input),
            feature_paths: settings.feature_paths.unwrap_or(self.feature_paths),
            feature_citations: settings.feature_citations.unwrap_or(self.feature_citations),
        }
    }
}

#[inline]
pub fn char_is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '-'
}

#[inline]
pub fn char_is_char_prefix(ch: char) -> bool {
    ch != ' ' && ch != '\n' && ch != '\t'
}

#[inline]
pub fn starts_with(source: &str, s: &str) -> bool {
    if s.len() > source.len() {
        return false;
    }
    let Some(part) = source.get(..s.len()) else {
        return false;
    };
    caseless::default_caseless_match_str(part, s)
}

enum PathLogic {
    Full,
    Tilde,
    RelativeCurrent,
    RelativeParent,
}

struct PathState<'a> {
    logic: PathLogic,
    home_dir: &'a str,
    current_dir: PathBuf,
    parent_dir: Option<PathBuf>,
}

impl From<&str> for PathLogic {
    fn from(s: &str) -> Self {
        if s.starts_with("~/") {
            PathLogic::Tilde
        } else if s.starts_with("./") {
            PathLogic::RelativeCurrent
        } else if s.starts_with("../") {
            PathLogic::RelativeParent
        } else {
            PathLogic::Full
        }
    }
}

impl<'a> PathState<'a> {
    fn new(s: &'a str, home_dir: &'a str, document_path: &'a str) -> Self {
        let logic = PathLogic::from(s);
        let current_dir = PathBuf::from(document_path);
        let current_dir = current_dir
            .parent()
            .map(PathBuf::from)
            .unwrap_or(current_dir);
        Self {
            home_dir,
            parent_dir: if matches!(logic, PathLogic::RelativeParent) {
                current_dir.parent().map(PathBuf::from)
            } else {
                None
            },
            logic,
            current_dir,
        }
    }

    fn expand(&'a self, s: &'a str) -> Cow<'a, str> {
        match self.logic {
            PathLogic::Full => Cow::Borrowed(s),
            PathLogic::Tilde => Cow::Owned(s.replacen('~', self.home_dir, 1)),
            PathLogic::RelativeCurrent => {
                if let Some(dir) = self.current_dir.to_str() {
                    tracing::warn!("Can't represent current_dir {:?} as str", self.current_dir);
                    Cow::Owned(s.replacen(".", dir, 1))
                } else {
                    Cow::Borrowed(s)
                }
            }
            PathLogic::RelativeParent => {
                if let Some(dir) = self.parent_dir.as_ref().and_then(|p| p.to_str()) {
                    tracing::warn!("Can't represent current_dir {:?} as str", self.current_dir);
                    Cow::Owned(s.replacen("..", dir, 1))
                } else {
                    Cow::Borrowed(s)
                }
            }
        }
    }
    fn fold(&'a self, s: &'a str) -> Cow<'a, str> {
        match self.logic {
            PathLogic::Full => Cow::Borrowed(s),
            PathLogic::Tilde => Cow::Owned(s.replacen(self.home_dir, "~", 1)),
            PathLogic::RelativeCurrent => {
                if let Some(dir) = self.current_dir.to_str() {
                    tracing::warn!("Can't represent current_dir {:?} as str", self.current_dir);
                    Cow::Owned(s.replacen(dir, ".", 1))
                } else {
                    Cow::Borrowed(s)
                }
            }

            PathLogic::RelativeParent => {
                if let Some(dir) = self.parent_dir.as_ref().and_then(|p| p.to_str()) {
                    tracing::warn!("Can't represent current_dir {:?} as str", self.current_dir);
                    Cow::Owned(s.replacen(dir, "..", 1))
                } else {
                    Cow::Borrowed(s)
                }
            }
        }
    }
}

impl PathLogic {}

pub struct RopeReader<'a> {
    tail: Vec<u8>,
    chunks: ropey::iter::Chunks<'a>,
}

impl<'a> RopeReader<'a> {
    pub fn new(rope: &'a ropey::Rope) -> Self {
        RopeReader {
            tail: Vec::new(),
            chunks: rope.chunks(),
        }
    }
}

impl std::io::Read for RopeReader<'_> {
    fn read(&mut self, mut buf: &mut [u8]) -> std::io::Result<usize> {
        match self.chunks.next() {
            Some(chunk) => {
                let tail_len = self.tail.len();

                // write previous tail
                if tail_len > 0 {
                    let tail = self.tail.drain(..);
                    Write::write_all(&mut buf, tail.as_slice())?;
                }

                // find last ending word
                let data = if let Some((byte_pos, _)) = chunk
                    .char_indices()
                    .rev()
                    .find(|(_, ch)| !char_is_word(*ch))
                {
                    if byte_pos != 0 {
                        Write::write_all(&mut self.tail, chunk[byte_pos..].as_bytes())?;
                        &chunk[0..byte_pos]
                    } else {
                        chunk
                    }
                } else {
                    chunk
                };
                Write::write_all(&mut buf, data.as_bytes())?;
                Ok(tail_len + data.len())
            }
            _ => {
                let tail_len = self.tail.len();

                if tail_len == 0 {
                    return Ok(0);
                }

                // write previous tail
                let tail = self.tail.drain(..);
                Write::write_all(&mut buf, tail.as_slice())?;
                Ok(tail_len)
            }
        }
    }
}

pub fn ac_searcher(prefix: &str) -> Result<AhoCorasick> {
    AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build([&prefix])
        .map_err(|e| anyhow::anyhow!("error {e}"))
}

pub fn search(
    prefix: &str,
    text: &Rope,
    ac: &AhoCorasick,
    max_completion_items: usize,
    result: &mut HashSet<String>,
) -> Result<()> {
    let searcher = ac.try_stream_find_iter(RopeReader::new(text))?;

    for mat in searcher {
        let mat = mat?;

        let Ok(start_char_idx) = text.try_byte_to_char(mat.start()) else {
            continue;
        };
        let Ok(mat_end) = text.try_byte_to_char(mat.end()) else {
            continue;
        };

        // check is word start
        if mat.start() > 0 {
            let Ok(s) = text.try_byte_to_char(mat.start() - 1) else {
                continue;
            };
            let Some(ch) = text.get_char(s) else {
                continue;
            };
            if char_is_word(ch) {
                continue;
            }
        }

        // search word end
        let word_end = text
            .chars()
            .skip(mat_end)
            .take_while(|ch| char_is_word(*ch))
            .count();

        let Ok(word_end) = text.try_char_to_byte(mat_end + word_end) else {
            continue;
        };
        let Ok(end_char_idx) = text.try_byte_to_char(word_end) else {
            continue;
        };

        let item = text.slice(start_char_idx..end_char_idx);
        if let Some(item) = item.as_str() {
            if item != prefix && starts_with(item, prefix) {
                result.insert(item.to_string());
                if result.len() >= max_completion_items {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
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
    unicode_input: Vec<UnicodeInputItem>,
    max_unicode_input_prefix_len: usize,
    max_snippet_input_prefix_len: usize,
    rx: mpsc::UnboundedReceiver<BackendRequest>,

    #[cfg(feature = "citation")]
    citation_bibliography_re: Option<Regex>,
}

impl BackendState {
    pub async fn new(
        home_dir: String,
        snippets: Vec<Snippet>,
        unicode_input: Vec<UnicodeInputItem>,
    ) -> (mpsc::UnboundedSender<BackendRequest>, Self) {
        let (request_tx, request_rx) = mpsc::unbounded_channel::<BackendRequest>();

        let settings = BackendSettings::default();
        (
            request_tx,
            BackendState {
                home_dir,
                #[cfg(feature = "citation")]
                citation_bibliography_re: Regex::new(&settings.citation_bibfile_extract_regexp)
                    .map_err(|e| {
                        tracing::error!("Invalid citation bibliography regex: {e}");
                        e
                    })
                    .ok(),

                settings,
                docs: HashMap::new(),
                max_unicode_input_prefix_len: unicode_input
                    .iter()
                    .map(|s| s.prefix.len())
                    .max()
                    .unwrap_or_default(),
                max_snippet_input_prefix_len: snippets
                    .iter()
                    .map(|s| s.prefix.len())
                    .max()
                    .unwrap_or_default(),
                snippets,
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
        let Some(doc) = self.docs.get_mut(&params.text_document.uri) else {
            tracing::error!("Doc {} not found", params.text_document.uri);
            return Ok(());
        };
        for change in params.clone().content_changes {
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
        Ok(())
    }

    fn change_configuration(&mut self, params: DidChangeConfigurationParams) -> Result<()> {
        self.settings = self
            .settings
            .apply_partial_settings(serde_json::from_value(params.settings)?);

        #[cfg(feature = "citation")]
        {
            self.citation_bibliography_re =
                Some(Regex::new(&self.settings.citation_bibfile_extract_regexp)?);
        };

        Ok(())
    }

    fn get_prefix(
        &self,
        max_chars: usize,
        params: &CompletionParams,
    ) -> Result<(Option<&str>, Option<&str>, &Document)> {
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
        let start_offset_word = cursor.saturating_sub(offset);

        let len_chars = doc.text.len_chars();

        if start_offset_word > len_chars || cursor > len_chars {
            anyhow::bail!("bounds error")
        }

        let mut iter = doc
            .text
            .get_chars_at(cursor)
            .ok_or_else(|| anyhow::anyhow!("bounds error"))?;
        iter.reverse();
        let offset = iter
            .enumerate()
            .take_while(|(i, ch)| *i < max_chars && char_is_char_prefix(*ch))
            .count();
        let start_offset_chars = cursor.saturating_sub(offset);

        if start_offset_chars > len_chars || cursor > len_chars {
            anyhow::bail!("bounds error")
        }

        let prefix = doc.text.slice(start_offset_word..cursor).as_str();
        let chars_prefix = doc.text.slice(start_offset_chars..cursor).as_str();
        Ok((prefix, chars_prefix, doc))
    }

    fn completion(&self, prefix: &str, current_doc: &Document) -> Result<HashSet<String>> {
        // prepare search pattern
        let ac = ac_searcher(prefix)?;
        let mut result = HashSet::with_capacity(self.settings.max_completion_items);

        // search in current doc at first
        search(
            prefix,
            &current_doc.text,
            &ac,
            self.settings.max_completion_items,
            &mut result,
        )?;
        if result.len() >= self.settings.max_completion_items {
            return Ok(result);
        }

        for doc in self.docs.values().filter(|doc| doc.uri != current_doc.uri) {
            search(
                prefix,
                &doc.text,
                &ac,
                self.settings.max_completion_items,
                &mut result,
            )?;
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
        filter_text_prefix: &'a str,
        exact_match: bool,
        doc: &'a Document,
        params: &'a CompletionParams,
    ) -> impl Iterator<Item = CompletionItem> + 'a {
        self.snippets
            .iter()
            .filter(move |s| {
                let filter_by_scope = if let Some(scope) = &s.scope {
                    scope.is_empty() | scope.contains(&doc.language_id)
                } else {
                    true
                };
                if !filter_by_scope {
                    return false;
                }
                if exact_match {
                    caseless::default_caseless_match_str(s.prefix.as_str(), prefix)
                } else {
                    starts_with(s.prefix.as_str(), prefix)
                }
            })
            .map(move |s| {
                let line = params.text_document_position.position.line;
                let start = params.text_document_position.position.character - prefix.len() as u32;
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
                CompletionItem {
                    label: s.prefix.to_owned(),
                    sort_text: Some(s.prefix.to_string()),
                    filter_text: Some(if filter_text_prefix.is_empty() {
                        s.prefix.to_string()
                    } else {
                        filter_text_prefix.to_string()
                    }),
                    kind: Some(CompletionItemKind::SNIPPET),
                    detail: Some(s.body.to_string()),
                    documentation: Some(if let Some(description) = &s.description {
                        Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: format!(
                                "{description}\n```{}\n{}\n```",
                                doc.language_id, s.body
                            ),
                        })
                    } else {
                        Documentation::String(s.body.to_string())
                    }),
                    text_edit: Some(CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
                        replace: range,
                        insert: range,
                        new_text: s.body.to_string(),
                    })),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                }
            })
            .take(self.settings.max_completion_items)
    }

    fn snippets_by_word_tail<'a>(
        &'a self,
        chars_prefix: &'a str,
        doc: &'a Document,
        params: &'a CompletionParams,
    ) -> impl Iterator<Item = CompletionItem> + 'a {
        let mut chars_snippets: Vec<CompletionItem> = Vec::new();

        for index in 0..=chars_prefix.len() {
            let Some(part) = chars_prefix.get(index..) else {
                continue;
            };
            if part.is_empty() {
                break;
            }
            // try to find tail for prefix to start completion
            if part.len() > self.max_snippet_input_prefix_len {
                continue;
            }
            if part.len() < self.settings.min_chars_prefix_len {
                break;
            }
            chars_snippets.extend(self.snippets(part, chars_prefix, false, doc, params));
            if chars_snippets.len() >= self.settings.max_completion_items {
                break;
            }
        }

        chars_snippets.into_iter()
    }

    fn unicode_input(
        &self,
        word_prefix: &str,
        chars_prefix: &str,
        params: &CompletionParams,
    ) -> impl Iterator<Item = CompletionItem> {
        let mut chars_snippets: Vec<CompletionItem> = Vec::new();

        for index in 0..=chars_prefix.len() {
            let Some(part) = chars_prefix.get(index..) else {
                continue;
            };
            if part.is_empty() {
                break;
            }
            // try to find tail for prefix to start completion
            if part.len() > self.max_unicode_input_prefix_len {
                continue;
            }
            if part.len() < self.settings.min_chars_prefix_len {
                break;
            }

            let items = self
                .unicode_input
                .iter()
                .filter_map(|s| {
                    if !starts_with(&s.prefix, part) {
                        return None;
                    }
                    tracing::info!(
                        "Chars prefix: {} index: {}, part: {} {s:?}",
                        chars_prefix,
                        index,
                        part
                    );
                    let line = params.text_document_position.position.line;
                    let start =
                        params.text_document_position.position.character - part.len() as u32;
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
                        label: s.body.to_string(),
                        filter_text: format!("{word_prefix}{}", s.prefix).into(),
                        kind: Some(CompletionItemKind::TEXT),
                        documentation: Documentation::String(s.prefix.to_string()).into(),
                        text_edit: Some(CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
                            replace: range,
                            insert: range,
                            new_text: s.body.to_string(),
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

        chars_snippets
            .into_iter()
            .enumerate()
            .map(move |(index, item)| CompletionItem {
                sort_text: format!("{:0width$}", index, width = 2).into(),
                ..item
            })
    }

    fn paths(
        &self,
        word_prefix: &str,
        chars_prefix: &str,
        params: &CompletionParams,
        current_document: &Document,
    ) -> impl Iterator<Item = CompletionItem> {
        // check is it path
        if !chars_prefix.contains(std::path::MAIN_SEPARATOR) {
            return Vec::new().into_iter();
        }

        let Some(first_char) = chars_prefix.chars().nth(0) else {
            return Vec::new().into_iter();
        };
        let Some(last_char) = chars_prefix.chars().last() else {
            return Vec::new().into_iter();
        };

        // sanitize surround chars
        let chars_prefix = if first_char.is_alphabetic()
            || first_char == std::path::MAIN_SEPARATOR
            || first_char == '~'
            || first_char == '.'
        {
            chars_prefix
        } else {
            &chars_prefix[1..]
        };

        let chars_prefix_len = chars_prefix.len() as u32;
        let document_path = current_document.uri.path();
        let path_state = PathState::new(chars_prefix, &self.home_dir, document_path);

        let chars_prefix = path_state.expand(chars_prefix);

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
                let item_filename = fname.to_str()?;
                let item_filename = item_filename.to_lowercase();
                if !filename.is_empty() && !item_filename.starts_with(&filename) {
                    return None;
                }

                // use full path
                let path = item.path();
                let full_path = path.to_str()?;

                // fold back
                let full_path = path_state.fold(full_path);

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
                    sort_text: Some(full_path.to_string()),
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

    #[cfg(feature = "citation")]
    fn citations<'a>(
        &'a self,
        word_prefix: &str,
        chars_prefix: &str,
        doc: &'a Document,
        params: &CompletionParams,
    ) -> impl Iterator<Item = CompletionItem> {
        let mut items: Vec<CompletionItem> = Vec::new();

        tracing::debug!("Citation word_prefix: {word_prefix}, chars_prefix: {chars_prefix}");

        let Some(re) = &self.citation_bibliography_re else {
            tracing::warn!("Citation bibliography regex empty or invalid");
            return Vec::new().into_iter();
        };

        let Some(slice) = doc.text.get_slice(..) else {
            tracing::warn!("Failed to get rope slice");
            return Vec::new().into_iter();
        };

        let cursor = RopeyCursor::new(slice);

        for span in re
            .captures_iter(Input::new(cursor))
            .filter_map(|c| c.get_group(1))
        {
            if items.len() >= self.settings.max_completion_items {
                break;
            }

            let Some(path) = slice.get_byte_slice(span.start..span.end) else {
                tracing::error!("Failed to get path by span");
                continue;
            };

            // TODO any ways get &str from whole RopeSlice
            let path = path.to_string();

            let path = if path.contains("~") {
                path.replacen('~', &self.home_dir, 1)
            } else {
                path
            };

            // TODO read and parse only if file changed
            tracing::debug!("Citation try to read: {path}");
            let bib = match std::fs::read_to_string(&path) {
                Err(e) => {
                    tracing::error!("Failed to read file {path}: {e}");
                    continue;
                }
                Ok(r) => r,
            };

            let bib = match biblatex::Bibliography::parse(&bib) {
                Err(e) => {
                    tracing::error!("Failed to parse bib file {path}: {e}");
                    continue;
                }
                Ok(r) => r,
            };

            items.extend(
                bib.iter()
                    .filter_map(|b| {
                        let matched = starts_with(&b.key, word_prefix);
                        tracing::debug!(
                            "Citation from file: {path} prefix: {word_prefix} key: {} - match: {}",
                            b.key,
                            matched,
                        );
                        if !matched {
                            return None;
                        }
                        let line = params.text_document_position.position.line;
                        let start = params.text_document_position.position.character
                            - word_prefix.len() as u32;
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
                        let documentation = {
                            let entry_type = b.entry_type.to_string();
                            let title = b
                                .title()
                                .ok()?
                                .iter()
                                .map(|chunk| chunk.v.get())
                                .collect::<Vec<_>>()
                                .join("");
                            let authors = b
                                .author()
                                .ok()?
                                .into_iter()
                                .map(|person| person.to_string())
                                .collect::<Vec<_>>()
                                .join(",");

                            let date = match b.date() {
                                Ok(d) => match d {
                                    biblatex::PermissiveType::Typed(date) => date.to_chunks(),
                                    biblatex::PermissiveType::Chunks(v) => v,
                                }
                                .iter()
                                .map(|chunk| chunk.v.get())
                                .collect::<Vec<_>>()
                                .join(""),
                                Err(e) => {
                                    tracing::error!("On parse date field on entry {b:?}: {e}");
                                    String::new()
                                }
                            };

                            Some(format!(
                                "# {title:?}\n*{authors}*\n\n{entry_type}{}",
                                if date.is_empty() {
                                    date
                                } else {
                                    format!(", {date}")
                                }
                            ))
                        };
                        Some(CompletionItem {
                            label: format!("@{}", b.key),
                            sort_text: Some(word_prefix.to_string()),
                            filter_text: Some(word_prefix.to_string()),
                            kind: Some(CompletionItemKind::REFERENCE),
                            text_edit: Some(CompletionTextEdit::InsertAndReplace(
                                InsertReplaceEdit {
                                    replace: range,
                                    insert: range,
                                    new_text: b.key.to_string(),
                                },
                            )),
                            documentation: Some(Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: documentation.unwrap_or_else(|| {
                                    format!(
                                        "'''{}'''\n\n*fallback to biblatex format*",
                                        b.to_biblatex_string()
                                    )
                                }),
                            })),
                            ..Default::default()
                        })
                    })
                    .take(self.settings.max_completion_items - items.len()),
            );
        }

        items.into_iter()
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

                    let Ok((prefix, chars_prefix, doc)) =
                        self.get_prefix(self.settings.max_chars_prefix_len, &params)
                    else {
                        if tx
                            .send(Err(anyhow::anyhow!("Failed to get prefix")))
                            .is_err()
                        {
                            tracing::error!("Error on send completion response");
                        }
                        continue;
                    };

                    let Some(chars_prefix) = chars_prefix else {
                        if tx
                            .send(Err(anyhow::anyhow!("Failed to get char prefix")))
                            .is_err()
                        {
                            tracing::error!("Error on send completion response");
                        }
                        continue;
                    };

                    if chars_prefix.is_empty() || chars_prefix.starts_with(' ') {
                        if tx
                            .send(Ok(BackendResponse::CompletionResponse(
                                CompletionResponse::Array(Vec::new()),
                            )))
                            .is_err()
                        {
                            tracing::error!("Error on send completion response");
                        }
                        continue;
                    };

                    let base_completion = || {
                        Vec::new()
                            .into_iter()
                            // snippets first
                            .chain(
                                match (
                                    self.settings.feature_snippets,
                                    self.settings.snippets_inline_by_word_tail,
                                    self.settings.snippets_first,
                                    prefix,
                                ) {
                                    (true, true, true, _) if !chars_prefix.is_empty() => {
                                        Some(self.snippets_by_word_tail(chars_prefix, doc, &params))
                                    }
                                    _ => None,
                                }
                                .into_iter()
                                .flatten(),
                            )
                            .chain(
                                match (
                                    self.settings.feature_snippets,
                                    self.settings.snippets_inline_by_word_tail,
                                    self.settings.snippets_first,
                                    prefix,
                                ) {
                                    (true, false, true, Some(prefix)) if !prefix.is_empty() => {
                                        Some(self.snippets(prefix, "", true, doc, &params))
                                    }
                                    _ => None,
                                }
                                .into_iter()
                                .flatten(),
                            )
                            // words
                            .chain(
                                if let Some(prefix) = prefix {
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
                            // snippets last
                            .chain(
                                match (
                                    self.settings.feature_snippets,
                                    self.settings.snippets_inline_by_word_tail,
                                    self.settings.snippets_first,
                                    prefix,
                                ) {
                                    (true, true, false, _) if !chars_prefix.is_empty() => {
                                        Some(self.snippets_by_word_tail(chars_prefix, doc, &params))
                                    }
                                    _ => None,
                                }
                                .into_iter()
                                .flatten(),
                            )
                            .chain(
                                match (
                                    self.settings.feature_snippets,
                                    self.settings.snippets_inline_by_word_tail,
                                    self.settings.snippets_first,
                                    prefix,
                                ) {
                                    (true, false, false, Some(prefix)) if !prefix.is_empty() => {
                                        Some(self.snippets(prefix, "", false, doc, &params))
                                    }
                                    _ => None,
                                }
                                .into_iter()
                                .flatten(),
                            )
                            .chain(
                                if self.settings.feature_unicode_input {
                                    Some(self.unicode_input(
                                        prefix.unwrap_or_default(),
                                        chars_prefix,
                                        &params,
                                    ))
                                } else {
                                    None
                                }
                                .into_iter()
                                .flatten(),
                            )
                            .chain(
                                if self.settings.feature_paths {
                                    Some(self.paths(
                                        prefix.unwrap_or_default(),
                                        chars_prefix,
                                        &params,
                                        doc,
                                    ))
                                } else {
                                    None
                                }
                                .into_iter()
                                .flatten(),
                            )
                            .collect()
                    };

                    #[cfg(feature = "citation")]
                    let results: Vec<CompletionItem> = if self.settings.feature_citations
                        & chars_prefix.contains(&self.settings.citation_prefix_trigger)
                    {
                        self.citations(prefix.unwrap_or_default(), chars_prefix, doc, &params)
                            .collect()
                    } else {
                        base_completion()
                    };

                    #[cfg(not(feature = "citation"))]
                    let results: Vec<CompletionItem> = base_completion();

                    tracing::debug!(
                        "completion request by prefix: {prefix:?} chars prefix: {chars_prefix:?} took {:.2}ms with {} result items",
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
