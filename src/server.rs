use crate::{snippets::Snippet, BackendRequest, BackendResponse, BackendState};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug)]
pub struct Backend {
    client: Client,
    tx: mpsc::UnboundedSender<BackendRequest>,
    _task: tokio::task::JoinHandle<()>,
}

impl Backend {
    async fn log_info(&self, message: &str) {
        tracing::info!(message);
        self.client.log_message(MessageType::INFO, message).await;
    }
    async fn log_err(&self, message: &str) {
        tracing::error!(message);
        self.client.log_message(MessageType::ERROR, message).await;
    }
    async fn send_request(&self, request: BackendRequest) -> anyhow::Result<()> {
        if self.tx.send(request).is_err() {
            self.log_err("error on send request").await;
            anyhow::bail!("Failed to send request");
        }
        Ok(())
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(PositionEncodingKind::UTF32),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![std::path::MAIN_SEPARATOR_STR.to_string()]),
                    ..CompletionOptions::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.log_info("server initialized!").await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.log_info(&format!("Did open: {}", params.text_document.uri.as_str()))
            .await;
        let _ = self.send_request(BackendRequest::NewDoc(params)).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        tracing::debug!("Did save: {params:?}");
        let _ = self.send_request(BackendRequest::SaveDoc(params)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        tracing::debug!("Did change: {params:?}");
        let _ = self.send_request(BackendRequest::ChangeDoc(params)).await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.log_info(&format!("Did change configuration: {params:?}"))
            .await;
        let _ = self
            .send_request(BackendRequest::ChangeConfiguration(params))
            .await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        tracing::debug!("Completion: {params:?}");
        let (tx, rx) = oneshot::channel::<anyhow::Result<BackendResponse>>();

        self.send_request(BackendRequest::CompletionRequest((tx, params)))
            .await
            .map_err(|_| tower_lsp::jsonrpc::Error::internal_error())?;

        let Ok(result) = rx.await else {
            self.log_err("Error on receive completion response").await;
            return Err(tower_lsp::jsonrpc::Error::internal_error());
        };

        match result {
            Ok(BackendResponse::CompletionResponse(r)) => Ok(Some(r)),
            Err(e) => {
                self.log_err(&format!("Completion error: {e}")).await;
                return Err(tower_lsp::jsonrpc::Error::internal_error());
            }
        }
    }

    // mock completionItem/resolve
    async fn completion_resolve(&self, params: CompletionItem) -> Result<CompletionItem> {
        Ok(params)
    }
}

pub async fn start<I, O>(
    read: I,
    write: O,
    snippets: Vec<Snippet>,
    unicode_input: HashMap<String, String>,
    home_dir: String,
) where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
{
    let (tx, backend_state) = BackendState::new(home_dir, snippets, unicode_input).await;

    let task = tokio::spawn(backend_state.start());

    let (service, socket) = LspService::new(|client| Backend {
        client,
        tx,
        _task: task,
    });
    Server::new(read, write, socket).serve(service).await;
}
