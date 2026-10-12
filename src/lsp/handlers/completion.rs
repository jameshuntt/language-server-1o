use anyhow::Result;
use lsp_types::{CompletionItem, CompletionParams};

use crate::lsp::Backend;

pub fn handle_completion(backend: &Backend, params: CompletionParams) -> Result<Vec<CompletionItem>> {
    backend.completion_items(params)
}
