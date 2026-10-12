//! MCP tools contributed by command families. A tool has typed arguments
//! (deserialised from the call's `arguments`) and a serialisable result, and
//! usually calls the same library function as the matching command.
//!
//! Input schemas are hand-written JSON Schema: no schema-derivation crate.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::ctx::Ctx;
use crate::error::Error;

pub(crate) type CallFuture = Pin<Box<dyn Future<Output = Result<Value, CallError>> + Send>>;
type Handler = dyn Fn(Arc<Ctx>, Value) -> CallFuture + Send + Sync;

pub(crate) enum CallError {
    /// Arguments did not match the tool's input type (JSON-RPC -32602).
    InvalidParams(String),
    /// The tool ran and failed (an `isError` result).
    Tool(Error),
}

#[derive(Clone)]
pub struct Tool {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
    read_only: bool,
    destructive: bool,
    open_world: bool,
    handler: Arc<Handler>,
}

impl Tool {
    pub fn new<A, R, F, Fut>(
        name: &'static str,
        description: &'static str,
        input_schema: Value,
        f: F,
    ) -> Self
    where
        A: DeserializeOwned,
        R: Serialize,
        F: Fn(Arc<Ctx>, A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R, Error>> + Send + 'static,
    {
        let f = Arc::new(f);
        Self {
            name,
            description,
            input_schema,
            read_only: false,
            destructive: true,
            open_world: true,
            handler: Arc::new(move |ctx, args| {
                let f = f.clone();
                Box::pin(async move {
                    let args = serde_json::from_value(args)
                        .map_err(|e| CallError::InvalidParams(e.to_string()))?;
                    let result = f(ctx, args).await.map_err(CallError::Tool)?;
                    serde_json::to_value(result).map_err(|e| CallError::Tool(e.into()))
                })
            }),
        }
    }

    /// Mark the tool as having no side effects (MCP `readOnlyHint`).
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self.destructive = false;
        self
    }

    /// Mark operations that execute commands or change remote state.
    pub fn destructive_open_world(mut self) -> Self {
        self.destructive = true;
        self.open_world = true;
        self
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn describe(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
            "annotations": {"readOnlyHint": self.read_only, "destructiveHint": self.destructive, "openWorldHint": self.open_world},
        })
    }

    pub(crate) fn call(&self, ctx: Arc<Ctx>, args: Value) -> CallFuture {
        (self.handler)(ctx, args)
    }
}

/// Schema for a tool without arguments.
pub fn no_arguments() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

/// Argument type for tools without arguments.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn destructive_remote_tools_publish_explicit_annotations() {
        let tool = Tool::new(
            "execute",
            "Run commands",
            no_arguments(),
            |_: Arc<Ctx>, _: NoArgs| async { Ok(()) },
        )
        .destructive_open_world();
        let annotations = &tool.describe()["annotations"];
        assert_eq!(annotations["readOnlyHint"], false);
        assert_eq!(annotations["destructiveHint"], true);
        assert_eq!(annotations["openWorldHint"], true);
    }
}
