use simple_completion_language_server::{server, snippets};
use std::collections::HashMap;

use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tower_lsp::{jsonrpc, lsp_types};

pub struct AsyncIn(UnboundedReceiver<String>);
pub struct AsyncOut(UnboundedSender<String>);

fn encode_message(content_type: Option<&str>, message: &str) -> String {
    let content_type = content_type
        .map(|ty| format!("\r\nContent-Type: {ty}"))
        .unwrap_or_default();

    format!(
        "Content-Length: {}{}\r\n\r\n{}",
        message.len(),
        content_type,
        message
    )
}

impl AsyncRead for AsyncIn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let rx = self.get_mut();
        match rx.0.poll_recv(cx) {
            Poll::Ready(Some(v)) => {
                tracing::debug!("read value: {:?}", v);
                buf.put_slice(v.as_bytes());
                Poll::Ready(Ok(()))
            }
            _ => Poll::Pending,
        }
    }
}

impl AsyncWrite for AsyncOut {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let tx = self.get_mut();
        let value = String::from_utf8(buf.to_vec()).unwrap();
        tracing::debug!("write value: {value:?}");
        let _ = tx.0.send(value);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct TestContext {
    pub request_tx: UnboundedSender<String>,
    pub response_rx: UnboundedReceiver<String>,
    pub _server: tokio::task::JoinHandle<()>,
}

impl TestContext {
    pub async fn new(
        snippets: Vec<snippets::Snippet>,
        unicode_input: HashMap<String, String>,
        home_dir: String,
    ) -> anyhow::Result<Self> {
        let (request_tx, rx) = mpsc::unbounded_channel::<String>();
        let (tx, response_rx) = mpsc::unbounded_channel::<String>();

        let async_in = AsyncIn(rx);
        let async_out = AsyncOut(tx);

        let server = tokio::spawn(async move {
            server::start(async_in, async_out, snippets, unicode_input, home_dir).await
        });

        Ok(Self {
            request_tx,
            response_rx,
            _server: server,
        })
    }

    pub async fn send_all(&mut self, messages: &[&str]) -> anyhow::Result<()> {
        for message in messages {
            self.send(&jsonrpc::Request::from_str(message)?).await?;
        }
        Ok(())
    }

    pub async fn send(&mut self, request: &jsonrpc::Request) -> anyhow::Result<()> {
        self.request_tx
            .send(encode_message(None, &serde_json::to_string(request)?))?;
        Ok(())
    }

    pub async fn recv<R: std::fmt::Debug + serde::de::DeserializeOwned>(
        &mut self,
    ) -> anyhow::Result<R> {
        // TODO split response for single messages
        loop {
            let response = self
                .response_rx
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("empty response"))?;
            // decode response
            let payload = response.split('\n').last().unwrap_or_default();

            // skip log messages
            if payload.contains("window/logMessage") {
                tracing::debug!("log: {payload}");
                continue;
            }
            let response = serde_json::from_str::<jsonrpc::Response>(payload)?;
            let (_id, result) = response.into_parts();
            return Ok(serde_json::from_value(result?)?);
        }
    }

    pub async fn request<R: std::fmt::Debug + serde::de::DeserializeOwned>(
        &mut self,
        request: &jsonrpc::Request,
    ) -> anyhow::Result<R> {
        self.send(request).await?;
        self.recv().await
    }

    pub async fn initialize(&mut self) -> anyhow::Result<()> {
        let request = jsonrpc::Request::build("initialize")
            .id(1)
            .params(serde_json::json!({"capabilities":{}}))
            .finish();

        let _ = self
            .request::<lsp_types::InitializeResult>(&request)
            .await?;

        Ok(())
    }
}

#[test_log::test(tokio::test)]
async fn initialize() -> anyhow::Result<()> {
    let mut context = TestContext::new(Vec::new(), HashMap::new(), String::new()).await?;

    let request = jsonrpc::Request::build("initialize")
        .id(1)
        .params(serde_json::json!({"capabilities":{}}))
        .finish();

    let response = context
        .request::<lsp_types::InitializeResult>(&request)
        .await?;

    assert_eq!(
        response.capabilities.completion_provider,
        Some(lsp_types::CompletionOptions {
            resolve_provider: Some(false),
            trigger_characters: Some(vec![std::path::MAIN_SEPARATOR_STR.to_string()]),
            ..lsp_types::CompletionOptions::default()
        })
    );
    assert_eq!(
        response.capabilities.text_document_sync,
        Some(lsp_types::TextDocumentSyncCapability::Kind(
            lsp_types::TextDocumentSyncKind::INCREMENTAL,
        ))
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn completion() -> anyhow::Result<()> {
    let mut context = TestContext::new(Vec::new(), HashMap::new(), String::new()).await?;
    context.initialize().await?;
    context.send_all(&[
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"languageId":"python","text":"hello\nhe","uri":"file:///tmp/main.py","version":0}}}"#,
        r#"{"jsonrpc":"2.0","method":"textDocument/completion","params":{"position":{"character":2,"line":1},"textDocument":{"uri":"file:///tmp/main.py"}},"id":3}"#
    ]).await?;

    let response = context.recv::<lsp_types::CompletionResponse>().await?;

    let lsp_types::CompletionResponse::Array(items) = response else {
        anyhow::bail!("completion array expected")
    };

    assert_eq!(items.len(), 1);
    assert_eq!(
        items.into_iter().map(|i| i.label).collect::<Vec<_>>(),
        vec!["hello"]
    );

    context.send_all(&[
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"languageId":"python","text":"hello\nel","uri":"file:///tmp/main2.py","version":0}}}"#,
        r#"{"jsonrpc":"2.0","method":"textDocument/completion","params":{"position":{"character":2,"line":1},"textDocument":{"uri":"file:///tmp/main2.py"}},"id":3}"#
    ]).await?;

    let response = context.recv::<lsp_types::CompletionResponse>().await?;

    let lsp_types::CompletionResponse::Array(items) = response else {
        anyhow::bail!("completion array expected")
    };

    assert_eq!(items.len(), 0);

    Ok(())
}

#[test_log::test(tokio::test)]
async fn snippets() -> anyhow::Result<()> {
    let mut context = TestContext::new(
        vec![
            snippets::Snippet {
                scope: Some(vec!["python".to_string()]),
                prefix: "ma".to_string(),
                body: "def main(): pass".to_string(),
                description: None,
            },
            snippets::Snippet {
                scope: Some(vec!["c".to_string()]),
                prefix: "ma".to_string(),
                body: "malloc".to_string(),
                description: None,
            },
        ],
        HashMap::new(),
        String::new(),
    )
    .await?;
    context.initialize().await?;
    context.send_all(&[
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"languageId":"python","text":"ma","uri":"file:///tmp/main.py","version":0}}}"#,
        r#"{"jsonrpc":"2.0","method":"textDocument/completion","params":{"position":{"character":2,"line":0},"textDocument":{"uri":"file:///tmp/main.py"}},"id":3}"#
    ]).await?;

    let response = context.recv::<lsp_types::CompletionResponse>().await?;

    let lsp_types::CompletionResponse::Array(items) = response else {
        anyhow::bail!("completion array expected")
    };

    assert_eq!(items.len(), 1);
    assert_eq!(
        items
            .into_iter()
            .filter_map(|i| i.insert_text)
            .collect::<Vec<_>>(),
        vec!["def main(): pass"]
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn unicode_input() -> anyhow::Result<()> {
    let mut context = TestContext::new(
        Vec::new(),
        HashMap::from_iter([
            ("alpha".to_string(), "α".to_string()),
            ("betta".to_string(), "β".to_string()),
        ]),
        String::new(),
    )
    .await?;
    context.initialize().await?;
    context.send_all(&[
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"languageId":"python","text":"α+bet","uri":"file:///tmp/main.py","version":0}}}"#,
        r#"{"jsonrpc":"2.0","method":"textDocument/completion","params":{"position":{"character":5,"line":0},"textDocument":{"uri":"file:///tmp/main.py"}},"id":3}"#
    ]).await?;

    let response = context.recv::<lsp_types::CompletionResponse>().await?;

    let lsp_types::CompletionResponse::Array(items) = response else {
        anyhow::bail!("completion array expected")
    };

    assert_eq!(
        items
            .into_iter()
            .filter_map(|i| match i.text_edit {
                Some(lsp_types::CompletionTextEdit::InsertAndReplace(te)) => Some(te.new_text),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["β"]
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn paths() -> anyhow::Result<()> {
    std::fs::create_dir_all("/tmp/scls-test/sub-folder")?;

    let mut context = TestContext::new(Vec::new(), HashMap::new(), "/tmp".to_string()).await?;
    context.initialize().await?;
    context.send_all(&[
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"languageId":"python","text":"/tmp/scls-test/","uri":"file:///tmp/main.py","version":0}}}"#,
        r#"{"jsonrpc":"2.0","method":"textDocument/completion","params":{"position":{"character":15,"line":0},"textDocument":{"uri":"file:///tmp/main.py"}},"id":3}"#
    ]).await?;

    let response = context.recv::<lsp_types::CompletionResponse>().await?;

    let lsp_types::CompletionResponse::Array(items) = response else {
        anyhow::bail!("completion array expected")
    };

    assert_eq!(
        items
            .into_iter()
            .filter_map(|i| match i.text_edit {
                Some(lsp_types::CompletionTextEdit::InsertAndReplace(te)) => Some(te.new_text),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["/tmp/scls-test/sub-folder"]
    );

    context.send_all(&[
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"languageId":"python","text":"/tmp/scls-test/su","uri":"file:///tmp/main2.py","version":0}}}"#,
        r#"{"jsonrpc":"2.0","method":"textDocument/completion","params":{"position":{"character":17,"line":0},"textDocument":{"uri":"file:///tmp/main2.py"}},"id":3}"#
    ]).await?;

    let response = context.recv::<lsp_types::CompletionResponse>().await?;

    let lsp_types::CompletionResponse::Array(items) = response else {
        anyhow::bail!("completion array expected")
    };

    assert_eq!(
        items
            .into_iter()
            .filter_map(|i| match i.text_edit {
                Some(lsp_types::CompletionTextEdit::InsertAndReplace(te)) => Some(te.new_text),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["/tmp/scls-test/sub-folder"]
    );

    context.send_all(&[
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"languageId":"python","text":"~/scls-test/su","uri":"file:///tmp/main3.py","version":0}}}"#,
        r#"{"jsonrpc":"2.0","method":"textDocument/completion","params":{"position":{"character":14,"line":0},"textDocument":{"uri":"file:///tmp/main3.py"}},"id":3}"#
    ]).await?;

    let response = context.recv::<lsp_types::CompletionResponse>().await?;

    let lsp_types::CompletionResponse::Array(items) = response else {
        anyhow::bail!("completion array expected")
    };

    assert_eq!(
        items
            .into_iter()
            .filter_map(|i| match i.text_edit {
                Some(lsp_types::CompletionTextEdit::InsertAndReplace(te)) => Some(te.new_text),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["~/scls-test/sub-folder"]
    );

    Ok(())
}
