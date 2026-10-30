use std::sync::Arc;

use indexmap::IndexMap;

use crate::Value;

/// An ordered set of named fields with an optional nominal type.
///
/// The type tag is what turns a bag of fields into an object. `ls` emits records tagged
/// `fs.entry`, `ps` emits `proc`. Commands like `rm` and `kill` declare which tags they
/// accept, and method calls such as `$p.kill()` are resolved through the tag.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Record {
    pub type_name: Option<Arc<str>>,
    /// Identity of the thing the record describes, such as `file:<dev>:<inode>:<size>:<mtime>`
    /// or `proc:<pid>:<start>`. Methods compare it with the live object before they act, so a
    /// snapshot can never hit a different file or a reused pid. Tables do not show it.
    pub id: Option<Arc<str>>,
    pub fields: IndexMap<String, Value>,
}

impl Record {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn typed(name: &str) -> Self {
        Record { type_name: Some(name.into()), id: None, fields: IndexMap::new() }
    }

    /// Builder style insert, handy for commands that construct their output records.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(key.to_string(), value.into());
        self
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.fields.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    pub fn type_name(&self) -> Option<&str> {
        self.type_name.as_deref()
    }

    pub fn with_id(mut self, id: impl Into<Arc<str>>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
}
