use anyhow::Result;
use lsp_server::Notification;
use lsp_types::DidChangeTextDocumentParams;

use crate::lsp::Backend;

pub fn handle_did_change(backend: &Backend, notification: Notification) -> Result<()> {
    let params: DidChangeTextDocumentParams = serde_json::from_value(notification.params)?;
    let uri = params.text_document.uri;
    let Some(change) = params.content_changes.into_iter().last() else {
        return Ok(());
    };
    let text = change.text;
    backend
        .documents
        .change(&uri, text.clone(), params.text_document.version);
    backend.refresh_uri(&uri);
    backend.publish_document_diagnostics(uri, &text)
}
