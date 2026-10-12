use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use crate::engine::{EngineDiagnostic, Lexer, Node, NodeValue, Parser, Span, Workspace};

pub const WORKSPACE_REF_APPLICATION_STATE: &str = "application_state";
pub const WORKSPACE_REF_APPLICATION_SLICE: &str = "application_slice";
pub const WORKSPACE_REF_APPLICATION_ACTION: &str = "application_action";
pub const WORKSPACE_REF_APPLICATION_SERVICE: &str = "application_service";

#[derive(Debug, Clone, PartialEq, Eq)]
struct OperationContract {
    request_type: String,
    response_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionContract {
    slice: String,
    payload_type: String,
}

pub fn application_reference_values(workspace: &Workspace, kind: &str) -> Option<Vec<String>> {
    let modifier = match kind {
        WORKSPACE_REF_APPLICATION_STATE => "application_state",
        WORKSPACE_REF_APPLICATION_SLICE => "application_slice",
        WORKSPACE_REF_APPLICATION_ACTION => "application_action",
        WORKSPACE_REF_APPLICATION_SERVICE => "application_service",
        _ => return None,
    };
    Some(declared_names(workspace, modifier))
}

pub fn application_semantics_diagnostics_for_text(
    workspace: &Workspace,
    modifier: &str,
    text: &str,
) -> Vec<EngineDiagnostic> {
    if !matches!(
        modifier,
        "application_state"
            | "application_slice"
            | "application_action"
            | "application_service"
            | "application_effect"
    ) {
        return Vec::new();
    }

    let ast = match Parser::new(Lexer::new(text).tokenize()).parse() {
        Ok(ast) => ast,
        Err(error) => return vec![EngineDiagnostic::from(error)],
    };

    match modifier {
        "application_slice" => validate_application_slice(workspace, &ast, text),
        "application_action" => validate_application_action(workspace, &ast, text),
        "application_effect" => validate_application_effect(workspace, &ast, text),
        _ => Vec::new(),
    }
}

fn validate_application_slice(
    workspace: &Workspace,
    ast: &[Node],
    text: &str,
) -> Vec<EngineDiagnostic> {
    let Some(root) = root_block(ast, "application_slice") else {
        return Vec::new();
    };
    let Some(state) = string_field(root, "state") else {
        return Vec::new();
    };
    let known = declared_names(workspace, "application_state")
        .into_iter()
        .collect::<BTreeSet<_>>();
    if known.contains(&state.0) {
        Vec::new()
    } else {
        vec![EngineDiagnostic::error(
            format!("unknown application state '{}' referenced by application slice", state.0),
            bounded_span(state.1, text),
        )]
    }
}

fn validate_application_action(
    workspace: &Workspace,
    ast: &[Node],
    text: &str,
) -> Vec<EngineDiagnostic> {
    let Some(root) = root_block(ast, "application_action") else {
        return Vec::new();
    };
    let Some(slice) = string_field(root, "slice") else {
        return Vec::new();
    };
    let known = declared_names(workspace, "application_slice")
        .into_iter()
        .collect::<BTreeSet<_>>();
    if known.contains(&slice.0) {
        Vec::new()
    } else {
        vec![EngineDiagnostic::error(
            format!("unknown application slice '{}' referenced by application action", slice.0),
            bounded_span(slice.1, text),
        )]
    }
}

fn validate_application_effect(
    workspace: &Workspace,
    ast: &[Node],
    text: &str,
) -> Vec<EngineDiagnostic> {
    let Some(root) = root_block(ast, "application_effect") else {
        return Vec::new();
    };

    let slices = declared_names(workspace, "application_slice")
        .into_iter()
        .collect::<BTreeSet<_>>();
    let services = declared_names(workspace, "application_service")
        .into_iter()
        .collect::<BTreeSet<_>>();
    let actions = declared_action_contracts(workspace);
    let operations = declared_operation_contracts(workspace);
    let mut diagnostics = Vec::new();

    if let Some((slice, span)) = string_field(root, "slice") {
        if !slices.contains(&slice) {
            diagnostics.push(EngineDiagnostic::error(
                format!("unknown application slice '{}' referenced by application effect", slice),
                bounded_span(span, text),
            ));
        }
    }

    for key in ["trigger_action", "success_action"] {
        if let Some((action, span)) = string_field(root, key) {
            if !actions.contains_key(&action) {
                diagnostics.push(EngineDiagnostic::error(
                    format!("unknown application action '{}' referenced by application effect", action),
                    bounded_span(span, text),
                ));
            }
        }
    }

    if let Some((service, span)) = string_field(root, "service") {
        if !services.contains(&service) {
            diagnostics.push(EngineDiagnostic::error(
                format!("unknown application service binding '{}' referenced by application effect", service),
                bounded_span(span, text),
            ));
        }
    }

    if let Some((operation, operation_span)) = string_field(root, "operation") {
        match operations.get(&operation) {
            None => diagnostics.push(EngineDiagnostic::error(
                format!("unknown 1o operation '{}' referenced by application effect", operation),
                bounded_span(operation_span, text),
            )),
            Some(contract) => {
                if let Some((request_type, span)) = string_field(root, "request_type") {
                    if request_type != contract.request_type {
                        diagnostics.push(EngineDiagnostic::error(
                            format!(
                                "application effect request type '{}' does not match operation '{}' request type '{}'",
                                request_type, operation, contract.request_type
                            ),
                            bounded_span(span, text),
                        ));
                    }
                }
                if let Some((response_type, span)) = string_field(root, "response_type") {
                    if response_type != contract.response_type {
                        diagnostics.push(EngineDiagnostic::error(
                            format!(
                                "application effect response type '{}' does not match operation '{}' response type '{}'",
                                response_type, operation, contract.response_type
                            ),
                            bounded_span(span, text),
                        ));
                    }
                }
            }
        }
    }

    if let (Some((success_action, _)), Some((response_type, response_span))) = (
        string_field(root, "success_action"),
        string_field(root, "response_type"),
    ) {
        if let Some(action) = actions.get(&success_action) {
            if action.payload_type != response_type {
                diagnostics.push(EngineDiagnostic::error(
                    format!(
                        "application effect response type '{}' does not match success action '{}' payload type '{}'",
                        response_type, success_action, action.payload_type
                    ),
                    bounded_span(response_span, text),
                ));
            }
            if let Some((effect_slice, slice_span)) = string_field(root, "slice") {
                if action.slice != effect_slice {
                    diagnostics.push(EngineDiagnostic::error(
                        format!(
                            "application effect slice '{}' does not match success action '{}' slice '{}'",
                            effect_slice, success_action, action.slice
                        ),
                        bounded_span(slice_span, text),
                    ));
                }
            }
        }
    }

    diagnostics
}

fn declared_names(workspace: &Workspace, modifier: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    for record in workspace.index.files_with_modifier(modifier) {
        let Ok(text) = fs::read_to_string(&record.path) else {
            continue;
        };
        let Ok(ast) = Parser::new(Lexer::new(&text).tokenize()).parse() else {
            continue;
        };
        let Some(root) = root_block(&ast, modifier) else {
            continue;
        };
        if let Some((name, _)) = string_field(root, "name") {
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

fn declared_action_contracts(workspace: &Workspace) -> BTreeMap<String, ActionContract> {
    let mut actions = BTreeMap::new();
    for record in workspace.index.files_with_modifier("application_action") {
        let Ok(text) = fs::read_to_string(&record.path) else {
            continue;
        };
        let Ok(ast) = Parser::new(Lexer::new(&text).tokenize()).parse() else {
            continue;
        };
        let Some(root) = root_block(&ast, "application_action") else {
            continue;
        };
        let (Some((name, _)), Some((slice, _)), Some((payload_type, _))) = (
            string_field(root, "name"),
            string_field(root, "slice"),
            string_field(root, "payload_type"),
        ) else {
            continue;
        };
        actions.insert(name, ActionContract { slice, payload_type });
    }
    actions
}

fn declared_operation_contracts(workspace: &Workspace) -> BTreeMap<String, OperationContract> {
    let mut operations = BTreeMap::new();
    for record in workspace.index.slices.values() {
        let Ok(text) = fs::read_to_string(&record.path) else {
            continue;
        };
        let Ok(ast) = Parser::new(Lexer::new(&text).tokenize()).parse() else {
            continue;
        };
        let Some(root) = root_block(&ast, "slice") else {
            continue;
        };
        let Some((model, _)) = string_field(root, "model") else {
            continue;
        };
        let Some(children) = block_children(root) else {
            continue;
        };
        for operation_node in children.iter().filter(|node| node.key == "operation") {
            let (Some((name, _)), Some((action, _))) = (
                string_field(operation_node, "name"),
                string_field(operation_node, "action"),
            ) else {
                continue;
            };
            let contract = match action.as_str() {
                "create" => OperationContract {
                    request_type: format!("{model}CreateInput"),
                    response_type: model.clone(),
                },
                "read" => OperationContract {
                    request_type: format!("{model}Id"),
                    response_type: model.clone(),
                },
                "update" => OperationContract {
                    request_type: format!("{model}UpdateInput"),
                    response_type: model.clone(),
                },
                "tombstone" => OperationContract {
                    request_type: format!("{model}TombstoneInput"),
                    response_type: format!("{model}Tombstone"),
                },
                _ => continue,
            };
            operations.insert(name, contract);
        }
    }
    operations
}

fn root_block<'a>(ast: &'a [Node], key: &str) -> Option<&'a Node> {
    ast.iter().find(|node| node.key == key && matches!(&node.value, NodeValue::Block(_)))
}

fn block_children(node: &Node) -> Option<&[Node]> {
    match &node.value {
        NodeValue::Block(children) => Some(children),
        _ => None,
    }
}

fn string_field(node: &Node, key: &str) -> Option<(String, Span)> {
    let children = block_children(node)?;
    let child = children.iter().find(|child| child.key == key)?;
    match &child.value {
        NodeValue::String(value) => Some((value.clone(), child.span)),
        _ => None,
    }
}

fn bounded_span(span: Span, text: &str) -> Span {
    Span {
        start: span.start.min(text.len()),
        end: span.end.min(text.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &std::path::Path, value: &str) {
        fs::write(path, value).unwrap();
    }

    fn semantic_workspace() -> TempDir {
        let temp = TempDir::new().unwrap();
        write(
            &temp.path().join("v1.inventory.1o.application_state"),
            r#"{ application_state { name "InventoryState" } }"#,
        );
        write(
            &temp.path().join("v1.inventory.1o.application_slice"),
            r#"{ application_slice { name "InventorySlice" state "InventoryState" } }"#,
        );
        write(
            &temp.path().join("v1.create_todo.1o.application_action"),
            r#"{ application_action { name "CreateTodo" slice "InventorySlice" payload_type "TodoCreateInput" } }"#,
        );
        write(
            &temp.path().join("v1.todo_created.1o.application_action"),
            r#"{ application_action { name "TodoCreated" slice "InventorySlice" payload_type "Todo" } }"#,
        );
        write(
            &temp.path().join("v1.inventory.1o.application_service"),
            r#"{ application_service { name "InventoryService" contract "InventoryService" service "type_serverd" binding "remote_nodeplan" } }"#,
        );
        write(
            &temp.path().join("v1.todo_api.1o.slice"),
            r#"{ slice { name "todo_api" model "Todo" container "todos" operation { name "todo.create" action "create" capability "inventory.write" } } }"#,
        );
        temp
    }

    #[test]
    fn invalid_application_slice_and_action_references_are_diagnosed() {
        let temp = semantic_workspace();
        let mut workspace = Workspace::new(temp.path());
        workspace.reindex();
        let slice_diagnostics = application_semantics_diagnostics_for_text(
            &workspace,
            "application_slice",
            r#"{ application_slice { name "BrokenSlice" state "MissingState" } }"#,
        );
        assert!(slice_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown application state 'MissingState'")));

        let action_diagnostics = application_semantics_diagnostics_for_text(
            &workspace,
            "application_action",
            r#"{ application_action { name "BrokenAction" slice "MissingSlice" payload_type "Todo" } }"#,
        );
        assert!(action_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown application slice 'MissingSlice'")));
    }

    #[test]
    fn effect_validates_slice_actions_service_operation_and_types() {
        let temp = semantic_workspace();
        let mut workspace = Workspace::new(temp.path());
        workspace.reindex();
        let valid = application_semantics_diagnostics_for_text(
            &workspace,
            "application_effect",
            r#"{ application_effect { name "CreateTodoEffect" slice "InventorySlice" trigger_action "CreateTodo" service "InventoryService" operation "todo.create" request_type "TodoCreateInput" response_type "Todo" success_action "TodoCreated" } }"#,
        );
        assert!(valid.is_empty(), "{valid:?}");

        let invalid = application_semantics_diagnostics_for_text(
            &workspace,
            "application_effect",
            r#"{ application_effect { name "CreateTodoEffect" slice "InventorySlice" trigger_action "CreateTodo" service "InventoryService" operation "todo.create" request_type "WrongRequest" response_type "WrongResponse" success_action "TodoCreated" } }"#,
        );
        assert!(invalid.iter().any(|diagnostic| diagnostic.message.contains("does not match operation 'todo.create' request type 'TodoCreateInput'")));
        assert!(invalid.iter().any(|diagnostic| diagnostic.message.contains("does not match operation 'todo.create' response type 'Todo'")));
        assert!(invalid.iter().any(|diagnostic| diagnostic.message.contains("does not match success action 'TodoCreated' payload type 'Todo'")));
    }
}
