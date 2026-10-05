//! Shared analysis context read by workers: kondo config, pass-1 defs (jar + builtin layers), client options.
use crate::analyzer::{Config, DefsIndex};
use arc_swap::ArcSwap;
use std::sync::Arc;

/// Client capabilities / settings that change answers (from `initialize`).
#[derive(Clone, Debug)]
pub struct ClientOpts {
    /// `dependency-scheme` = "jar" (else "zipfile").
    pub jar_scheme: bool,
    /// Hover `contentFormat` contains markdown.
    pub hover_markdown: bool,
    /// `[:hover :arity-on-same-line?]` or `:show-docs-arity-on-same-line?`.
    pub arity_on_same_line: bool,
    pub hide_file_location: bool,
    pub hide_signature_call: bool,
    /// Completion `resolveSupport.properties` contains `documentation` / `additionalTextEdits`.
    pub resolve_documentation: bool,
    pub resolve_alias_edit: bool,
    /// Completion `documentationFormat` contains markdown.
    pub completion_markdown: bool,
    /// Completion `snippetSupport`.
    pub completion_snippets: bool,
    /// Setting `:use-metadata-for-privacy?` (snippet `defn-`).
    pub use_metadata_privacy: bool,
    /// Setting `:additional-snippets`: (name, detail, snippet).
    pub additional_snippets: Vec<(String, Option<String>, String)>,
    /// workspace.workspaceEdit: documentChanges / resourceOperations present / changeAnnotationSupport present.
    pub we_doc_changes: bool,
    pub we_resource_ops: bool,
    pub we_annotations: bool,
}

impl Default for ClientOpts {
    fn default() -> Self {
        ClientOpts { jar_scheme: false, hover_markdown: false, arity_on_same_line: false, hide_file_location: false, hide_signature_call: false, resolve_documentation: false, resolve_alias_edit: false, completion_markdown: false, completion_snippets: false, use_metadata_privacy: false, additional_snippets: Vec::new(), we_doc_changes: false, we_resource_ops: false, we_annotations: false }
    }
}

pub struct Ctx {
    pub cfg: ArcSwap<Config>,
    /// Defs visible to pass 1 (`:refer :all` of jar namespaces); never contains project files.
    pub defs: ArcSwap<DefsIndex>,
    pub opts: ArcSwap<ClientOpts>,
    /// Project source paths (absolute); empty until discovery. Files outside them are "external" for keyword usages.
    pub source_paths: ArcSwap<Vec<std::path::PathBuf>>,
    /// Mova stdlib files with no `ns` form: uri -> namespace of their forms (`core/core.mova` -> `clojure.core`).
    pub init_ns: ArcSwap<std::collections::HashMap<String, crate::intern::SymId>>,
}

impl Ctx {
    pub fn new() -> Ctx {
        Ctx { cfg: ArcSwap::from_pointee(Config::new()), defs: ArcSwap::from_pointee(DefsIndex::new()), opts: ArcSwap::from_pointee(ClientOpts::default()), source_paths: ArcSwap::from_pointee(Vec::new()), init_ns: ArcSwap::from_pointee(Default::default()) }
    }
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx::new()
    }
}

pub type SharedCtx = Arc<Ctx>;
