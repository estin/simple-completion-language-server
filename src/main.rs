use etcetera::base_strategy::{choose_base_strategy, BaseStrategy};
use std::collections::HashMap;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use xshell::{cmd, Shell};

use simple_completion_language_server::{
    server,
    snippets::config::{load_snippets, load_unicode_input_from_path},
    snippets::external::ExternalSnippets,
    StartOptions,
};

async fn serve(start_options: &StartOptions) {
    let _guard = if let Ok(log_file) = &std::env::var("LOG_FILE") {
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
                    .unwrap_or_else(|_| "info,simple-completion-language-server=info".into()),
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

    let unicode_input = load_unicode_input_from_path(&start_options.unicode_input_path)
        .unwrap_or_else(|e| {
            tracing::error!("On read 'unicode input' config: {e}");
            HashMap::new()
        });

    server::start(
        stdin,
        stdout,
        snippets,
        unicode_input,
        start_options.home_dir.clone(),
    )
    .await;
}

fn help() {
    println!(
        "usage:
simple-completion-language-server fetch-external-snippets
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

fn validate_unicode_input(start_options: &StartOptions) -> anyhow::Result<()> {
    let unicode_input = load_unicode_input_from_path(&start_options.unicode_input_path)?;
    tracing::info!("Successful. Total: {}", unicode_input.len());
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    let strategy = choose_base_strategy().expect("Unable to find the config directory!");
    let mut config_dir = strategy.config_dir();
    let config_subdirectory_name =
        std::env::var("SCLS_CONFIG_SUBDIRECTORY").unwrap_or_else(|_| "helix".to_owned());
    config_dir.push(config_subdirectory_name);

    let start_options = StartOptions {
        home_dir: etcetera::home_dir()
            .expect("Unable to get home dir!")
            .to_str()
            .expect("Unable to get home dir as string!")
            .to_string(),
        snippets_path: std::env::var("SNIPPETS_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let mut filepath = config_dir.clone();
                filepath.push("snippets");
                filepath
            }),
        external_snippets_config_path: std::env::var("EXTERNAL_SNIPPETS_CONFIG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let mut filepath = config_dir.clone();
                filepath.push("external-snippets.toml");
                filepath
            }),
        unicode_input_path: std::env::var("UNICODE_INPUT_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let mut filepath = config_dir.clone();
                filepath.push("unicode-input");
                filepath
            }),
    };

    match args.len() {
        2.. => {
            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new(
                    std::env::var("RUST_LOG")
                        .unwrap_or_else(|_| "info,simple-completion-language-server=info".into()),
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
                "validate-unicode-input" => validate_unicode_input(&start_options)
                    .expect("Failed to validate 'unicode input' config"),
                _ => help(),
            }
        }
        _ => serve(&start_options).await,
    };
}
