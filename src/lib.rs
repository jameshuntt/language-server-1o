#![forbid(unsafe_code)]
// Preserve crate::engine and crate::utils call sites while making core_1o
// the only parser/schema implementation.
pub use core_1o::{engine, schema_definitions, utils};

pub mod application_semantics;
pub mod document_store;
pub mod lsp;
pub mod semantic;
pub mod typeserver_declarative;
pub mod typeserver_operation_endpoint;
