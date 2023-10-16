use tokio::sync::{mpsc, oneshot};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use xshell::{cmd, Shell};

use simple_completion_language_server::{
    config_dir, snippets::config::load_snippets, snippets::external::ExternalSnippets,
    BackendRequest, BackendResponse, BackendState, StartOptions,
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

async fn serve(start_options: &StartOptions) {
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

    let snippets = load_snippets(start_options).unwrap_or_else(|e| {
        tracing::error!("On read snippets: {e}");
        Vec::new()
    });

    let (tx, backend_state) = BackendState::new(snippets).await;

    let task = tokio::spawn(backend_state.start());

    let (service, socket) = LspService::new(|client| Backend {
        client,
        tx,
        _task: task,
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

fn help() {
    println!(
        "usage:
simple-completion-language-server feth-external-snippets
    Fetch external snippets (git clone or git pull).
simple-completion-language-server validate-snippets
    Read all snippets to ensure correctness.
simple-completion-language-server
    Start language server protocol on stdin+stdout."
    );
}

fn fetch_external_snippets(start_options: &StartOptions) -> anyhow::Result<()> {
    tracing::info!(
        "Try read config from: {:?}",
        start_options.external_snippets_config_path
    );

    let path = std::path::Path::new(&start_options.external_snippets_config_path);

    if !path.exists() {
        return Ok(());
    }

    let Some(base_path) = path.parent() else {
        anyhow::bail!("Failed to get base path")
    };

    let base_path = base_path.join("external-snippets");

    let content = std::fs::read_to_string(path)?;

    let sources = toml::from_str::<ExternalSnippets>(&content)
        .map(|sc| sc.sources)
        .map_err(|e| anyhow::anyhow!(e))?;

    let sh = Shell::new()?;
    for source in sources {
        let git_repo = &source.git;
        let destination_path = base_path.join(source.destination_path()?);

        // TODO don't fetch full history?
        if destination_path.exists() {
            sh.change_dir(&destination_path);
            tracing::info!("Try update: {:?}", destination_path);
            cmd!(sh, "git pull --rebase").run()?;
        } else {
            tracing::info!("Try clone {} to {:?}", git_repo, destination_path);
            sh.create_dir(&destination_path)?;
            cmd!(sh, "git clone {git_repo} {destination_path}").run()?;
        }
    }

    Ok(())
}

fn validate_snippets(start_options: &StartOptions) -> anyhow::Result<()> {
    let snippets = load_snippets(start_options)?;
    tracing::info!("Successful. Total: {}", snippets.len());
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    let start_options = StartOptions {
        snippets_path: std::env::var("SNIPPETS_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let mut filepath = config_dir();
                filepath.push("snippets");
                filepath
            }),
        external_snippets_config_path: std::env::var("EXTERNAL_SNIPPETS_CONFIG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let mut filepath = config_dir();
                filepath.push("external-snippets.toml");
                filepath
            }),
    };

    match args.len() {
        2.. => {
            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new(
                    std::env::var("RUST_LOG")
                        .unwrap_or_else(|_| "info,simple-comletion-language-server=info".into()),
                ))
                .with(tracing_subscriber::fmt::layer())
                .init();

            let cmd = args[1].parse::<String>().expect("command required");

            if cmd.contains("-h") || cmd.contains("help") {
                help();
                return;
            }

            match cmd.as_str() {
                "fetch-external-snippets" => fetch_external_snippets(&start_options)
                    .expect("Failed to fetch external snippets"),
                "validate-snippets" => {
                    validate_snippets(&start_options).expect("Failed to validate snippets")
                }
                _ => help(),
            }
        }
        _ => serve(&start_options).await,
    };
}
