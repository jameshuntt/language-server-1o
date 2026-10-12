use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;
use std::str::FromStr;

use anyhow::Result;
use lsp_server::{Message, Notification, Request};
use lsp_types::{
    CompletionItem, CompletionParams, CompletionResponse, Diagnostic, InitializeParams,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};

use crate::document_store::DocumentStore;
use crate::engine::{
    compute_completion_context, completions_for_schema, diagnostics_for_text_workspace,
    parse_nodeplan_workspace_link, CompletionKind, Lexer, Parser, Workspace,
};
use crate::semantic::{
    normalize_completion_items, schema_for_node_stack, semantic_ref_for_schema,
    semantic_references, SemanticBridge, SemanticContext,
};
use crate::utils::{parse_filename, to_lsp, uri_to_path};

#[derive(Clone)]
pub struct Client {
    sender: crossbeam_channel::Sender<Message>,
}

impl Client {
    pub fn new(sender: crossbeam_channel::Sender<Message>) -> Self {
        Self { sender }
    }

    pub fn send_diagnostics(&self, uri: Uri, diagnostics: Vec<Diagnostic>) -> Result<()> {
        let notification = Notification::new(
            "textDocument/publishDiagnostics".into(),
            lsp_types::PublishDiagnosticsParams {
                uri,
                diagnostics,
                version: None,
            },
        );
        self.sender.send(Message::Notification(notification))?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Backend {
    pub(crate) client: Client,
    pub(crate) workspace: Arc<RwLock<Workspace>>,
    pub(crate) documents: Arc<DocumentStore>,
    pub(crate) semantic: Arc<SemanticBridge>,
    semantic_refresh_started: Arc<AtomicBool>,
}

impl Backend {
    pub fn new(client: Client, params: InitializeParams) -> Self {
        let root = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .map(|folder| uri_to_path(&folder.uri))
            .or_else(|| params.root_uri.as_ref().map(uri_to_path))
            .unwrap_or_else(|| PathBuf::from("/"));

        // NodePlan registry access is an optional provider, not a runtime
        // dependency for standalone projects. The semantic trait remains public.
        let semantic = SemanticBridge::offline();
        Self::new_with_semantic_bridge(client, root, semantic)
    }

    pub fn new_with_semantic_bridge(
        client: Client,
        root: impl Into<PathBuf>,
        semantic: SemanticBridge,
    ) -> Self {
        let mut workspace = Workspace::new(root.into());
        workspace.reindex();
        Self {
            client,
            workspace: Arc::new(RwLock::new(workspace)),
            documents: Arc::new(DocumentStore::default()),
            semantic: Arc::new(semantic),
            semantic_refresh_started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start_semantic_refresh_loop(&self) {
        if self
            .semantic_refresh_started
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let backend = self.clone();
        thread::Builder::new()
            .name("1o-semantic-refresh".into())
            .spawn(move || {
                let mut last_state: Option<String> = None;
                loop {
                    thread::sleep(Duration::from_millis(750));
                    if backend.documents.entries().is_empty() {
                        continue;
                    }
                    let Some(context) = backend.semantic_context() else {
                        continue;
                    };
                    let current_state = match backend.semantic.refresh_identity(&context) {
                        Ok(Some(identity)) => format!("available:{identity}"),
                        Ok(None) => "available:unversioned".into(),
                        Err(_) => "unavailable".into(),
                    };
                    let changed = last_state
                        .as_ref()
                        .is_some_and(|prior| prior != &current_state);
                    last_state = Some(current_state);
                    if !changed {
                        continue;
                    }
                    for (uri_text, document) in backend.documents.entries() {
                        let Ok(uri) = Uri::from_str(&uri_text) else {
                            continue;
                        };
                        let _ = backend.publish_document_diagnostics(uri, &document.text);
                    }
                }
            })
            .expect("spawn bounded semantic refresh loop");
    }

    pub fn capabilities() -> ServerCapabilities {
        ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            ..Default::default()
        }
    }

    pub fn handle_request(&self, request: Request) -> Result<Option<serde_json::Value>> {
        match request.method.as_str() {
            "textDocument/completion" => {
                let params: CompletionParams = serde_json::from_value(request.params)?;
                let items = self.completion_items(params)?;
                Ok(Some(serde_json::to_value(CompletionResponse::Array(items))?))
            }
            _ => Ok(None),
        }
    }

    pub fn handle_notification(&self, notification: Notification) -> Result<()> {
        match notification.method.as_str() {
            "textDocument/didOpen" => crate::lsp::handlers::handle_did_open(self, notification),
            "textDocument/didChange" => crate::lsp::handlers::handle_did_change(self, notification),
            "textDocument/didClose" => crate::lsp::handlers::handle_did_close(self, notification),
            "textDocument/didSave" => crate::lsp::handlers::handle_did_save(self, notification),
            "workspace/didChangeWatchedFiles" => {
                // Schemas and project manifests can change independently of 1o
                // sources. Reload the registry and revalidate open documents.
                self.workspace.write().expect("workspace write lock").reload_schema_state();
                for (uri_text, document) in self.documents.entries() {
                    if let Ok(uri) = Uri::from_str(&uri_text) {
                        let _ = self.publish_document_diagnostics(uri, &document.text);
                    }
                }
                Ok(())
            }
            "workspace/didCreateFiles" => {
                self.handle_file_events(&notification.params, false);
                Ok(())
            }
            "workspace/didDeleteFiles" => {
                self.handle_file_events(&notification.params, true);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn handle_file_events(&self, params: &serde_json::Value, deleted: bool) {
        let Some(files) = params.get("files").and_then(serde_json::Value::as_array) else {
            return;
        };
        for file in files {
            let Some(uri) = file.get("uri").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Ok(parsed_uri) = uri.parse::<Uri>() else { continue; };
            let path = uri_to_path(&parsed_uri);
            if deleted {
                self.workspace
                    .write()
                    .expect("workspace write lock")
                    .remove_path(&path);
            } else {
                self.workspace
                    .write()
                    .expect("workspace write lock")
                    .refresh_path(&path);
            }
        }
    }

    pub fn refresh_uri(&self, uri: &Uri) {
        let path = uri_to_path(uri);
        self.workspace
            .write()
            .expect("workspace write lock")
            .refresh_path(&path);
    }

    pub fn remove_uri(&self, uri: &Uri) {
        let path = uri_to_path(uri);
        self.workspace
            .write()
            .expect("workspace write lock")
            .remove_path(&path);
    }

    pub fn document_text(&self, uri: &Uri) -> Result<String> {
        if let Some(text) = self.documents.text(uri) {
            return Ok(text);
        }
        Ok(fs::read_to_string(uri_to_path(uri))?)
    }

    pub fn set_open_document(&self, uri: &Uri, text: String, version: i32) {
        self.documents.open(uri, text, version);
        self.refresh_uri(uri);
    }

    pub fn document_diagnostics(&self, uri: &Uri, text: &str) -> Result<Vec<Diagnostic>> {
        let path = uri_to_path(uri);
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            return Ok(vec![]);
        };
        let Some(record) = parse_filename(file_name) else {
            return Ok(vec![]);
        };

        let (mut diagnostics, schema) = {
            let workspace = self.workspace.read().expect("workspace read lock");
            let mut diagnostics = diagnostics_for_text_workspace(
                &workspace,
                &record.modifier,
                &record.version,
                text,
            );
            diagnostics.extend(
                crate::typeserver_declarative::declarative_diagnostics_for_text(
                    &workspace,
                    &record.modifier,
                    text,
                ),
            );
            diagnostics.extend(
                crate::typeserver_operation_endpoint::operation_endpoint_diagnostics_for_text(
                    &workspace,
                    &record.modifier,
                    text,
                ),
            );
            diagnostics.extend(
                crate::application_semantics::application_semantics_diagnostics_for_text(
                    &workspace,
                    &record.modifier,
                    text,
                ),
            );
            (
                diagnostics,
                workspace
                    .schema_for_file(&record.modifier, &record.version),
            )
        };

        if let Some(schema) = schema {
            if let Ok(ast) = Parser::new(Lexer::new(text).tokenize()).parse() {
                let references = semantic_references(&ast, &schema);
                if !references.is_empty() {
                    let context = self.semantic_context();
                    diagnostics.extend(
                        self.semantic
                            .validate_references(&references, context.as_ref()),
                    );
                }
            }
        }
        Ok(diagnostics
            .iter()
            .map(|diagnostic| to_lsp(text, diagnostic))
            .collect())
    }

    pub fn publish_document_diagnostics(&self, uri: Uri, text: &str) -> Result<()> {
        let diagnostics = self.document_diagnostics(&uri, text)?;
        self.client.send_diagnostics(uri, diagnostics)
    }

    pub fn completion_items(&self, params: CompletionParams) -> Result<Vec<CompletionItem>> {
        let uri = params.text_document_position.text_document.uri;
        let text = self.document_text(&uri)?;
        let path = uri_to_path(&uri);
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            return Ok(vec![]);
        };
        let Some(record) = parse_filename(file_name) else {
            return Ok(vec![]);
        };
        let schema = {
            let workspace = self.workspace.read().expect("workspace read lock");
            workspace
                .schema_for_file(&record.modifier, &record.version)
        };
        let Some(base_schema) = schema else {
            return Ok(vec![]);
        };
        let ast = match Parser::new(Lexer::new(&text).tokenize()).parse() {
            Ok(ast) => ast,
            Err(_) => return Ok(vec![]),
        };
        let cursor = offset_from_position(&text, params.text_document_position.position);
        let context = compute_completion_context(&ast, cursor);
        let current_schema = schema_for_node_stack(&base_schema, &context.parent_stack)
            .unwrap_or(&base_schema);

        let mut items = match context.kind {
            CompletionKind::Key => completions_for_schema(current_schema),
            CompletionKind::Value(_) => {
                if let Some(semantic_ref) = semantic_ref_for_schema(current_schema) {
                    self.semantic
                        .complete(semantic_ref, self.semantic_context().as_ref())
                } else if let Some(workspace_ref) = workspace_ref_for_schema(current_schema) {
                    let workspace = self.workspace.read().expect("workspace read lock");
                    let values = crate::application_semantics::application_reference_values(
                        &workspace,
                        workspace_ref,
                    )
                    .unwrap_or_else(|| {
                        crate::typeserver_operation_endpoint::declared_workspace_reference_values(
                            &workspace,
                            workspace_ref,
                        )
                    });
                    values
                        .into_iter()
                        .map(|value| CompletionItem::new_simple(value, "1o workspace reference".into()))
                        .collect()
                } else {
                    primitive_value_completions(current_schema)
                }
            }
            CompletionKind::None => vec![],
        };
        normalize_completion_items(&mut items);
        Ok(items)
    }

    pub fn semantic_context(&self) -> Option<SemanticContext> {
        let link_files = self
            .workspace
            .read()
            .expect("workspace read lock")
            .index
            .files_with_modifier("nodeplan");
        for record in link_files {
            let Ok(text) = fs::read_to_string(&record.path) else {
                continue;
            };
            match parse_nodeplan_workspace_link(&text) {
                Ok(Some(link)) => {
                    return Some(SemanticContext {
                        workspace_id: link.workspace_id,
                    });
                }
                Ok(None) => {}
                Err(error) => eprintln!(
                    "[1o-lsp] invalid NodePlan link {}: {error}",
                    record.path.display()
                ),
            }
        }
        None
    }

    pub fn workspace_root(&self) -> PathBuf {
        self.workspace.read().expect("workspace read lock").root.clone()
    }
}

fn workspace_ref_for_schema(schema: &serde_json::Value) -> Option<&str> {
    schema
        .get("x-1o-workspace-ref")
        .and_then(serde_json::Value::as_str)
}

fn primitive_value_completions(schema: &serde_json::Value) -> Vec<CompletionItem> {
    if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array) {
        return values
            .iter()
            .filter_map(|value| value.as_str())
            .map(|value| CompletionItem::new_simple(value.to_string(), "schema enum".into()))
            .collect();
    }
    if schema.get("type").and_then(serde_json::Value::as_str) == Some("boolean") {
        return ["false", "true"]
            .into_iter()
            .map(|value| CompletionItem::new_simple(value.into(), "boolean".into()))
            .collect();
    }
    vec![]
}

fn offset_from_position(text: &str, position: lsp_types::Position) -> usize {
    let mut offset = 0usize;
    for (line, value) in text.split_inclusive('\n').enumerate() {
        if line as u32 == position.line {
            return offset + (position.character as usize).min(value.trim_end_matches('\n').len());
        }
        offset += value.len();
    }
    text.len()
}

pub fn is_path_inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tempfile::TempDir;

    #[test]
    fn completion_document_source_prefers_unsaved_in_memory_text() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("v1.example.1o.service");
        fs::write(&path, "disk text").unwrap();
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let backend = Backend::new_with_semantic_bridge(
            Client::new(sender),
            temp.path(),
            SemanticBridge::offline(),
        );
        let uri = Uri::from_str(&format!("file://{}", path.display())).unwrap();
        backend.set_open_document(&uri, "unsaved current text".into(), 2);
        assert_eq!(backend.document_text(&uri).unwrap(), "unsaved current text");
    }

    #[test]
    fn page_sibling_whitespace_completes_schema_keys() {
        let tmp = TempDir::new().unwrap();
        let schema_dir = tmp.path().join("schema/page");
        fs::create_dir_all(&schema_dir).unwrap();
        fs::write(tmp.path().join("schema/1o.version.json"),
                  r#"{"versions":{"v1":{"page":"v1"}}}"#).unwrap();
        fs::write(schema_dir.join("v1.schema.json"),
                  r#"{"type":"object","properties":{"page":{"type":"object","properties":{"id":{"type":"string"},"route":{"type":"string"},"title":{"type":"string"}}}}}"#).unwrap();
        let text = "{\n  page {\n    id \"home\"\n    route \"/\"\n    title \"Home\"\n  }\n}\n";
        let path = tmp.path().join("v1.home.1o.page");
        fs::write(&path, text).unwrap();
        let uri = Uri::from_str(&format!("file://{}", path.display())).unwrap();
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let backend = Backend::new_with_semantic_bridge(
            Client::new(sender), tmp.path(), SemanticBridge::offline());
        backend.set_open_document(&uri, text.into(), 1);
        let params = CompletionParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                position: lsp_types::Position { line: 3, character: 3 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };
        let items = backend.completion_items(params).unwrap();
        let labels = items.iter().map(|item| item.label.as_str()).collect::<Vec<_>>();
        assert!(labels.contains(&"route") && labels.contains(&"title"), "{labels:?}");
    }

    #[test]
    fn actual_server_capabilities_advertise_full_sync_and_completion() {
        let capabilities = Backend::capabilities();
        assert!(matches!(
            capabilities.text_document_sync,
            Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL))
        ));
        assert!(capabilities.completion_provider.is_some());
    }

    #[test]
    fn declarative_schema_enum_completion_exposes_uuid_versions_and_sqlite_backend() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/typeserver_declarative_schema_bridge_v1");
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let backend = Backend::new_with_semantic_bridge(
            Client::new(sender),
            &fixture,
            SemanticBridge::offline(),
        );

        let model_path = fixture.join("v1.todo.1o.model");
        let model_text = fs::read_to_string(&model_path).unwrap();
        let model_uri = Uri::from_str(&format!("file://{}", model_path.display())).unwrap();
        backend.set_open_document(&model_uri, model_text.clone(), 1);
        let uuid_items = backend
            .completion_items(completion_params_at_text(&model_uri, &model_text, "v4"))
            .unwrap();
        assert!(uuid_items.iter().any(|item| item.label == "v4"));
        assert!(uuid_items.iter().any(|item| item.label == "v5"));

        let container_path = fixture.join("v1.todos.1o.container");
        let container_text = fs::read_to_string(&container_path).unwrap();
        let container_uri =
            Uri::from_str(&format!("file://{}", container_path.display())).unwrap();
        backend.set_open_document(&container_uri, container_text.clone(), 1);
        let backend_items = backend
            .completion_items(completion_params_at_text(
                &container_uri,
                &container_text,
                "sqlite",
            ))
            .unwrap();
        assert!(backend_items.iter().any(|item| item.label == "sqlite"));
    }

    #[test]
    fn declarative_lsp_reports_invalid_primitive_and_missing_model_locally() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/typeserver_declarative_schema_bridge_v1");
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let backend = Backend::new_with_semantic_bridge(
            Client::new(sender),
            &fixture,
            SemanticBridge::offline(),
        );

        let model_path = fixture.join("v1.todo.1o.model");
        let model_uri = Uri::from_str(&format!("file://{}", model_path.display())).unwrap();
        let invalid_model = r#"{ model { name "Todo" field { name "title" type { string { min_length 10 max_length 1 } } } } }"#;
        let model_diagnostics = backend
            .document_diagnostics(&model_uri, invalid_model)
            .unwrap();
        assert!(model_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("min_length 10 cannot exceed max_length 1")));

        let container_path = fixture.join("v1.todos.1o.container");
        let container_uri =
            Uri::from_str(&format!("file://{}", container_path.display())).unwrap();
        let missing_model = r#"{ container { name "todos" model "Missing" storage { backend "sqlite" namespace "local.todos" } service "type_serverd" } }"#;
        let container_diagnostics = backend
            .document_diagnostics(&container_uri, missing_model)
            .unwrap();
        assert!(container_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown 1o model 'Missing'")));
    }

    #[test]
    fn typed_operation_endpoint_completion_uses_schema_and_workspace_references() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/typed_operation_endpoint_bridge_v1");
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let backend = Backend::new_with_semantic_bridge(
            Client::new(sender),
            &fixture,
            SemanticBridge::offline(),
        );

        let slice_path = fixture.join("v1.todo_api.1o.slice");
        let slice_text = fs::read_to_string(&slice_path).unwrap();
        let slice_uri = Uri::from_str(&format!("file://{}", slice_path.display())).unwrap();
        backend.set_open_document(&slice_uri, slice_text.clone(), 1);
        let action_items = backend
            .completion_items(completion_params_at_text(&slice_uri, &slice_text, "\"create\""))
            .unwrap();
        assert!(action_items.iter().any(|item| item.label == "create"));
        assert!(action_items.iter().any(|item| item.label == "read"));
        assert!(action_items.iter().any(|item| item.label == "update"));
        assert!(action_items.iter().any(|item| item.label == "tombstone"));
        let model_items = backend
            .completion_items(completion_params_at_text(&slice_uri, &slice_text, "Todo"))
            .unwrap();
        assert!(model_items.iter().any(|item| item.label == "Todo"));
        let container_items = backend
            .completion_items(completion_params_at_text(&slice_uri, &slice_text, "todos"))
            .unwrap();
        assert!(container_items.iter().any(|item| item.label == "todos"));

        let endpoint_path = fixture.join("v1.todo_local.1o.endpoint");
        let endpoint_text = fs::read_to_string(&endpoint_path).unwrap();
        let endpoint_uri = Uri::from_str(&format!("file://{}", endpoint_path.display())).unwrap();
        backend.set_open_document(&endpoint_uri, endpoint_text.clone(), 1);
        let method_items = backend
            .completion_items(completion_params_at_text(&endpoint_uri, &endpoint_text, "POST"))
            .unwrap();
        assert!(method_items.iter().any(|item| item.label == "POST"));
        assert!(method_items.iter().any(|item| item.label == "GET"));
        assert!(method_items.iter().any(|item| item.label == "PATCH"));
        assert!(method_items.iter().any(|item| item.label == "DELETE"));
        let operation_items = backend
            .completion_items(completion_params_at_text(&endpoint_uri, &endpoint_text, "todo.create"))
            .unwrap();
        assert!(operation_items.iter().any(|item| item.label == "todo.create"));
        assert!(operation_items.iter().any(|item| item.label == "todo.read"));
    }

    #[test]
    fn crud_lifecycle_lsp_completes_update_tombstone_and_declared_operations() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/crud_lifecycle_bridge_v1");
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let backend = Backend::new_with_semantic_bridge(
            Client::new(sender),
            &fixture,
            SemanticBridge::offline(),
        );

        let slice_path = fixture.join("v1.todo_api.1o.slice");
        let slice_text = fs::read_to_string(&slice_path).unwrap();
        let slice_uri = Uri::from_str(&format!("file://{}", slice_path.display())).unwrap();
        backend.set_open_document(&slice_uri, slice_text.clone(), 1);
        let action_items = backend
            .completion_items(completion_params_at_text(&slice_uri, &slice_text, "\"update\""))
            .unwrap();
        assert!(action_items.iter().any(|item| item.label == "update"));
        assert!(action_items.iter().any(|item| item.label == "tombstone"));

        let endpoint_path = fixture.join("v1.todo_local.1o.endpoint");
        let endpoint_text = fs::read_to_string(&endpoint_path).unwrap();
        let endpoint_uri = Uri::from_str(&format!("file://{}", endpoint_path.display())).unwrap();
        backend.set_open_document(&endpoint_uri, endpoint_text.clone(), 1);
        let method_items = backend
            .completion_items(completion_params_at_text(&endpoint_uri, &endpoint_text, "PATCH"))
            .unwrap();
        assert!(method_items.iter().any(|item| item.label == "PATCH"));
        assert!(method_items.iter().any(|item| item.label == "DELETE"));
        let operation_items = backend
            .completion_items(completion_params_at_text(&endpoint_uri, &endpoint_text, "todo.update"))
            .unwrap();
        assert!(operation_items.iter().any(|item| item.label == "todo.update"));
        assert!(operation_items.iter().any(|item| item.label == "todo.delete"));
    }


    #[derive(Default)]
    struct ApplicationSemanticProvider;

    impl crate::semantic::SemanticProvider for ApplicationSemanticProvider {
        fn complete(
            &self,
            semantic_ref: &str,
            _: &crate::semantic::SemanticContext,
        ) -> Result<Vec<crate::semantic::SemanticSymbol>, crate::semantic::SemanticProviderError> {
            if semantic_ref != "nodeplan.service" {
                return Err(crate::semantic::SemanticProviderError::UnsupportedSemanticRef(
                    semantic_ref.into(),
                ));
            }
            Ok(vec![crate::semantic::SemanticSymbol {
                value: "type_serverd".into(),
                detail: "mock live NodePlan service".into(),
            }])
        }

        fn validate(
            &self,
            semantic_ref: &str,
            value: &str,
            _: &crate::semantic::SemanticContext,
        ) -> Result<crate::semantic::SemanticValidation, crate::semantic::SemanticProviderError> {
            if semantic_ref != "nodeplan.service" {
                return Err(crate::semantic::SemanticProviderError::UnsupportedSemanticRef(
                    semantic_ref.into(),
                ));
            }
            if value == "type_serverd" {
                Ok(crate::semantic::SemanticValidation::Exists {
                    accepted_generation: 7,
                    accepted_generation_digest: "application-semantics".into(),
                })
            } else {
                Ok(crate::semantic::SemanticValidation::Missing {
                    accepted_generation: 7,
                    accepted_generation_digest: "application-semantics".into(),
                })
            }
        }
    }

    fn application_semantics_backend() -> Backend {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/application_state_service_semantics_v1");
        let (sender, _receiver) = crossbeam_channel::unbounded();
        Backend::new_with_semantic_bridge(
            Client::new(sender),
            &fixture,
            SemanticBridge::with_provider(Arc::new(ApplicationSemanticProvider)),
        )
    }

    #[test]
    fn application_state_service_semantics_complete_slice_action_service_operation_and_binding() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/application_state_service_semantics_v1");
        let backend = application_semantics_backend();

        let action_path = fixture.join("v1.create_todo.1o.application_action");
        let action_text = fs::read_to_string(&action_path).unwrap();
        let action_uri = Uri::from_str(&format!("file://{}", action_path.display())).unwrap();
        backend.set_open_document(&action_uri, action_text.clone(), 1);
        let slice_items = backend
            .completion_items(completion_params_at_text(&action_uri, &action_text, "InventorySlice"))
            .unwrap();
        assert!(slice_items.iter().any(|item| item.label == "InventorySlice"));

        let service_path = fixture.join("v1.inventory.1o.application_service");
        let service_text = fs::read_to_string(&service_path).unwrap();
        let service_uri = Uri::from_str(&format!("file://{}", service_path.display())).unwrap();
        backend.set_open_document(&service_uri, service_text.clone(), 1);
        let service_items = backend
            .completion_items(completion_params_at_text(&service_uri, &service_text, "type_serverd"))
            .unwrap();
        assert!(service_items.iter().any(|item| item.label == "type_serverd"));
        let binding_items = backend
            .completion_items(completion_params_at_text(&service_uri, &service_text, "remote_nodeplan"))
            .unwrap();
        assert!(binding_items.iter().any(|item| item.label == "local_uds"));
        assert!(binding_items.iter().any(|item| item.label == "remote_nodeplan"));

        let effect_path = fixture.join("v1.create_todo.1o.application_effect");
        let effect_text = fs::read_to_string(&effect_path).unwrap();
        let effect_uri = Uri::from_str(&format!("file://{}", effect_path.display())).unwrap();
        backend.set_open_document(&effect_uri, effect_text.clone(), 1);
        let action_items = backend
            .completion_items(completion_params_at_text(&effect_uri, &effect_text, "\"CreateTodo\""))
            .unwrap();
        assert!(action_items.iter().any(|item| item.label == "CreateTodo"));
        assert!(action_items.iter().any(|item| item.label == "TodoCreated"));
        let application_service_items = backend
            .completion_items(completion_params_at_text(&effect_uri, &effect_text, "\"InventoryService\""))
            .unwrap();
        assert!(application_service_items
            .iter()
            .any(|item| item.label == "InventoryService"));
        let operation_items = backend
            .completion_items(completion_params_at_text(&effect_uri, &effect_text, "\"todo.create\""))
            .unwrap();
        assert!(operation_items.iter().any(|item| item.label == "todo.create"));
        assert!(operation_items.iter().any(|item| item.label == "todo.read"));
    }

    #[test]
    fn application_state_service_semantics_diagnose_invalid_refs_and_type_mismatches() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/application_state_service_semantics_v1");
        let backend = application_semantics_backend();

        let action_path = fixture.join("v1.create_todo.1o.application_action");
        let action_uri = Uri::from_str(&format!("file://{}", action_path.display())).unwrap();
        let bad_action = r#"{
            application_action {
                name "CreateTodo"
                slice "MissingSlice"
                payload_type "TodoCreateInput"
            }
        }"#;
        let action_diagnostics = backend.document_diagnostics(&action_uri, bad_action).unwrap();
        assert!(action_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown application slice 'MissingSlice'")));

        let effect_path = fixture.join("v1.create_todo.1o.application_effect");
        let effect_uri = Uri::from_str(&format!("file://{}", effect_path.display())).unwrap();
        let bad_effect = r#"{
            application_effect {
                name "CreateTodoEffect"
                slice "InventorySlice"
                trigger_action "MissingAction"
                service "InventoryService"
                operation "todo.create"
                request_type "WrongRequest"
                response_type "WrongResponse"
                success_action "TodoCreated"
            }
        }"#;
        let effect_diagnostics = backend.document_diagnostics(&effect_uri, bad_effect).unwrap();
        assert!(effect_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown application action 'MissingAction'")));
        assert!(effect_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("request type 'WrongRequest' does not match operation 'todo.create' request type 'TodoCreateInput'")));
        assert!(effect_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("response type 'WrongResponse' does not match operation 'todo.create' response type 'Todo'")));
        assert!(effect_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("response type 'WrongResponse' does not match success action 'TodoCreated' payload type 'Todo'")));
    }

    #[test]
    fn typed_endpoint_lsp_reports_missing_operation_locally() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/typed_operation_endpoint_bridge_v1");
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let backend = Backend::new_with_semantic_bridge(
            Client::new(sender),
            &fixture,
            SemanticBridge::offline(),
        );
        let endpoint_path = fixture.join("v1.todo_local.1o.endpoint");
        let endpoint_uri = Uri::from_str(&format!("file://{}", endpoint_path.display())).unwrap();
        let invalid = r#"{ endpoint { name "todo_local" service "type_serverd" route { method "POST" path "/todos" operation "missing.create" } } }"#;
        let diagnostics = backend.document_diagnostics(&endpoint_uri, invalid).unwrap();
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.message.contains("unknown 1o operation 'missing.create'")));
    }

    fn completion_params_at_text(uri: &Uri, text: &str, value: &str) -> CompletionParams {
        let offset = text.find(value).unwrap() + 1;
        let prefix = &text[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let character = prefix
            .rsplit_once('\n')
            .map(|(_, tail)| tail.len())
            .unwrap_or(prefix.len()) as u32;
        CompletionParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_types::Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }
    }
}
