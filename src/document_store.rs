use std::collections::HashMap;
use std::sync::RwLock;

use lsp_types::Uri;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDocument {
    pub text: String,
    pub version: i32,
}

#[derive(Debug, Default)]
pub struct DocumentStore {
    documents: RwLock<HashMap<String, OpenDocument>>,
}

impl DocumentStore {
    pub fn open(&self, uri: &Uri, text: String, version: i32) {
        self.documents
            .write()
            .expect("document store write lock")
            .insert(uri.as_str().to_string(), OpenDocument { text, version });
    }

    pub fn change(&self, uri: &Uri, text: String, version: i32) {
        self.open(uri, text, version);
    }

    pub fn close(&self, uri: &Uri) {
        self.documents
            .write()
            .expect("document store write lock")
            .remove(uri.as_str());
    }

    pub fn text(&self, uri: &Uri) -> Option<String> {
        self.documents
            .read()
            .expect("document store read lock")
            .get(uri.as_str())
            .map(|document| document.text.clone())
    }

    pub fn version(&self, uri: &Uri) -> Option<i32> {
        self.documents
            .read()
            .expect("document store read lock")
            .get(uri.as_str())
            .map(|document| document.version)
    }

    pub fn contains(&self, uri: &Uri) -> bool {
        self.documents
            .read()
            .expect("document store read lock")
            .contains_key(uri.as_str())
    }

    pub fn entries(&self) -> Vec<(String, OpenDocument)> {
        self.documents
            .read()
            .expect("document store read lock")
            .iter()
            .map(|(uri, document)| (uri.clone(), document.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn did_change_store_replaces_full_in_memory_text() {
        let store = DocumentStore::default();
        let uri = Uri::from_str("file:///tmp/v1.example.1o.service").unwrap();
        store.open(&uri, "old text".into(), 1);
        store.change(&uri, "entire new unsaved document".into(), 2);
        assert_eq!(store.text(&uri).as_deref(), Some("entire new unsaved document"));
        assert_eq!(store.version(&uri), Some(2));
    }
}
