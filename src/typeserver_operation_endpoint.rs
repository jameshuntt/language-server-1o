use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::{
    diagnostics_for_text_workspace, EngineDiagnostic, EngineSeverity, FileRecord, Lexer, Node,
    NodeValue, Parser, Span, Workspace,
};
use crate::semantic::{SemanticContext, SemanticProvider, SemanticProviderError, SemanticValidation};
use crate::typeserver_declarative::{
    build_typeserver_declarative_plan, SourceIdentity, TypeServerDeclarativePlan,
    TYPE_SERVER_SERVICE_SEMANTIC_REF,
};

pub const OPERATION_ENDPOINT_PLAN_SCHEMA: &str = "one_o.typeserver.typed_operation_endpoint.v1";
pub const WORKSPACE_REF_MODEL: &str = "model";
pub const WORKSPACE_REF_CONTAINER: &str = "container";
pub const WORKSPACE_REF_OPERATION: &str = "operation";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TypedOperationEndpointBundle {
    pub schema: String,
    pub resource_plan: TypeServerDeclarativePlan,
    pub operation_plan: TypedOperationPlan,
    pub endpoint_plan: TypedEndpointPlan,
}

impl TypedOperationEndpointBundle {
    pub fn deterministic_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TypedOperationPlan {
    pub workspace_id: String,
    pub accepted_generation: u64,
    pub accepted_generation_digest: String,
    pub source_files: Vec<SourceIdentity>,
    pub operations: Vec<TypedOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TypedOperation {
    pub name: String,
    pub source: SourceIdentity,
    pub action: OperationAction,
    pub model: String,
    pub container: String,
    pub capability: String,
    pub service: String,
    pub request_type: String,
    pub response_type: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum OperationAction {
    Create,
    Read,
    Update,
    Tombstone,
}

impl OperationAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Read => "read",
            Self::Update => "update",
            Self::Tombstone => "tombstone",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TypedEndpointPlan {
    pub workspace_id: String,
    pub accepted_generation: u64,
    pub accepted_generation_digest: String,
    pub endpoints: Vec<TypedEndpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TypedEndpoint {
    pub name: String,
    pub source: SourceIdentity,
    pub service: String,
    pub routes: Vec<TypedEndpointRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TypedEndpointRoute {
    pub route_id: String,
    pub method: EndpointMethod,
    pub path: String,
    pub operation: String,
    pub action: OperationAction,
    pub model: String,
    pub container: String,
    pub request_fields: Vec<crate::typeserver_declarative::DeclarativeField>,
    pub service: String,
    pub capability: String,
    pub request_type: String,
    pub response_type: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum EndpointMethod {
    #[serde(rename = "GET")]
    Get,
    #[serde(rename = "POST")]
    Post,
    #[serde(rename = "PATCH")]
    Patch,
    #[serde(rename = "DELETE")]
    Delete,
}

impl EndpointMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationEndpointDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub source: Option<String>,
    pub span: Span,
}

impl OperationEndpointDiagnostic {
    fn new(
        code: &'static str,
        message: impl Into<String>,
        source: Option<String>,
        span: Span,
    ) -> Self {
        Self { code, message: message.into(), source, span }
    }
}

#[derive(Debug, Clone)]
struct UnresolvedSlice {
    name: String,
    source: SourceIdentity,
    model: String,
    container: String,
    operations: Vec<UnresolvedOperation>,
}

#[derive(Debug, Clone)]
struct UnresolvedOperation {
    name: String,
    action: OperationAction,
    capability: String,
}

#[derive(Debug, Clone)]
struct UnresolvedEndpoint {
    name: String,
    source: SourceIdentity,
    service: String,
    routes: Vec<UnresolvedRoute>,
}

#[derive(Debug, Clone)]
struct UnresolvedRoute {
    method: EndpointMethod,
    path: String,
    operation: String,
}

pub fn build_typed_operation_endpoint_bundle(
    root: impl AsRef<Path>,
    provider: &dyn SemanticProvider,
    context: &SemanticContext,
) -> Result<TypedOperationEndpointBundle, Vec<OperationEndpointDiagnostic>> {
    let root = root.as_ref();
    let resource_plan = build_typeserver_declarative_plan(root, provider, context)
        .map_err(|diagnostics| {
            diagnostics
                .into_iter()
                .map(|diagnostic| OperationEndpointDiagnostic::new(
                    diagnostic.code,
                    diagnostic.message,
                    diagnostic.source,
                    diagnostic.span,
                ))
                .collect::<Vec<_>>()
        })?;

    let mut workspace = Workspace::new(root);
    workspace.reindex();

    let mut diagnostics = Vec::new();
    let mut slices = Vec::new();
    for record in sorted_records(workspace.index.slices.values()) {
        match read_and_validate(&workspace, &record) {
            Ok(ast) => match parse_slice_ast(&ast, source_identity(&workspace.root, &record)) {
                Ok(value) => slices.push(value),
                Err(mut errors) => diagnostics.append(&mut errors),
            },
            Err(mut errors) => diagnostics.append(&mut errors),
        }
    }

    let mut endpoints = Vec::new();
    for record in workspace.index.files_with_modifier("endpoint") {
        match read_and_validate(&workspace, &record) {
            Ok(ast) => match parse_endpoint_ast(&ast, source_identity(&workspace.root, &record)) {
                Ok(value) => endpoints.push(value),
                Err(mut errors) => diagnostics.append(&mut errors),
            },
            Err(mut errors) => diagnostics.append(&mut errors),
        }
    }

    if slices.is_empty() {
        diagnostics.push(OperationEndpointDiagnostic::new(
            "missing_slice",
            "at least one typed operation slice declaration is required",
            None,
            Span { start: 0, end: 0 },
        ));
    }
    if endpoints.is_empty() {
        diagnostics.push(OperationEndpointDiagnostic::new(
            "missing_endpoint",
            "at least one typed endpoint declaration is required",
            None,
            Span { start: 0, end: 0 },
        ));
    }

    let models = resource_plan
        .models
        .iter()
        .map(|model| model.name.clone())
        .collect::<BTreeSet<_>>();
    let containers = resource_plan
        .containers
        .iter()
        .map(|container| (container.name.clone(), container))
        .collect::<BTreeMap<_, _>>();

    let mut operations = Vec::new();
    let mut operation_names = BTreeMap::<String, SourceIdentity>::new();
    for slice in &slices {
        if !models.contains(&slice.model) {
            diagnostics.push(OperationEndpointDiagnostic::new(
                "missing_model",
                format!("unknown 1o model '{}' referenced by slice '{}'", slice.model, slice.name),
                Some(slice.source.path.clone()),
                Span { start: 0, end: 0 },
            ));
            continue;
        }
        let Some(container) = containers.get(&slice.container) else {
            diagnostics.push(OperationEndpointDiagnostic::new(
                "missing_container",
                format!(
                    "unknown 1o container '{}' referenced by slice '{}'",
                    slice.container, slice.name
                ),
                Some(slice.source.path.clone()),
                Span { start: 0, end: 0 },
            ));
            continue;
        };
        if container.model.name != slice.model {
            diagnostics.push(OperationEndpointDiagnostic::new(
                "slice_model_container_mismatch",
                format!(
                    "slice '{}' model '{}' does not match container '{}' model '{}'",
                    slice.name, slice.model, slice.container, container.model.name
                ),
                Some(slice.source.path.clone()),
                Span { start: 0, end: 0 },
            ));
            continue;
        }

        for operation in &slice.operations {
            if let Some(prior) = operation_names.insert(operation.name.clone(), slice.source.clone()) {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "duplicate_operation",
                    format!(
                        "duplicate 1o operation '{}' in '{}' and '{}'",
                        operation.name, prior.path, slice.source.path
                    ),
                    Some(slice.source.path.clone()),
                    Span { start: 0, end: 0 },
                ));
                continue;
            }
            let (request_type, response_type) = operation_types(&slice.model, operation.action);
            operations.push(TypedOperation {
                name: operation.name.clone(),
                source: slice.source.clone(),
                action: operation.action,
                model: slice.model.clone(),
                container: slice.container.clone(),
                capability: operation.capability.clone(),
                service: container.service.service_id.clone(),
                request_type,
                response_type,
            });
        }
    }
    operations.sort_by(|left, right| left.name.cmp(&right.name));
    let operations_by_name = operations
        .iter()
        .map(|operation| (operation.name.clone(), operation))
        .collect::<BTreeMap<_, _>>();

    let mut typed_endpoints = Vec::new();
    for endpoint in &endpoints {
        match provider.validate(TYPE_SERVER_SERVICE_SEMANTIC_REF, &endpoint.service, context) {
            Ok(SemanticValidation::Exists { accepted_generation, accepted_generation_digest }) => {
                if accepted_generation != resource_plan.accepted_generation
                    || accepted_generation_digest != resource_plan.accepted_generation_digest
                {
                    diagnostics.push(OperationEndpointDiagnostic::new(
                        "nodeplan_generation_changed",
                        "NodePlan accepted generation changed while constructing endpoint plan",
                        Some(endpoint.source.path.clone()),
                        Span { start: 0, end: 0 },
                    ));
                    continue;
                }
            }
            Ok(SemanticValidation::Missing { accepted_generation, .. }) => {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "missing_nodeplan_service",
                    format!(
                        "unknown NodePlan service '{}' in accepted generation {}",
                        endpoint.service, accepted_generation
                    ),
                    Some(endpoint.source.path.clone()),
                    Span { start: 0, end: 0 },
                ));
                continue;
            }
            Err(SemanticProviderError::Unavailable(detail) | SemanticProviderError::Timeout(detail)) => {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "nodeplan_semantics_unavailable",
                    format!(
                        "NodePlan semantic validation unavailable; local operation/endpoint validation remains valid: {detail}"
                    ),
                    Some(endpoint.source.path.clone()),
                    Span { start: 0, end: 0 },
                ));
                continue;
            }
            Err(error) => {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "nodeplan_semantic_error",
                    format!("NodePlan semantic validation failed: {error}"),
                    Some(endpoint.source.path.clone()),
                    Span { start: 0, end: 0 },
                ));
                continue;
            }
        }

        let mut seen_method_path = BTreeSet::new();
        let mut typed_routes = Vec::new();
        for route in &endpoint.routes {
            let key = (route.method, route.path.clone());
            if !seen_method_path.insert(key) {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "duplicate_route",
                    format!("duplicate endpoint route '{} {}'", route.method.as_str(), route.path),
                    Some(endpoint.source.path.clone()),
                    Span { start: 0, end: 0 },
                ));
                continue;
            }
            let Some(operation) = operations_by_name.get(&route.operation) else {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "missing_operation",
                    format!(
                        "unknown 1o operation '{}' referenced by endpoint '{}'",
                        route.operation, endpoint.name
                    ),
                    Some(endpoint.source.path.clone()),
                    Span { start: 0, end: 0 },
                ));
                continue;
            };
            if operation.service != endpoint.service {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "endpoint_service_mismatch",
                    format!(
                        "endpoint '{}' service '{}' does not match operation '{}' service '{}'",
                        endpoint.name, endpoint.service, operation.name, operation.service
                    ),
                    Some(endpoint.source.path.clone()),
                    Span { start: 0, end: 0 },
                ));
                continue;
            }
            if !method_matches_action(route.method, operation.action) {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "route_action_mismatch",
                    format!(
                        "route '{} {}' is incompatible with '{}' action '{}'",
                        route.method.as_str(), route.path, operation.name, operation.action.as_str()
                    ),
                    Some(endpoint.source.path.clone()),
                    Span { start: 0, end: 0 },
                ));
                continue;
            }
            typed_routes.push(TypedEndpointRoute {
                route_id: route_id(&endpoint.name, route.method, &operation.name),
                method: route.method,
                path: route.path.clone(),
                operation: operation.name.clone(),
                action: operation.action,
                model: operation.model.clone(),
                container: operation.container.clone(),
                request_fields: resource_plan
                    .models
                    .iter()
                    .find(|model| model.name == operation.model)
                    .map(|model| model.fields.clone())
                    .unwrap_or_default(),
                service: endpoint.service.clone(),
                capability: operation.capability.clone(),
                request_type: operation.request_type.clone(),
                response_type: operation.response_type.clone(),
            });
        }
        typed_routes.sort_by(|left, right| {
            (left.method, left.path.as_str(), left.operation.as_str())
                .cmp(&(right.method, right.path.as_str(), right.operation.as_str()))
        });
        typed_endpoints.push(TypedEndpoint {
            name: endpoint.name.clone(),
            source: endpoint.source.clone(),
            service: endpoint.service.clone(),
            routes: typed_routes,
        });
    }

    typed_endpoints.sort_by(|left, right| left.name.cmp(&right.name));

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let mut operation_sources = slices.into_iter().map(|slice| slice.source).collect::<Vec<_>>();
    operation_sources.sort();
    operation_sources.dedup();

    Ok(TypedOperationEndpointBundle {
        schema: OPERATION_ENDPOINT_PLAN_SCHEMA.into(),
        operation_plan: TypedOperationPlan {
            workspace_id: resource_plan.workspace_id.clone(),
            accepted_generation: resource_plan.accepted_generation,
            accepted_generation_digest: resource_plan.accepted_generation_digest.clone(),
            source_files: operation_sources,
            operations,
        },
        endpoint_plan: TypedEndpointPlan {
            workspace_id: resource_plan.workspace_id.clone(),
            accepted_generation: resource_plan.accepted_generation,
            accepted_generation_digest: resource_plan.accepted_generation_digest.clone(),
            endpoints: typed_endpoints,
        },
        resource_plan,
    })
}

pub fn declared_workspace_reference_values(workspace: &Workspace, kind: &str) -> Vec<String> {
    match kind {
        WORKSPACE_REF_MODEL => declared_names_for_modifier(workspace, "model"),
        WORKSPACE_REF_CONTAINER => declared_names_for_modifier(workspace, "container"),
        WORKSPACE_REF_OPERATION => declared_operation_names(workspace),
        _ => Vec::new(),
    }
}

fn declared_names_for_modifier(workspace: &Workspace, modifier: &str) -> Vec<String> {
    let records = match modifier {
        "model" => sorted_records(workspace.index.models.values()),
        "container" => sorted_records(workspace.index.containers.values()),
        _ => workspace.index.files_with_modifier(modifier),
    };
    let mut names = BTreeSet::new();
    for record in records {
        let Ok(text) = fs::read_to_string(&record.path) else { continue; };
        let Ok(ast) = Parser::new(Lexer::new(&text).tokenize()).parse() else { continue; };
        let Some(root) = ast.iter().find(|node| node.key == modifier) else { continue; };
        let Some(children) = block_children(root) else { continue; };
        if let Some(Node { value: NodeValue::String(value), .. }) = children.iter().find(|node| node.key == "name") {
            names.insert(value.clone());
        }
    }
    names.into_iter().collect()
}

pub fn declared_operation_names(workspace: &Workspace) -> Vec<String> {
    let mut names = BTreeSet::new();
    for record in workspace.index.slices.values() {
        let Ok(text) = fs::read_to_string(&record.path) else { continue; };
        let Ok(ast) = Parser::new(Lexer::new(&text).tokenize()).parse() else { continue; };
        let source = source_identity(&workspace.root, record);
        let Ok(slice) = parse_slice_ast(&ast, source) else { continue; };
        for operation in slice.operations {
            names.insert(operation.name);
        }
    }
    names.into_iter().collect()
}

pub fn operation_endpoint_diagnostics_for_text(
    workspace: &Workspace,
    modifier: &str,
    text: &str,
) -> Vec<EngineDiagnostic> {
    if modifier != "slice" && modifier != "endpoint" {
        return Vec::new();
    }
    let ast = match Parser::new(Lexer::new(text).tokenize()).parse() {
        Ok(ast) => ast,
        Err(error) => return vec![EngineDiagnostic::from(error)],
    };
    let source = SourceIdentity {
        path: "<open-document>".into(),
        file_version: "v1".into(),
        modifier: modifier.into(),
    };
    let diagnostics = match modifier {
        "slice" => match parse_slice_ast(&ast, source) {
            Ok(slice) => {
                let mut diagnostics = Vec::new();
                let models = declared_workspace_reference_values(workspace, WORKSPACE_REF_MODEL)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let containers = declared_workspace_reference_values(workspace, WORKSPACE_REF_CONTAINER)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                if !models.contains(&slice.model) {
                    diagnostics.push(OperationEndpointDiagnostic::new(
                        "missing_model",
                        format!("unknown 1o model '{}' referenced by slice '{}'", slice.model, slice.name),
                        None,
                        Span { start: 0, end: text.len() },
                    ));
                }
                if !containers.contains(&slice.container) {
                    diagnostics.push(OperationEndpointDiagnostic::new(
                        "missing_container",
                        format!(
                            "unknown 1o container '{}' referenced by slice '{}'",
                            slice.container, slice.name
                        ),
                        None,
                        Span { start: 0, end: text.len() },
                    ));
                }
                diagnostics
            }
            Err(errors) => errors,
        },
        "endpoint" => match parse_endpoint_ast(&ast, source) {
            Ok(endpoint) => {
                let known = declared_operation_names(workspace).into_iter().collect::<BTreeSet<_>>();
                endpoint
                    .routes
                    .iter()
                    .filter(|route| !known.contains(&route.operation))
                    .map(|route| OperationEndpointDiagnostic::new(
                        "missing_operation",
                        format!(
                            "unknown 1o operation '{}' referenced by endpoint '{}'",
                            route.operation, endpoint.name
                        ),
                        None,
                        Span { start: 0, end: text.len() },
                    ))
                    .collect()
            }
            Err(errors) => errors,
        },
        _ => Vec::new(),
    };

    diagnostics
        .into_iter()
        .map(|diagnostic| EngineDiagnostic::error(diagnostic.message, diagnostic.span))
        .collect()
}

fn read_and_validate(
    workspace: &Workspace,
    record: &FileRecord,
) -> Result<Vec<Node>, Vec<OperationEndpointDiagnostic>> {
    let source = source_identity(&workspace.root, record);
    let text = fs::read_to_string(&record.path).map_err(|error| {
        vec![OperationEndpointDiagnostic::new(
            "source_read",
            format!("failed to read '{}': {error}", source.path),
            Some(source.path.clone()),
            Span { start: 0, end: 0 },
        )]
    })?;
    let structural = diagnostics_for_text_workspace(
        workspace,
        &record.modifier,
        &record.version,
        &text,
    );
    let errors = structural
        .into_iter()
        .filter(|diagnostic| matches!(diagnostic.severity, EngineSeverity::Error))
        .map(|diagnostic| OperationEndpointDiagnostic::new(
            "schema_validation",
            diagnostic.message,
            Some(source.path.clone()),
            diagnostic.span,
        ))
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        return Err(errors);
    }
    Parser::new(Lexer::new(&text).tokenize())
        .parse()
        .map_err(|error| vec![OperationEndpointDiagnostic::new(
            "parse",
            EngineDiagnostic::from(error).message,
            Some(source.path),
            Span { start: 0, end: text.len() },
        )])
}

fn parse_slice_ast(
    ast: &[Node],
    source: SourceIdentity,
) -> Result<UnresolvedSlice, Vec<OperationEndpointDiagnostic>> {
    let Some(slice_node) = ast.iter().find(|node| node.key == "slice") else {
        return Err(vec![OperationEndpointDiagnostic::new(
            "slice_declaration",
            "missing top-level slice declaration",
            Some(source.path),
            Span { start: 0, end: 0 },
        )]);
    };
    let Some(children) = block_children(slice_node) else {
        return Err(vec![OperationEndpointDiagnostic::new(
            "slice_declaration",
            "slice declaration must be a block",
            Some(source.path),
            slice_node.span,
        )]);
    };
    let name = req_string(children, "name", &source, slice_node.span)?;
    let model = req_string(children, "model", &source, slice_node.span)?;
    let container = req_string(children, "container", &source, slice_node.span)?;
    let mut operations = Vec::new();
    let mut names = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for operation_node in children.iter().filter(|node| node.key == "operation") {
        let Some(operation_children) = block_children(operation_node) else {
            diagnostics.push(OperationEndpointDiagnostic::new(
                "operation_declaration",
                "operation declaration must be a block",
                Some(source.path.clone()),
                operation_node.span,
            ));
            continue;
        };
        let operation_name = match req_string(operation_children, "name", &source, operation_node.span) {
            Ok(value) => value,
            Err(mut errors) => { diagnostics.append(&mut errors); continue; }
        };
        if !names.insert(operation_name.clone()) {
            diagnostics.push(OperationEndpointDiagnostic::new(
                "duplicate_operation",
                format!("duplicate operation name '{operation_name}'"),
                Some(source.path.clone()),
                operation_node.span,
            ));
            continue;
        }
        let action_text = match req_string(operation_children, "action", &source, operation_node.span) {
            Ok(value) => value,
            Err(mut errors) => { diagnostics.append(&mut errors); continue; }
        };
        let action = match action_text.as_str() {
            "create" => OperationAction::Create,
            "read" => OperationAction::Read,
            "update" => OperationAction::Update,
            "tombstone" => OperationAction::Tombstone,
            other => {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "unsupported_action",
                    format!("unsupported operation action '{other}'"),
                    Some(source.path.clone()),
                    operation_node.span,
                ));
                continue;
            }
        };
        let capability = match req_string(operation_children, "capability", &source, operation_node.span) {
            Ok(value) if !value.trim().is_empty() => value,
            Ok(_) => {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "operation_capability",
                    "operation capability cannot be empty",
                    Some(source.path.clone()),
                    operation_node.span,
                ));
                continue;
            }
            Err(mut errors) => { diagnostics.append(&mut errors); continue; }
        };
        operations.push(UnresolvedOperation { name: operation_name, action, capability });
    }
    if operations.is_empty() {
        diagnostics.push(OperationEndpointDiagnostic::new(
            "missing_operation",
            "slice must declare at least one operation",
            Some(source.path.clone()),
            slice_node.span,
        ));
    }
    if diagnostics.is_empty() {
        Ok(UnresolvedSlice { name, source, model, container, operations })
    } else {
        Err(diagnostics)
    }
}

fn parse_endpoint_ast(
    ast: &[Node],
    source: SourceIdentity,
) -> Result<UnresolvedEndpoint, Vec<OperationEndpointDiagnostic>> {
    let Some(endpoint_node) = ast.iter().find(|node| node.key == "endpoint") else {
        return Err(vec![OperationEndpointDiagnostic::new(
            "endpoint_declaration",
            "missing top-level endpoint declaration",
            Some(source.path),
            Span { start: 0, end: 0 },
        )]);
    };
    let Some(children) = block_children(endpoint_node) else {
        return Err(vec![OperationEndpointDiagnostic::new(
            "endpoint_declaration",
            "endpoint declaration must be a block",
            Some(source.path),
            endpoint_node.span,
        )]);
    };
    let name = req_string(children, "name", &source, endpoint_node.span)?;
    let service = req_string(children, "service", &source, endpoint_node.span)?;
    let mut routes = Vec::new();
    let mut method_paths = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for route_node in children.iter().filter(|node| node.key == "route") {
        let Some(route_children) = block_children(route_node) else {
            diagnostics.push(OperationEndpointDiagnostic::new(
                "route_declaration",
                "route declaration must be a block",
                Some(source.path.clone()),
                route_node.span,
            ));
            continue;
        };
        let method_text = match req_string(route_children, "method", &source, route_node.span) {
            Ok(value) => value,
            Err(mut errors) => { diagnostics.append(&mut errors); continue; }
        };
        let method = match method_text.as_str() {
            "GET" => EndpointMethod::Get,
            "POST" => EndpointMethod::Post,
            "PATCH" => EndpointMethod::Patch,
            "DELETE" => EndpointMethod::Delete,
            other => {
                diagnostics.push(OperationEndpointDiagnostic::new(
                    "unsupported_method",
                    format!("unsupported endpoint method '{other}'"),
                    Some(source.path.clone()),
                    route_node.span,
                ));
                continue;
            }
        };
        let path = match req_string(route_children, "path", &source, route_node.span) {
            Ok(value) => value,
            Err(mut errors) => { diagnostics.append(&mut errors); continue; }
        };
        if let Err(reason) = validate_route_path(&path) {
            diagnostics.push(OperationEndpointDiagnostic::new(
                "malformed_route_path",
                reason,
                Some(source.path.clone()),
                route_node.span,
            ));
            continue;
        }
        if !method_paths.insert((method, path.clone())) {
            diagnostics.push(OperationEndpointDiagnostic::new(
                "duplicate_route",
                format!("duplicate endpoint route '{} {}'", method.as_str(), path),
                Some(source.path.clone()),
                route_node.span,
            ));
            continue;
        }
        let operation = match req_string(route_children, "operation", &source, route_node.span) {
            Ok(value) => value,
            Err(mut errors) => { diagnostics.append(&mut errors); continue; }
        };
        routes.push(UnresolvedRoute { method, path, operation });
    }
    if routes.is_empty() {
        diagnostics.push(OperationEndpointDiagnostic::new(
            "missing_route",
            "endpoint must declare at least one route",
            Some(source.path.clone()),
            endpoint_node.span,
        ));
    }
    if diagnostics.is_empty() {
        Ok(UnresolvedEndpoint { name, source, service, routes })
    } else {
        Err(diagnostics)
    }
}

fn req_string(
    nodes: &[Node],
    key: &str,
    source: &SourceIdentity,
    span: Span,
) -> Result<String, Vec<OperationEndpointDiagnostic>> {
    let Some(node) = nodes.iter().find(|node| node.key == key) else {
        return Err(vec![OperationEndpointDiagnostic::new(
            "required_string",
            format!("missing required string '{key}'"),
            Some(source.path.clone()),
            span,
        )]);
    };
    match &node.value {
        NodeValue::String(value) => Ok(value.clone()),
        _ => Err(vec![OperationEndpointDiagnostic::new(
            "required_string",
            format!("'{key}' must be a string"),
            Some(source.path.clone()),
            node.span,
        )]),
    }
}

fn operation_types(model: &str, action: OperationAction) -> (String, String) {
    match action {
        OperationAction::Create => (format!("{model}CreateInput"), model.to_string()),
        OperationAction::Read => (format!("{model}Id"), model.to_string()),
        OperationAction::Update => (format!("{model}UpdateInput"), model.to_string()),
        OperationAction::Tombstone => (format!("{model}TombstoneInput"), format!("{model}Tombstone")),
    }
}

fn method_matches_action(method: EndpointMethod, action: OperationAction) -> bool {
    matches!(
        (method, action),
        (EndpointMethod::Post, OperationAction::Create)
            | (EndpointMethod::Get, OperationAction::Read)
            | (EndpointMethod::Patch, OperationAction::Update)
            | (EndpointMethod::Delete, OperationAction::Tombstone)
    )
}

fn validate_route_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.chars().any(char::is_whitespace) || path.contains("//") {
        return Err(format!("malformed route path '{path}'"));
    }
    for segment in path.split('/').skip(1) {
        if segment.is_empty() {
            return Err(format!("malformed route path '{path}'"));
        }
        if segment.contains(':') && (!segment.starts_with(':') || segment.len() == 1) {
            return Err(format!("malformed route path parameter in '{path}'"));
        }
    }
    Ok(())
}

fn route_id(endpoint: &str, method: EndpointMethod, operation: &str) -> String {
    format!(
        "one_o.{}.{}.{}",
        safe_id(endpoint),
        method.as_str().to_ascii_lowercase(),
        safe_id(operation)
    )
}

fn safe_id(value: &str) -> String {
    value
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect()
}

fn sorted_records<'a>(records: impl Iterator<Item = &'a FileRecord>) -> Vec<FileRecord> {
    let mut records = records.cloned().collect::<Vec<_>>();
    records.sort_by(|left, right| left.path.cmp(&right.path));
    records
}

fn source_identity(root: &Path, record: &FileRecord) -> SourceIdentity {
    let relative = record.path.strip_prefix(root).unwrap_or(&record.path);
    SourceIdentity {
        path: normalize_path(relative),
        file_version: record.version.clone(),
        modifier: record.modifier.clone(),
    }
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn block_children(node: &Node) -> Option<&[Node]> {
    match &node.value {
        NodeValue::Block(children) => Some(children),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::SemanticSymbol;

    struct MockProvider;

    impl SemanticProvider for MockProvider {
        fn complete(
            &self,
            semantic_ref: &str,
            _context: &SemanticContext,
        ) -> Result<Vec<SemanticSymbol>, SemanticProviderError> {
            if semantic_ref != TYPE_SERVER_SERVICE_SEMANTIC_REF {
                return Err(SemanticProviderError::UnsupportedSemanticRef(semantic_ref.into()));
            }
            Ok(vec![SemanticSymbol { value: "type_serverd".into(), detail: "mock".into() }])
        }

        fn validate(
            &self,
            semantic_ref: &str,
            value: &str,
            _context: &SemanticContext,
        ) -> Result<SemanticValidation, SemanticProviderError> {
            if semantic_ref != TYPE_SERVER_SERVICE_SEMANTIC_REF {
                return Err(SemanticProviderError::UnsupportedSemanticRef(semantic_ref.into()));
            }
            if value == "type_serverd" {
                Ok(SemanticValidation::Exists {
                    accepted_generation: 1,
                    accepted_generation_digest: "digest-1".into(),
                })
            } else {
                Ok(SemanticValidation::Missing {
                    accepted_generation: 1,
                    accepted_generation_digest: "digest-1".into(),
                })
            }
        }
    }

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/typed_operation_endpoint_bridge_v1")
    }

    #[test]
    fn todo_operations_and_endpoint_build_deterministically() {
        let context = SemanticContext { workspace_id: "nodeplan-real-service-pilot".into() };
        let first = build_typed_operation_endpoint_bundle(fixture_root(), &MockProvider, &context).unwrap();
        let second = build_typed_operation_endpoint_bundle(fixture_root(), &MockProvider, &context).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.deterministic_json().unwrap(), second.deterministic_json().unwrap());
        assert_eq!(first.operation_plan.operations.len(), 2);
        assert_eq!(first.endpoint_plan.endpoints[0].routes.len(), 2);
        assert!(first.operation_plan.operations.iter().any(|operation| {
            operation.name == "todo.create"
                && operation.capability == "todo.write"
                && operation.request_type == "TodoCreateInput"
        }));
        assert!(first.operation_plan.operations.iter().any(|operation| {
            operation.name == "todo.read"
                && operation.capability == "todo.read"
                && operation.request_type == "TodoId"
        }));
    }

    #[test]
    fn crud_lifecycle_operations_and_routes_resolve_deterministically() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/crud_lifecycle_bridge_v1");
        let context = SemanticContext { workspace_id: "nodeplan-real-service-pilot".into() };
        let first = build_typed_operation_endpoint_bundle(&root, &MockProvider, &context).unwrap();
        let second = build_typed_operation_endpoint_bundle(&root, &MockProvider, &context).unwrap();
        assert_eq!(first.deterministic_json().unwrap(), second.deterministic_json().unwrap());
        assert_eq!(first.operation_plan.operations.len(), 4);
        assert_eq!(first.endpoint_plan.endpoints[0].routes.len(), 4);
        let update = first.operation_plan.operations.iter().find(|op| op.name == "todo.update").unwrap();
        assert_eq!(update.action, OperationAction::Update);
        assert_eq!(update.capability, "todo.write");
        assert_eq!(update.request_type, "TodoUpdateInput");
        let tombstone = first.operation_plan.operations.iter().find(|op| op.name == "todo.delete").unwrap();
        assert_eq!(tombstone.action, OperationAction::Tombstone);
        assert_eq!(tombstone.capability, "todo.delete");
        assert_eq!(tombstone.request_type, "TodoTombstoneInput");
        assert!(first.endpoint_plan.endpoints[0].routes.iter().any(|route| {
            route.method == EndpointMethod::Patch && route.operation == "todo.update"
        }));
        assert!(first.endpoint_plan.endpoints[0].routes.iter().any(|route| {
            route.method == EndpointMethod::Delete && route.operation == "todo.delete"
        }));
    }

    #[test]
    fn missing_operation_is_rejected() {
        let temp = tempfile::TempDir::new().unwrap();
        copy_fixture(&fixture_root(), temp.path());
        fs::write(
            temp.path().join("v1.todo_local.1o.endpoint"),
            r#"{ endpoint { name "todo_local" service "type_serverd" route { method "POST" path "/todos" operation "missing.create" } } }"#,
        ).unwrap();
        let context = SemanticContext { workspace_id: "nodeplan-real-service-pilot".into() };
        let errors = build_typed_operation_endpoint_bundle(temp.path(), &MockProvider, &context).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "missing_operation"));
    }

    #[test]
    fn duplicate_exact_route_is_rejected() {
        let temp = tempfile::TempDir::new().unwrap();
        copy_fixture(&fixture_root(), temp.path());
        fs::write(
            temp.path().join("v1.todo_local.1o.endpoint"),
            r#"{ endpoint { name "todo_local" service "type_serverd" route { method "POST" path "/todos" operation "todo.create" } route { method "POST" path "/todos" operation "todo.create" } } }"#,
        ).unwrap();
        let context = SemanticContext { workspace_id: "nodeplan-real-service-pilot".into() };
        let errors = build_typed_operation_endpoint_bundle(temp.path(), &MockProvider, &context).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "duplicate_route"));
    }

    #[test]
    fn unsupported_action_and_malformed_path_are_rejected() {
        let slice_source = SourceIdentity { path: "slice".into(), file_version: "v1".into(), modifier: "slice".into() };
        let slice_ast = Parser::new(Lexer::new(r#"{ slice { name "x" model "Todo" container "todos" operation { name "x" action "delete" capability "x" } } }"#).tokenize()).parse().unwrap();
        assert!(parse_slice_ast(&slice_ast, slice_source).unwrap_err().iter().any(|error| error.code == "unsupported_action"));
        let endpoint_source = SourceIdentity { path: "endpoint".into(), file_version: "v1".into(), modifier: "endpoint".into() };
        let endpoint_ast = Parser::new(Lexer::new(r#"{ endpoint { name "x" service "type_serverd" route { method "PUT" path "/todos" operation "todo.read" } } }"#).tokenize()).parse().unwrap();
        assert!(parse_endpoint_ast(&endpoint_ast, endpoint_source).unwrap_err().iter().any(|error| error.code == "unsupported_method"));
        assert!(validate_route_path("todos").is_err());
        assert!(validate_route_path("/todos/:id").is_ok());
    }

    #[test]
    fn missing_container_duplicate_operation_and_action_mismatch_are_rejected() {
        let context = SemanticContext { workspace_id: "nodeplan-real-service-pilot".into() };

        let temp = tempfile::TempDir::new().unwrap();
        copy_fixture(&fixture_root(), temp.path());
        fs::write(
            temp.path().join("v1.todo_api.1o.slice"),
            r#"{ slice { name "todo_api" model "Missing" container "todos" operation { name "todo.create" action "create" capability "todo.write" } } }"#,
        ).unwrap();
        let errors = build_typed_operation_endpoint_bundle(temp.path(), &MockProvider, &context).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "missing_model"));

        let temp = tempfile::TempDir::new().unwrap();
        copy_fixture(&fixture_root(), temp.path());
        fs::write(
            temp.path().join("v1.todo_api.1o.slice"),
            r#"{ slice { name "todo_api" model "Todo" container "missing" operation { name "todo.create" action "create" capability "todo.write" } } }"#,
        ).unwrap();
        let errors = build_typed_operation_endpoint_bundle(temp.path(), &MockProvider, &context).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "missing_container"));

        let temp = tempfile::TempDir::new().unwrap();
        copy_fixture(&fixture_root(), temp.path());
        fs::write(
            temp.path().join("v1.todo_api.1o.slice"),
            r#"{ slice { name "todo_api" model "Todo" container "todos" operation { name "todo.create" action "create" capability "todo.write" } operation { name "todo.create" action "create" capability "todo.write" } } }"#,
        ).unwrap();
        let errors = build_typed_operation_endpoint_bundle(temp.path(), &MockProvider, &context).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "duplicate_operation"));

        let temp = tempfile::TempDir::new().unwrap();
        copy_fixture(&fixture_root(), temp.path());
        fs::write(
            temp.path().join("v1.todo_local.1o.endpoint"),
            r#"{ endpoint { name "todo_local" service "type_serverd" route { method "GET" path "/todos" operation "todo.create" } route { method "GET" path "/todos/:id" operation "todo.read" } } }"#,
        ).unwrap();
        let errors = build_typed_operation_endpoint_bundle(temp.path(), &MockProvider, &context).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "route_action_mismatch"));
    }

    #[test]
    fn offline_nodeplan_truth_blocks_new_live_endpoint_plan_without_breaking_local_parsing() {
        struct OfflineProvider;
        impl SemanticProvider for OfflineProvider {
            fn complete(&self, _semantic_ref: &str, _context: &SemanticContext) -> Result<Vec<SemanticSymbol>, SemanticProviderError> {
                Err(SemanticProviderError::Unavailable("registry offline".into()))
            }
            fn validate(&self, _semantic_ref: &str, _value: &str, _context: &SemanticContext) -> Result<SemanticValidation, SemanticProviderError> {
                Err(SemanticProviderError::Unavailable("registry offline".into()))
            }
        }
        let mut workspace = Workspace::new(fixture_root());
        workspace.reindex();
        let slice_text = fs::read_to_string(fixture_root().join("v1.todo_api.1o.slice")).unwrap();
        assert!(operation_endpoint_diagnostics_for_text(&workspace, "slice", &slice_text).is_empty());
        let context = SemanticContext { workspace_id: "nodeplan-real-service-pilot".into() };
        let errors = build_typed_operation_endpoint_bundle(fixture_root(), &OfflineProvider, &context).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "nodeplan_semantics_unavailable"));
    }

    fn copy_fixture(source: &Path, destination: &Path) {
        for entry in walkdir::WalkDir::new(source).into_iter().flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(source).unwrap();
            let target = destination.join(relative);
            if path.is_dir() {
                fs::create_dir_all(&target).unwrap();
            } else {
                if let Some(parent) = target.parent() { fs::create_dir_all(parent).unwrap(); }
                fs::copy(path, target).unwrap();
            }
        }
    }
}
