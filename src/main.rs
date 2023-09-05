use tokio::sync::{mpsc, oneshot};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use simple_completion_language_server::{
    config_dir, BackendRequest, BackendResponse, BackendState,
};

#[derive(Debug)]
struct Backend {
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
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions::default()),
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
}

#[tokio::main]
async fn main() {
    let _quard = if let Ok(log_file) = &std::env::var("LOG_FILE") {
        let log_file = std::path::Path::new(log_file);
        let file_appender = tracing_appender::rolling::never(
            log_file
                .parent()
                .expect("Failed to parse LOG_FILE parent part"),
            log_file
                .file_name()
                .expect("Failed to parse LOG_FILE file_name part"),
        );
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                std::env::var("RUST_LOG")
                    .unwrap_or_else(|_| "info,simple-comletion-language-server=info".into()),
            ))
            .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
            .init();
        Some(_guard)
    } else {
        None
    };

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let snippets_path = std::env::var("SNIPPETS_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let mut filepath = config_dir();
            filepath.push("snippets");
            filepath
        });
    let (tx, backend_state) = BackendState::new(&snippets_path).await;

    let task = tokio::spawn(backend_state.start());

    let (service, socket) = LspService::new(|client| Backend {
        client,
        tx,
        _task: task,
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
