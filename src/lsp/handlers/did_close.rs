use anyhow::Result;
use lsp_server::Notification;
use lsp_types::DidCloseTextDocumentParams;

use crate::lsp::Backend;

pub fn handle_did_close(backend: &Backend, notification: Notification) -> Result<()> {
    let params: DidCloseTextDocumentParams = serde_json::from_value(notification.params)?;
    let uri = params.text_document.uri;
    backend.documents.close(&uri);
    backend.client.send_diagnostics(uri, vec![])
}
