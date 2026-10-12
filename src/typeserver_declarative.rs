use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::{
    diagnostics_for_text_workspace, parse_nodeplan_workspace_link, EngineDiagnostic, EngineSeverity,
    FileRecord, Lexer, Node, NodeValue, Parser, Span, Workspace,
};
use crate::semantic::{
    SemanticContext, SemanticProvider, SemanticProviderError, SemanticValidation,
};

pub const TYPE_SERVER_SERVICE_SEMANTIC_REF: &str = "nodeplan.service";
pub const UUID_V5_NAMESPACE_RESOLUTION_V1: &str =
    "literal_uuid_or_uuid_v5_url_namespace_from_utf8_label_v1";
pub const UUID_V5_CANONICAL_INPUT_V1: &str =
    "length_prefixed_utf8_fields_in_declared_order_v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceIdentity {
    pub path: String,
    pub file_version: String,
    pub modifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TypeServerDeclarativePlan {
    pub workspace_id: String,
    pub accepted_generation: u64,
    pub accepted_generation_digest: String,
    pub source_files: Vec<SourceIdentity>,
    pub models: Vec<DeclarativeModel>,
    pub containers: Vec<DeclarativeContainer>,
}

impl TypeServerDeclarativePlan {
    pub fn deterministic_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclarativeModel {
    pub name: String,
    pub source: SourceIdentity,
    pub fields: Vec<DeclarativeField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclarativeField {
    pub name: String,
    pub semantic_type_name: String,
    pub primitive: SemanticPrimitive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SemanticPrimitive {
    Bool,
    String {
        min_length: Option<u64>,
        max_length: Option<u64>,
    },
    Integer {
        signed: bool,
        bits: u16,
        min: Option<i64>,
        max: Option<i64>,
    },
    UuidV4 {
        generation: String,
    },
    UuidV5 {
        namespace: UuidV5Namespace,
        input_fields: Vec<String>,
        canonical_input: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UuidV5Namespace {
    pub source: String,
    pub resolution: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnresolvedDeclarativeContainer {
    pub name: String,
    pub source: SourceIdentity,
    pub model: String,
    pub storage: DeclarativeStorageIntent,
    pub service: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclarativeContainer {
    pub name: String,
    pub source: SourceIdentity,
    pub model: ResolvedModelReference,
    pub storage: DeclarativeStorageIntent,
    pub service: ResolvedNodePlanServiceReference,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedModelReference {
    pub name: String,
    pub source: SourceIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedNodePlanServiceReference {
    pub service_id: String,
    pub accepted_generation: u64,
    pub accepted_generation_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclarativeStorageIntent {
    pub backend: StorageBackend,
    pub namespace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StorageBackend {
    Sqlite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDeclarativeWorkspace {
    pub models: Vec<DeclarativeModel>,
    pub containers: Vec<UnresolvedDeclarativeContainer>,
    pub source_files: Vec<SourceIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarativeDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub source: Option<String>,
    pub span: Span,
}

impl DeclarativeDiagnostic {
    fn new(
        code: &'static str,
        message: impl Into<String>,
        source: Option<String>,
        span: Span,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            source,
            span,
        }
    }
}

pub fn validate_local_declarative_workspace(
    root: impl AsRef<Path>,
) -> Result<LocalDeclarativeWorkspace, Vec<DeclarativeDiagnostic>> {
    let root = root.as_ref();
    let mut workspace = Workspace::new(root);
    workspace.reindex();

    let mut diagnostics = Vec::new();
    let mut models = Vec::new();
    let mut containers = Vec::new();
    let mut sources = BTreeSet::new();

    for record in sorted_records(workspace.index.models.values()) {
        let source = source_identity(&workspace.root, &record);
        sources.insert(source.clone());
        match read_and_validate(&workspace, &record) {
            Ok((_text, ast)) => match parse_model_ast(&ast, source) {
                Ok(model) => models.push(model),
                Err(mut errors) => diagnostics.append(&mut errors),
            },
            Err(mut errors) => diagnostics.append(&mut errors),
        }
    }

    for record in sorted_records(workspace.index.containers.values()) {
        let source = source_identity(&workspace.root, &record);
        sources.insert(source.clone());
        match read_and_validate(&workspace, &record) {
            Ok((_text, ast)) => match parse_container_ast(&ast, source) {
                Ok(container) => containers.push(container),
                Err(mut errors) => diagnostics.append(&mut errors),
            },
            Err(mut errors) => diagnostics.append(&mut errors),
        }
    }

    for record in workspace.index.files_with_modifier("nodeplan") {
        let source = source_identity(&workspace.root, &record);
        sources.insert(source.clone());
        match read_and_validate(&workspace, &record) {
            Ok((text, _ast)) => match parse_nodeplan_workspace_link(&text) {
                Ok(Some(_)) => {}
                Ok(None) => diagnostics.push(DeclarativeDiagnostic::new(
                    "nodeplan_workspace_link",
                    "nodeplan workspace link file does not contain a nodeplan declaration",
                    Some(source.path.clone()),
                    Span { start: 0, end: text.len() },
                )),
                Err(error) => diagnostics.push(DeclarativeDiagnostic::new(
                    "nodeplan_workspace_link",
                    error,
                    Some(source.path.clone()),
                    Span { start: 0, end: text.len() },
                )),
            },
            Err(mut errors) => diagnostics.append(&mut errors),
        }
    }

    models.sort_by(|left, right| left.name.cmp(&right.name));
    containers.sort_by(|left, right| left.name.cmp(&right.name));

    let mut model_names = BTreeMap::new();
    for model in &models {
        if let Some(prior) = model_names.insert(model.name.clone(), model.source.clone()) {
            diagnostics.push(DeclarativeDiagnostic::new(
                "duplicate_model",
                format!(
                    "duplicate 1o model declaration '{}' in '{}' and '{}'",
                    model.name, prior.path, model.source.path
                ),
                Some(model.source.path.clone()),
                Span { start: 0, end: 0 },
            ));
        }
    }

    for container in &containers {
        if !model_names.contains_key(&container.model) {
            diagnostics.push(DeclarativeDiagnostic::new(
                "missing_model",
                format!(
                    "unknown 1o model '{}' referenced by container '{}'",
                    container.model, container.name
                ),
                Some(container.source.path.clone()),
                Span { start: 0, end: 0 },
            ));
        }
    }

    if diagnostics.is_empty() {
        Ok(LocalDeclarativeWorkspace {
            models,
            containers,
            source_files: sources.into_iter().collect(),
        })
    } else {
        Err(diagnostics)
    }
}

pub fn build_typeserver_declarative_plan(
    root: impl AsRef<Path>,
    provider: &dyn SemanticProvider,
    context: &SemanticContext,
) -> Result<TypeServerDeclarativePlan, Vec<DeclarativeDiagnostic>> {
    let root = root.as_ref();
    let local = validate_local_declarative_workspace(root)?;
    let declared_workspace_id = read_declared_nodeplan_workspace_id(root)?;
    if declared_workspace_id != context.workspace_id {
        return Err(vec![DeclarativeDiagnostic::new(
            "nodeplan_workspace_mismatch",
            format!(
                "1o workspace links to NodePlan workspace '{}' but semantic context selected '{}'",
                declared_workspace_id, context.workspace_id
            ),
            None,
            Span { start: 0, end: 0 },
        )]);
    }
    let models_by_name = local
        .models
        .iter()
        .map(|model| (model.name.clone(), model.source.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut diagnostics = Vec::new();
    let mut containers = Vec::new();
    let mut accepted_identity: Option<(u64, String)> = None;

    for container in &local.containers {
        let Some(model_source) = models_by_name.get(&container.model) else {
            diagnostics.push(DeclarativeDiagnostic::new(
                "missing_model",
                format!(
                    "unknown 1o model '{}' referenced by container '{}'",
                    container.model, container.name
                ),
                Some(container.source.path.clone()),
                Span { start: 0, end: 0 },
            ));
            continue;
        };

        match provider.validate(TYPE_SERVER_SERVICE_SEMANTIC_REF, &container.service, context) {
            Ok(SemanticValidation::Exists {
                accepted_generation,
                accepted_generation_digest,
            }) => {
                let identity = (accepted_generation, accepted_generation_digest.clone());
                if let Some(prior) = &accepted_identity {
                    if prior != &identity {
                        diagnostics.push(DeclarativeDiagnostic::new(
                            "nodeplan_generation_changed",
                            "NodePlan accepted generation changed while constructing declarative plan",
                            Some(container.source.path.clone()),
                            Span { start: 0, end: 0 },
                        ));
                        continue;
                    }
                } else {
                    accepted_identity = Some(identity);
                }

                containers.push(DeclarativeContainer {
                    name: container.name.clone(),
                    source: container.source.clone(),
                    model: ResolvedModelReference {
                        name: container.model.clone(),
                        source: model_source.clone(),
                    },
                    storage: container.storage.clone(),
                    service: ResolvedNodePlanServiceReference {
                        service_id: container.service.clone(),
                        accepted_generation,
                        accepted_generation_digest,
                    },
                });
            }
            Ok(SemanticValidation::Missing {
                accepted_generation,
                ..
            }) => diagnostics.push(DeclarativeDiagnostic::new(
                "missing_nodeplan_service",
                format!(
                    "unknown NodePlan service '{}' in accepted generation {}",
                    container.service, accepted_generation
                ),
                Some(container.source.path.clone()),
                Span { start: 0, end: 0 },
            )),
            Err(SemanticProviderError::Unavailable(detail)
            | SemanticProviderError::Timeout(detail)) => diagnostics.push(
                DeclarativeDiagnostic::new(
                    "nodeplan_semantics_unavailable",
                    format!(
                        "NodePlan semantic validation unavailable; local schema validation remains valid: {detail}"
                    ),
                    Some(container.source.path.clone()),
                    Span { start: 0, end: 0 },
                ),
            ),
            Err(error) => diagnostics.push(DeclarativeDiagnostic::new(
                "nodeplan_semantic_error",
                format!("NodePlan semantic validation failed: {error}"),
                Some(container.source.path.clone()),
                Span { start: 0, end: 0 },
            )),
        }
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let Some((accepted_generation, accepted_generation_digest)) = accepted_identity else {
        return Err(vec![DeclarativeDiagnostic::new(
            "missing_container",
            "at least one container with a resolved NodePlan service is required",
            None,
            Span { start: 0, end: 0 },
        )]);
    };

    containers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(TypeServerDeclarativePlan {
        workspace_id: context.workspace_id.clone(),
        accepted_generation,
        accepted_generation_digest,
        source_files: local.source_files,
        models: local.models,
        containers,
    })
}

pub fn declarative_diagnostics_for_text(
    workspace: &Workspace,
    modifier: &str,
    text: &str,
) -> Vec<EngineDiagnostic> {
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
        "model" => parse_model_ast(&ast, source).err().unwrap_or_default(),
        "container" => {
            let mut diagnostics = Vec::new();
            match parse_container_ast(&ast, source) {
                Ok(container) => {
                    let known = declared_model_names(workspace);
                    if !known.contains(&container.model) {
                        diagnostics.push(DeclarativeDiagnostic::new(
                            "missing_model",
                            format!(
                                "unknown 1o model '{}' referenced by container '{}'",
                                container.model, container.name
                            ),
                            None,
                            Span { start: 0, end: text.len() },
                        ));
                    }
                }
                Err(mut errors) => diagnostics.append(&mut errors),
            }
            diagnostics
        }
        _ => Vec::new(),
    };

    diagnostics
        .into_iter()
        .map(|diagnostic| EngineDiagnostic::error(diagnostic.message, diagnostic.span))
        .collect()
}

fn read_declared_nodeplan_workspace_id(
    root: &Path,
) -> Result<String, Vec<DeclarativeDiagnostic>> {
    let mut workspace = Workspace::new(root);
    workspace.reindex();
    let links = workspace.index.files_with_modifier("nodeplan");
    if links.len() != 1 {
        return Err(vec![DeclarativeDiagnostic::new(
            "nodeplan_workspace_link",
            format!(
                "expected exactly one 1o nodeplan workspace link, found {}",
                links.len()
            ),
            None,
            Span { start: 0, end: 0 },
        )]);
    }
    let record = &links[0];
    let source = source_identity(&workspace.root, record);
    let text = fs::read_to_string(&record.path).map_err(|error| {
        vec![DeclarativeDiagnostic::new(
            "nodeplan_workspace_link",
            format!("failed to read NodePlan workspace link '{}': {error}", source.path),
            Some(source.path.clone()),
            Span { start: 0, end: 0 },
        )]
    })?;
    match parse_nodeplan_workspace_link(&text) {
        Ok(Some(link)) => Ok(link.workspace_id),
        Ok(None) => Err(vec![DeclarativeDiagnostic::new(
            "nodeplan_workspace_link",
            "nodeplan workspace link file does not contain a nodeplan declaration",
            Some(source.path),
            Span { start: 0, end: text.len() },
        )]),
        Err(error) => Err(vec![DeclarativeDiagnostic::new(
            "nodeplan_workspace_link",
            error,
            Some(source.path),
            Span { start: 0, end: text.len() },
        )]),
    }
}

fn read_and_validate(
    workspace: &Workspace,
    record: &FileRecord,
) -> Result<(String, Vec<Node>), Vec<DeclarativeDiagnostic>> {
    let source = source_identity(&workspace.root, record);
    let text = fs::read_to_string(&record.path).map_err(|error| {
        vec![DeclarativeDiagnostic::new(
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
    if !structural.is_empty() {
        return Err(structural
            .into_iter()
            .map(|diagnostic| {
                let severity = match diagnostic.severity {
                    EngineSeverity::Error => "error",
                    EngineSeverity::Warning => "warning",
                };
                DeclarativeDiagnostic::new(
                    "schema_validation",
                    format!("{severity}: {}", diagnostic.message),
                    Some(source.path.clone()),
                    diagnostic.span,
                )
            })
            .collect());
    }

    let ast = Parser::new(Lexer::new(&text).tokenize())
        .parse()
        .map_err(|error| {
            vec![DeclarativeDiagnostic::new(
                "parse",
                format!("1o parse failed: {:?}", error),
                Some(source.path.clone()),
                Span { start: 0, end: text.len() },
            )]
        })?;
    Ok((text, ast))
}

fn parse_model_ast(
    ast: &[Node],
    source: SourceIdentity,
) -> Result<DeclarativeModel, Vec<DeclarativeDiagnostic>> {
    let mut diagnostics = Vec::new();
    let Some(model_node) = ast.iter().find(|node| node.key == "model") else {
        return Err(vec![DeclarativeDiagnostic::new(
            "missing_model_root",
            "missing model declaration",
            Some(source.path.clone()),
            Span { start: 0, end: 0 },
        )]);
    };
    let Some(children) = block_children(model_node) else {
        return Err(vec![DeclarativeDiagnostic::new(
            "invalid_model_root",
            "model declaration must be a block",
            Some(source.path.clone()),
            model_node.span,
        )]);
    };
    let name = match required_string(children, "name") {
        Ok(value) => value,
        Err(message) => {
            diagnostics.push(DeclarativeDiagnostic::new(
                "model_name",
                message,
                Some(source.path.clone()),
                model_node.span,
            ));
            String::new()
        }
    };

    let mut fields = Vec::new();
    let mut seen = BTreeSet::new();
    for field_node in children.iter().filter(|node| node.key == "field") {
        match parse_field(field_node, &name, &source) {
            Ok(field) => {
                if !seen.insert(field.name.clone()) {
                    diagnostics.push(DeclarativeDiagnostic::new(
                        "duplicate_field",
                        format!("duplicate field '{}' in model '{}'", field.name, name),
                        Some(source.path.clone()),
                        field_node.span,
                    ));
                } else {
                    fields.push(field);
                }
            }
            Err(mut errors) => diagnostics.append(&mut errors),
        }
    }
    if fields.is_empty() {
        diagnostics.push(DeclarativeDiagnostic::new(
            "missing_field",
            format!("model '{}' must declare at least one field", name),
            Some(source.path.clone()),
            model_node.span,
        ));
    }
    fields.sort_by(|left, right| left.name.cmp(&right.name));

    if diagnostics.is_empty() {
        Ok(DeclarativeModel {
            name,
            source,
            fields,
        })
    } else {
        Err(diagnostics)
    }
}

fn parse_field(
    field_node: &Node,
    model_name: &str,
    source: &SourceIdentity,
) -> Result<DeclarativeField, Vec<DeclarativeDiagnostic>> {
    let Some(children) = block_children(field_node) else {
        return Err(vec![DeclarativeDiagnostic::new(
            "invalid_field",
            "field declaration must be a block",
            Some(source.path.clone()),
            field_node.span,
        )]);
    };
    let name = required_string(children, "name").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "field_name",
            message,
            Some(source.path.clone()),
            field_node.span,
        )]
    })?;
    let Some(type_node) = children.iter().find(|node| node.key == "type") else {
        return Err(vec![DeclarativeDiagnostic::new(
            "field_type",
            format!("field '{}' is missing type", name),
            Some(source.path.clone()),
            field_node.span,
        )]);
    };
    let primitive = parse_primitive(type_node, source)?;
    Ok(DeclarativeField {
        semantic_type_name: semantic_type_name(model_name, &name),
        name,
        primitive,
    })
}

fn parse_primitive(
    type_node: &Node,
    source: &SourceIdentity,
) -> Result<SemanticPrimitive, Vec<DeclarativeDiagnostic>> {
    let Some(children) = block_children(type_node) else {
        return Err(vec![DeclarativeDiagnostic::new(
            "primitive_type",
            "field type must be a primitive type block",
            Some(source.path.clone()),
            type_node.span,
        )]);
    };
    let primitives = children
        .iter()
        .filter(|node| matches!(node.key.as_str(), "uuid" | "string" | "integer" | "bool"))
        .collect::<Vec<_>>();
    if primitives.len() != 1 {
        return Err(vec![DeclarativeDiagnostic::new(
            "primitive_type",
            "field type must contain exactly one primitive declaration",
            Some(source.path.clone()),
            type_node.span,
        )]);
    }
    let primitive = primitives[0];
    match primitive.key.as_str() {
        "uuid" => parse_uuid(primitive, source),
        "string" => parse_string(primitive, source),
        "integer" => parse_integer(primitive, source),
        "bool" => Ok(SemanticPrimitive::Bool),
        _ => unreachable!(),
    }
}

fn parse_uuid(
    node: &Node,
    source: &SourceIdentity,
) -> Result<SemanticPrimitive, Vec<DeclarativeDiagnostic>> {
    let children = block_children(node).unwrap_or(&[]);
    let version = required_string(children, "version").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "uuid_version",
            message,
            Some(source.path.clone()),
            node.span,
        )]
    })?;
    let namespace = optional_string(children, "namespace");
    let input_node = children.iter().find(|child| child.key == "input");

    match version.as_str() {
        "v4" => {
            if namespace.is_some() || input_node.is_some() {
                Err(vec![DeclarativeDiagnostic::new(
                    "uuid_v4_v5_configuration",
                    "uuid v4 cannot declare v5-only namespace or deterministic input",
                    Some(source.path.clone()),
                    node.span,
                )])
            } else {
                Ok(SemanticPrimitive::UuidV4 {
                    generation: "random".into(),
                })
            }
        }
        "v5" => {
            let Some(namespace) = namespace.filter(|value| !value.trim().is_empty()) else {
                return Err(vec![DeclarativeDiagnostic::new(
                    "uuid_v5_namespace",
                    "uuid v5 requires a non-empty namespace",
                    Some(source.path.clone()),
                    node.span,
                )]);
            };
            let Some(input_node) = input_node else {
                return Err(vec![DeclarativeDiagnostic::new(
                    "uuid_v5_input",
                    "uuid v5 requires deterministic input fields",
                    Some(source.path.clone()),
                    node.span,
                )]);
            };
            let Some(input_children) = block_children(input_node) else {
                return Err(vec![DeclarativeDiagnostic::new(
                    "uuid_v5_input",
                    "uuid v5 input must be a block of field declarations",
                    Some(source.path.clone()),
                    input_node.span,
                )]);
            };
            let input_fields = input_children
                .iter()
                .filter(|child| child.key == "field")
                .filter_map(|child| match &child.value {
                    NodeValue::String(value) if !value.trim().is_empty() => Some(value.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if input_fields.is_empty() {
                return Err(vec![DeclarativeDiagnostic::new(
                    "uuid_v5_input",
                    "uuid v5 requires at least one deterministic input field",
                    Some(source.path.clone()),
                    input_node.span,
                )]);
            }
            let mut unique = BTreeSet::new();
            if input_fields.iter().any(|field| !unique.insert(field.clone())) {
                return Err(vec![DeclarativeDiagnostic::new(
                    "uuid_v5_input_duplicate",
                    "uuid v5 deterministic input fields must be unique",
                    Some(source.path.clone()),
                    input_node.span,
                )]);
            }
            Ok(SemanticPrimitive::UuidV5 {
                namespace: UuidV5Namespace {
                    source: namespace,
                    resolution: UUID_V5_NAMESPACE_RESOLUTION_V1.into(),
                },
                input_fields,
                canonical_input: UUID_V5_CANONICAL_INPUT_V1.into(),
            })
        }
        other => Err(vec![DeclarativeDiagnostic::new(
            "uuid_version",
            format!("unknown uuid version '{other}'; expected v4 or v5"),
            Some(source.path.clone()),
            node.span,
        )]),
    }
}

fn parse_string(
    node: &Node,
    source: &SourceIdentity,
) -> Result<SemanticPrimitive, Vec<DeclarativeDiagnostic>> {
    let children = block_children(node).unwrap_or(&[]);
    let min_length = optional_u64(children, "min_length");
    let max_length = optional_u64(children, "max_length");
    if let (Some(min), Some(max)) = (min_length, max_length) {
        if min > max {
            return Err(vec![DeclarativeDiagnostic::new(
                "string_length_range",
                format!("string min_length {min} cannot exceed max_length {max}"),
                Some(source.path.clone()),
                node.span,
            )]);
        }
    }
    Ok(SemanticPrimitive::String {
        min_length,
        max_length,
    })
}

fn parse_integer(
    node: &Node,
    source: &SourceIdentity,
) -> Result<SemanticPrimitive, Vec<DeclarativeDiagnostic>> {
    let children = block_children(node).unwrap_or(&[]);
    let signed = required_bool(children, "signed").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "integer_signed",
            message,
            Some(source.path.clone()),
            node.span,
        )]
    })?;
    let bits = required_i64(children, "bits").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "integer_bits",
            message,
            Some(source.path.clone()),
            node.span,
        )]
    })?;
    if !matches!(bits, 8 | 16 | 32 | 64 | 128) {
        return Err(vec![DeclarativeDiagnostic::new(
            "integer_bits",
            format!("unsupported integer bits '{bits}'; expected 8, 16, 32, 64, or 128"),
            Some(source.path.clone()),
            node.span,
        )]);
    }
    let min = optional_i64(children, "min");
    let max = optional_i64(children, "max");
    if let (Some(min), Some(max)) = (min, max) {
        if min > max {
            return Err(vec![DeclarativeDiagnostic::new(
                "integer_range",
                format!("integer min {min} cannot exceed max {max}"),
                Some(source.path.clone()),
                node.span,
            )]);
        }
    }
    Ok(SemanticPrimitive::Integer {
        signed,
        bits: bits as u16,
        min,
        max,
    })
}

fn parse_container_ast(
    ast: &[Node],
    source: SourceIdentity,
) -> Result<UnresolvedDeclarativeContainer, Vec<DeclarativeDiagnostic>> {
    let Some(container_node) = ast.iter().find(|node| node.key == "container") else {
        return Err(vec![DeclarativeDiagnostic::new(
            "missing_container_root",
            "missing container declaration",
            Some(source.path.clone()),
            Span { start: 0, end: 0 },
        )]);
    };
    let Some(children) = block_children(container_node) else {
        return Err(vec![DeclarativeDiagnostic::new(
            "invalid_container_root",
            "container declaration must be a block",
            Some(source.path.clone()),
            container_node.span,
        )]);
    };
    let name = required_string(children, "name").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "container_name",
            message,
            Some(source.path.clone()),
            container_node.span,
        )]
    })?;
    let model = required_string(children, "model").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "container_model",
            message,
            Some(source.path.clone()),
            container_node.span,
        )]
    })?;
    let service = required_string(children, "service").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "container_service",
            message,
            Some(source.path.clone()),
            container_node.span,
        )]
    })?;
    let Some(storage_node) = children.iter().find(|node| node.key == "storage") else {
        return Err(vec![DeclarativeDiagnostic::new(
            "container_storage",
            format!("container '{name}' is missing storage declaration"),
            Some(source.path.clone()),
            container_node.span,
        )]);
    };
    let Some(storage_children) = block_children(storage_node) else {
        return Err(vec![DeclarativeDiagnostic::new(
            "container_storage",
            "storage declaration must be a block",
            Some(source.path.clone()),
            storage_node.span,
        )]);
    };
    let backend = required_string(storage_children, "backend").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "storage_backend",
            message,
            Some(source.path.clone()),
            storage_node.span,
        )]
    })?;
    let backend = match backend.as_str() {
        "sqlite" => StorageBackend::Sqlite,
        other => {
            return Err(vec![DeclarativeDiagnostic::new(
                "storage_backend",
                format!("unsupported storage backend '{other}'"),
                Some(source.path.clone()),
                storage_node.span,
            )])
        }
    };
    let namespace = required_string(storage_children, "namespace").map_err(|message| {
        vec![DeclarativeDiagnostic::new(
            "storage_namespace",
            message,
            Some(source.path.clone()),
            storage_node.span,
        )]
    })?;
    if namespace.trim().is_empty() {
        return Err(vec![DeclarativeDiagnostic::new(
            "storage_namespace",
            "storage namespace cannot be empty",
            Some(source.path.clone()),
            storage_node.span,
        )]);
    }

    Ok(UnresolvedDeclarativeContainer {
        name,
        source,
        model,
        storage: DeclarativeStorageIntent { backend, namespace },
        service,
    })
}

fn declared_model_names(workspace: &Workspace) -> BTreeSet<String> {
    workspace
        .index
        .models
        .values()
        .filter_map(|record| fs::read_to_string(&record.path).ok())
        .filter_map(|text| Parser::new(Lexer::new(&text).tokenize()).parse().ok())
        .filter_map(|ast| {
            let model = ast.iter().find(|node| node.key == "model")?;
            let children = block_children(model)?;
            required_string(children, "name").ok()
        })
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

fn required_string(nodes: &[Node], key: &str) -> Result<String, String> {
    let Some(node) = nodes.iter().find(|node| node.key == key) else {
        return Err(format!("missing required string '{key}'"));
    };
    match &node.value {
        NodeValue::String(value) => Ok(value.clone()),
        _ => Err(format!("'{key}' must be a string")),
    }
}

fn optional_string(nodes: &[Node], key: &str) -> Option<String> {
    nodes.iter().find(|node| node.key == key).and_then(|node| {
        if let NodeValue::String(value) = &node.value {
            Some(value.clone())
        } else {
            None
        }
    })
}

fn required_bool(nodes: &[Node], key: &str) -> Result<bool, String> {
    let Some(node) = nodes.iter().find(|node| node.key == key) else {
        return Err(format!("missing required boolean '{key}'"));
    };
    match &node.value {
        NodeValue::Bool(value) => Ok(*value),
        _ => Err(format!("'{key}' must be a boolean")),
    }
}

fn required_i64(nodes: &[Node], key: &str) -> Result<i64, String> {
    let Some(node) = nodes.iter().find(|node| node.key == key) else {
        return Err(format!("missing required integer '{key}'"));
    };
    match &node.value {
        NodeValue::Number(value) => Ok(*value),
        _ => Err(format!("'{key}' must be an integer")),
    }
}

fn optional_i64(nodes: &[Node], key: &str) -> Option<i64> {
    nodes.iter().find(|node| node.key == key).and_then(|node| {
        if let NodeValue::Number(value) = &node.value {
            Some(*value)
        } else {
            None
        }
    })
}

fn optional_u64(nodes: &[Node], key: &str) -> Option<u64> {
    optional_i64(nodes, key).and_then(|value| u64::try_from(value).ok())
}

fn semantic_type_name(model: &str, field: &str) -> String {
    if field == "id" {
        return format!("{model}Id");
    }
    format!("{model}{}", pascal_case(field))
}

fn pascal_case(value: &str) -> String {
    value
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::{SemanticSymbol, SemanticValidation};
    use tempfile::TempDir;

    struct MockProvider {
        available: bool,
        services: BTreeSet<String>,
    }

    impl SemanticProvider for MockProvider {
        fn complete(
            &self,
            semantic_ref: &str,
            _context: &SemanticContext,
        ) -> Result<Vec<SemanticSymbol>, SemanticProviderError> {
            if !self.available {
                return Err(SemanticProviderError::Unavailable("offline".into()));
            }
            if semantic_ref != TYPE_SERVER_SERVICE_SEMANTIC_REF {
                return Err(SemanticProviderError::UnsupportedSemanticRef(
                    semantic_ref.into(),
                ));
            }
            Ok(self
                .services
                .iter()
                .cloned()
                .map(|value| SemanticSymbol {
                    value,
                    detail: "mock".into(),
                })
                .collect())
        }

        fn validate(
            &self,
            semantic_ref: &str,
            value: &str,
            _context: &SemanticContext,
        ) -> Result<SemanticValidation, SemanticProviderError> {
            if !self.available {
                return Err(SemanticProviderError::Unavailable("offline".into()));
            }
            if semantic_ref != TYPE_SERVER_SERVICE_SEMANTIC_REF {
                return Err(SemanticProviderError::UnsupportedSemanticRef(
                    semantic_ref.into(),
                ));
            }
            if self.services.contains(value) {
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
            .join("fixtures/typeserver_declarative_schema_bridge_v1")
    }

    fn source() -> SourceIdentity {
        SourceIdentity {
            path: "v1.test.1o.model".into(),
            file_version: "v1".into(),
            modifier: "model".into(),
        }
    }

    fn parse_model_text(text: &str) -> Result<DeclarativeModel, Vec<DeclarativeDiagnostic>> {
        let ast = Parser::new(Lexer::new(text).tokenize()).parse().unwrap();
        parse_model_ast(&ast, source())
    }

    #[test]
    fn todo_fixture_builds_newtype_ready_deterministic_plan() {
        let provider = MockProvider {
            available: true,
            services: ["type_serverd".to_string()].into_iter().collect(),
        };
        let context = SemanticContext {
            workspace_id: "nodeplan-real-service-pilot".into(),
        };
        let first = build_typeserver_declarative_plan(fixture_root(), &provider, &context).unwrap();
        let second = build_typeserver_declarative_plan(fixture_root(), &provider, &context).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.deterministic_json().unwrap(), second.deterministic_json().unwrap());
        assert_eq!(first.models[0].name, "Todo");
        assert_eq!(first.containers[0].name, "todos");
        assert_eq!(first.containers[0].model.name, "Todo");
        assert_eq!(first.containers[0].storage.backend, StorageBackend::Sqlite);
        assert_eq!(first.containers[0].storage.namespace, "local.todos");
        assert_eq!(first.containers[0].service.service_id, "type_serverd");
        let id = first.models[0]
            .fields
            .iter()
            .find(|field| field.name == "id")
            .unwrap();
        assert_eq!(id.semantic_type_name, "TodoId");
        assert!(matches!(&id.primitive, SemanticPrimitive::UuidV4 { .. }));
    }

    #[test]
    fn uuid_v4_is_valid() {
        let model = parse_model_text(
            r#"{ model { name "Todo" field { name "id" type { uuid { version "v4" } } } } }"#,
        )
        .unwrap();
        assert!(matches!(&model.fields[0].primitive, SemanticPrimitive::UuidV4 { .. }));
    }

    #[test]
    fn uuid_v5_with_namespace_and_deterministic_input_is_valid() {
        let model = parse_model_text(
            r#"{ model { name "Todo" field { name "id" type { uuid { version "v5" namespace "my_app.todo" input { field "account_id" field "title" } } } } } }"#,
        )
        .unwrap();
        let SemanticPrimitive::UuidV5 {
            namespace,
            input_fields,
            canonical_input,
        } = &model.fields[0].primitive else {
            panic!("expected uuid v5")
        };
        assert_eq!(namespace.source, "my_app.todo");
        assert_eq!(namespace.resolution, UUID_V5_NAMESPACE_RESOLUTION_V1);
        assert_eq!(
            input_fields.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["account_id", "title"]
        );
        assert_eq!(canonical_input, UUID_V5_CANONICAL_INPUT_V1);
    }

    #[test]
    fn uuid_v5_without_namespace_is_rejected() {
        let errors = parse_model_text(
            r#"{ model { name "Todo" field { name "id" type { uuid { version "v5" input { field "title" } } } } } }"#,
        )
        .unwrap_err();
        assert!(errors.iter().any(|error| error.code == "uuid_v5_namespace"));
    }

    #[test]
    fn uuid_v5_without_input_is_rejected() {
        let errors = parse_model_text(
            r#"{ model { name "Todo" field { name "id" type { uuid { version "v5" namespace "my_app.todo" } } } } }"#,
        )
        .unwrap_err();
        assert!(errors.iter().any(|error| error.code == "uuid_v5_input"));
    }

    #[test]
    fn uuid_v4_with_v5_only_configuration_is_rejected() {
        let errors = parse_model_text(
            r#"{ model { name "Todo" field { name "id" type { uuid { version "v4" namespace "my_app.todo" input { field "title" } } } } } }"#,
        )
        .unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.code == "uuid_v4_v5_configuration"));
    }

    #[test]
    fn unknown_uuid_version_is_rejected() {
        let errors = parse_model_text(
            r#"{ model { name "Todo" field { name "id" type { uuid { version "v7" } } } } }"#,
        )
        .unwrap_err();
        assert!(errors.iter().any(|error| error.code == "uuid_version"));
    }

    #[test]
    fn impossible_string_range_is_rejected() {
        let errors = parse_model_text(
            r#"{ model { name "Todo" field { name "title" type { string { min_length 256 max_length 1 } } } } }"#,
        )
        .unwrap_err();
        assert!(errors.iter().any(|error| error.code == "string_length_range"));
    }

    #[test]
    fn integer_contract_is_bounded_and_newtype_ready() {
        let model = parse_model_text(
            r#"{ model { name "Todo" field { name "rank" type { integer { signed false bits 32 min 0 max 100 } } } } }"#,
        )
        .unwrap();
        assert_eq!(model.fields[0].semantic_type_name, "TodoRank");
        assert!(matches!(
            &model.fields[0].primitive,
            SemanticPrimitive::Integer {
                signed: false,
                bits: 32,
                min: Some(0),
                max: Some(100)
            }
        ));
    }

    #[test]
    fn missing_model_reference_fails_cleanly() {
        let temp = TempDir::new().unwrap();
        copy_fixture(&fixture_root(), temp.path());
        fs::write(
            temp.path().join("v1.todos.1o.container"),
            r#"{ container { name "todos" model "Missing" storage { backend "sqlite" namespace "local.todos" } service "type_serverd" } }"#,
        )
        .unwrap();
        let errors = validate_local_declarative_workspace(temp.path()).unwrap_err();
        assert!(errors.iter().any(|error| error.code == "missing_model"));
    }

    #[test]
    fn missing_live_nodeplan_service_fails_semantically() {
        let provider = MockProvider {
            available: true,
            services: BTreeSet::new(),
        };
        let context = SemanticContext {
            workspace_id: "nodeplan-real-service-pilot".into(),
        };
        let errors = build_typeserver_declarative_plan(fixture_root(), &provider, &context)
            .unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.code == "missing_nodeplan_service"));
    }

    #[test]
    fn offline_provider_does_not_invalidate_local_schema() {
        let local = validate_local_declarative_workspace(fixture_root()).unwrap();
        assert_eq!(local.models[0].name, "Todo");
        let provider = MockProvider {
            available: false,
            services: BTreeSet::new(),
        };
        let context = SemanticContext {
            workspace_id: "nodeplan-real-service-pilot".into(),
        };
        let errors = build_typeserver_declarative_plan(fixture_root(), &provider, &context)
            .unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.code == "nodeplan_semantics_unavailable"));
    }

    fn copy_fixture(source: &Path, destination: &Path) {
        for entry in walkdir::WalkDir::new(source).into_iter().flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(source).unwrap();
            let target = destination.join(relative);
            if path.is_dir() {
                fs::create_dir_all(&target).unwrap();
            } else {
                fs::copy(path, target).unwrap();
            }
        }
    }
}
