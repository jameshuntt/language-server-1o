use anyhow::Result;
use lsp_server::Notification;
use lsp_types::DidSaveTextDocumentParams;

use crate::lsp::Backend;

pub fn handle_did_save(backend: &Backend, notification: Notification) -> Result<()> {
    let params: DidSaveTextDocumentParams = serde_json::from_value(notification.params)?;
    let uri = params.text_document.uri;
    backend.refresh_uri(&uri);
    let text = match params.text {
        Some(text) => text,
        None => backend.document_text(&uri)?,
    };
    backend.publish_document_diagnostics(uri, &text)
}
