use anyhow::Result;
use lsp_server::Notification;
use lsp_types::DidOpenTextDocumentParams;

use crate::lsp::Backend;

pub fn handle_did_open(backend: &Backend, notification: Notification) -> Result<()> {
    let params: DidOpenTextDocumentParams = serde_json::from_value(notification.params)?;
    let uri = params.text_document.uri;
    let text = params.text_document.text;
    backend
        .documents
        .open(&uri, text.clone(), params.text_document.version);
    backend.refresh_uri(&uri);
    backend.publish_document_diagnostics(uri, &text)
}
