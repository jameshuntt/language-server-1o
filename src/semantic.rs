use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use lsp_types::{CompletionItem, CompletionItemKind};
use serde_json::Value;

use crate::engine::{EngineDiagnostic, Node, NodeValue, Span};

pub const SEMANTIC_REF_EXTENSION: &str = "x-1o-semantic-ref";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticContext {
    pub workspace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemanticSymbol {
    pub value: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticValidation {
    Exists {
        accepted_generation: u64,
        accepted_generation_digest: String,
    },
    Missing {
        accepted_generation: u64,
        accepted_generation_digest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticProviderError {
    Unavailable(String),
    Timeout(String),
    UnsupportedSemanticRef(String),
    InvalidConfiguration(String),
}

impl fmt::Display for SemanticProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "semantic provider unavailable: {detail}"),
            Self::Timeout(detail) => write!(f, "semantic provider timeout: {detail}"),
            Self::UnsupportedSemanticRef(value) => write!(f, "unsupported semantic reference: {value}"),
            Self::InvalidConfiguration(detail) => write!(f, "invalid semantic provider configuration: {detail}"),
        }
    }
}

impl std::error::Error for SemanticProviderError {}

pub trait SemanticProvider: Send + Sync {
    fn complete(
        &self,
        semantic_ref: &str,
        context: &SemanticContext,
    ) -> Result<Vec<SemanticSymbol>, SemanticProviderError>;

    fn validate(
        &self,
        semantic_ref: &str,
        value: &str,
        context: &SemanticContext,
    ) -> Result<SemanticValidation, SemanticProviderError>;

    /// Validate within an enclosing scope (e.g. a function within the service
    /// declared in the same block). Providers that hold per-scope inventories
    /// override this; the default ignores the scope and stays flat.
    fn validate_scoped(
        &self,
        semantic_ref: &str,
        value: &str,
        _scope: Option<(&str, &str)>,
        context: &SemanticContext,
    ) -> Result<SemanticValidation, SemanticProviderError> {
        self.validate(semantic_ref, value, context)
    }

    fn refresh_identity(
        &self,
        _context: &SemanticContext,
    ) -> Result<Option<String>, SemanticProviderError> {
        Ok(None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticReference {
    pub semantic_ref: String,
    pub value: String,
    pub key: String,
    pub span: Span,
    /// The enclosing SCOPE reference, when the schema places one in the same
    /// block (or an enclosing one): a `nodeplan.function` field whose block
    /// also carries a `nodeplan.service` field validates against THAT
    /// service's functions, not the workspace-wide flat list — so a function
    /// that exists on some other service no longer slips through.
    pub scope: Option<(String, String)>,
}

#[derive(Clone, Default)]
pub struct SemanticBridge {
    provider: Option<Arc<dyn SemanticProvider>>,
}

impl SemanticBridge {
    pub fn offline() -> Self {
        Self { provider: None }
    }

    pub fn with_provider(provider: Arc<dyn SemanticProvider>) -> Self {
        Self { provider: Some(provider) }
    }

    pub fn complete(
        &self,
        semantic_ref: &str,
        context: Option<&SemanticContext>,
    ) -> Vec<CompletionItem> {
        let (Some(provider), Some(context)) = (&self.provider, context) else {
            return vec![];
        };
        let Ok(symbols) = provider.complete(semantic_ref, context) else {
            return vec![];
        };

        let mut values = symbols
            .into_iter()
            .map(|symbol| (symbol.value, symbol.detail))
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        values
            .into_iter()
            .map(|(value, detail)| CompletionItem {
                label: value,
                detail: Some(detail),
                kind: Some(CompletionItemKind::REFERENCE),
                ..Default::default()
            })
            .collect()
    }

    pub fn refresh_identity(
        &self,
        context: &SemanticContext,
    ) -> Result<Option<String>, SemanticProviderError> {
        let Some(provider) = &self.provider else {
            return Err(SemanticProviderError::Unavailable(
                "no semantic provider configured".into(),
            ));
        };
        provider.refresh_identity(context)
    }

    pub fn validate_references(
        &self,
        references: &[SemanticReference],
        context: Option<&SemanticContext>,
    ) -> Vec<EngineDiagnostic> {
        if references.is_empty() {
            return vec![];
        }

        let Some(context) = context else {
            return vec![EngineDiagnostic::warning(
                "NodePlan semantic validation unavailable; no 1o NodePlan workspace link is active",
                references[0].span,
            )];
        };
        let Some(provider) = &self.provider else {
            return vec![EngineDiagnostic::warning(
                "NodePlan semantic validation unavailable; local schema validation still active",
                references[0].span,
            )];
        };

        let mut diagnostics = Vec::new();
        let mut provider_unavailable_reported = false;
        for reference in references {
            let scope = reference
                .scope
                .as_ref()
                .map(|(scope_ref, scope_value)| (scope_ref.as_str(), scope_value.as_str()));
            match provider.validate_scoped(
                &reference.semantic_ref,
                &reference.value,
                scope,
                context,
            ) {
                Ok(SemanticValidation::Exists { .. }) => {}
                Ok(SemanticValidation::Missing {
                    accepted_generation,
                    ..
                }) => diagnostics.push(EngineDiagnostic::error(
                    match &reference.scope {
                        Some((scope_ref, scope_value)) => format!(
                            "unknown {} '{}' on {} '{}' in accepted generation {}",
                            display_semantic_ref(&reference.semantic_ref),
                            reference.value,
                            display_semantic_ref(scope_ref),
                            scope_value,
                            accepted_generation
                        ),
                        None => format!(
                            "unknown {} '{}' in accepted generation {}",
                            display_semantic_ref(&reference.semantic_ref),
                            reference.value,
                            accepted_generation
                        ),
                    },
                    reference.span,
                )),
                Err(SemanticProviderError::Unavailable(_)
                | SemanticProviderError::Timeout(_)) => {
                    if !provider_unavailable_reported {
                        diagnostics.push(EngineDiagnostic::warning(
                            "NodePlan semantic validation unavailable; local schema validation still active",
                            reference.span,
                        ));
                        provider_unavailable_reported = true;
                    }
                }
                Err(error) => diagnostics.push(EngineDiagnostic::warning(
                    format!("NodePlan semantic validation unavailable: {error}"),
                    reference.span,
                )),
            }
        }
        diagnostics
    }
}

fn display_semantic_ref(semantic_ref: &str) -> &str {
    semantic_ref.strip_prefix("nodeplan.").unwrap_or(semantic_ref)
}

pub fn semantic_references(ast: &[Node], schema: &Value) -> Vec<SemanticReference> {
    let mut out = Vec::new();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return out;
    };
    for node in ast {
        if let Some(node_schema) = properties.get(&node.key) {
            collect_node_references(node, node_schema, None, &mut out);
        }
    }
    out
}

/// The semantic ref that SCOPES others when present in the same block.
const SCOPE_SEMANTIC_REF: &str = "nodeplan.service";

fn collect_node_references(
    node: &Node,
    schema: &Value,
    scope: Option<&(String, String)>,
    out: &mut Vec<SemanticReference>,
) {
    if let (Some(semantic_ref), NodeValue::String(value)) = (
        schema.get(SEMANTIC_REF_EXTENSION).and_then(Value::as_str),
        &node.value,
    ) {
        out.push(SemanticReference {
            semantic_ref: semantic_ref.to_string(),
            value: value.clone(),
            key: node.key.clone(),
            span: node.span,
            // the scope field itself carries no scope; everything else in the
            // block validates within it.
            scope: if semantic_ref == SCOPE_SEMANTIC_REF { None } else { scope.cloned() },
        });
    }

    let NodeValue::Block(children) = &node.value else {
        return;
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    // a block that declares its own service-bound string field opens a new
    // scope for its children; otherwise the enclosing scope inherits down.
    let block_scope: Option<(String, String)> = children.iter().find_map(|child| {
        let child_schema = properties.get(&child.key)?;
        let child_ref = child_schema.get(SEMANTIC_REF_EXTENSION).and_then(Value::as_str)?;
        if child_ref != SCOPE_SEMANTIC_REF {
            return None;
        }
        match &child.value {
            NodeValue::String(value) => Some((child_ref.to_string(), value.clone())),
            _ => None,
        }
    });
    let effective = block_scope.as_ref().or(scope);
    for child in children {
        if let Some(child_schema) = properties.get(&child.key) {
            collect_node_references(child, child_schema, effective, out);
        }
    }
}

pub fn schema_for_node_stack<'a>(
    base_schema: &'a Value,
    parent_stack: &[&Node],
) -> Option<&'a Value> {
    let mut schema = base_schema;
    for node in parent_stack {
        schema = schema
            .get("properties")
            .and_then(Value::as_object)?
            .get(&node.key)?;
    }
    Some(schema)
}

pub fn semantic_ref_for_schema(schema: &Value) -> Option<&str> {
    schema.get(SEMANTIC_REF_EXTENSION).and_then(Value::as_str)
}

pub fn normalize_completion_items(items: &mut Vec<CompletionItem>) {
    items.sort_by(|left, right| left.label.cmp(&right.label));
    let mut seen = BTreeSet::new();
    items.retain(|item| seen.insert(item.label.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Lexer, Parser};
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockProvider {
        values: Mutex<Vec<String>>,
        unavailable: bool,
    }

    impl SemanticProvider for MockProvider {
        fn complete(
            &self,
            _: &str,
            _: &SemanticContext,
        ) -> Result<Vec<SemanticSymbol>, SemanticProviderError> {
            if self.unavailable {
                return Err(SemanticProviderError::Unavailable("offline".into()));
            }
            Ok(self
                .values
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .map(|value| SemanticSymbol { value, detail: "mock".into() })
                .collect())
        }

        fn validate(
            &self,
            _: &str,
            value: &str,
            _: &SemanticContext,
        ) -> Result<SemanticValidation, SemanticProviderError> {
            if self.unavailable {
                return Err(SemanticProviderError::Unavailable("offline".into()));
            }
            let exists = self.values.lock().unwrap().iter().any(|known| known == value);
            if exists {
                Ok(SemanticValidation::Exists {
                    accepted_generation: 1,
                    accepted_generation_digest: "digest".into(),
                })
            } else {
                Ok(SemanticValidation::Missing {
                    accepted_generation: 1,
                    accepted_generation_digest: "digest".into(),
                })
            }
        }
    }

    fn service_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "service": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "x-1o-semantic-ref": "nodeplan.service"}
                    },
                    "required": ["name"]
                }
            },
            "required": ["service"]
        })
    }

    #[test]
    fn semantic_reference_annotation_is_discovered_without_parser_hardcoding() {
        let ast = Parser::new(Lexer::new("{ service { name \"type_serverd\" } }").tokenize())
            .parse()
            .unwrap();
        let refs = semantic_references(&ast, &service_schema());
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].semantic_ref, "nodeplan.service");
        assert_eq!(refs[0].value, "type_serverd");
    }

    #[test]
    fn provider_backed_completion_is_deterministic_and_sorted() {
        let provider = Arc::new(MockProvider {
            values: Mutex::new(vec!["zeta".into(), "alpha".into(), "alpha".into()]),
            unavailable: false,
        });
        let bridge = SemanticBridge::with_provider(provider);
        let items = bridge.complete(
            "nodeplan.service",
            Some(&SemanticContext { workspace_id: "w".into() }),
        );
        assert_eq!(items.iter().map(|item| item.label.as_str()).collect::<Vec<_>>(), vec!["alpha", "zeta"]);
    }

    #[test]
    fn missing_symbol_becomes_semantic_error() {
        let provider = Arc::new(MockProvider {
            values: Mutex::new(vec!["type_serverd".into()]),
            unavailable: false,
        });
        let bridge = SemanticBridge::with_provider(provider);
        let reference = SemanticReference {
            semantic_ref: "nodeplan.service".into(),
            value: "missing".into(),
            key: "name".into(),
            span: Span { start: 0, end: 7 },
            scope: None,
        };
        let diagnostics = bridge.validate_references(
            &[reference],
            Some(&SemanticContext { workspace_id: "w".into() }),
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("unknown service 'missing'"));
    }

    #[test]
    fn offline_provider_degrades_to_warning_not_false_invalidity() {
        let provider = Arc::new(MockProvider {
            values: Mutex::new(vec![]),
            unavailable: true,
        });
        let bridge = SemanticBridge::with_provider(provider);
        let reference = SemanticReference {
            semantic_ref: "nodeplan.runtime".into(),
            value: "runtime-type-serverd-real".into(),
            key: "runtime".into(),
            span: Span { start: 0, end: 4 },
            scope: None,
        };
        let diagnostics = bridge.validate_references(
            &[reference],
            Some(&SemanticContext { workspace_id: "w".into() }),
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("local schema validation still active"));
        assert!(matches!(diagnostics[0].severity, crate::engine::EngineSeverity::Warning));
    }
}
