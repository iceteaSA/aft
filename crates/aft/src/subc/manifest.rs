//! Manifest, lane classification, and control-surface helpers exposed over subc.

use super::{
    json, Bindings, Concurrency, ExecutionMode, Flags, IdentityBinding, IdentityScope, Lane,
    LazyLock, ModuleManifest, Priority, ProviderRole, StorageBinding, StorageKind, StorageScope,
    Tool, TrustTier, Value, MODULE_CONTROL_OP_HEALTH_CHECK, PROTOCOL_VERSION,
};

pub(super) fn is_bash_family_tool(name: &str) -> bool {
    name == "bash" || name.starts_with("bash_")
}

pub(super) fn is_subc_agent_core_tool(name: &str) -> bool {
    matches!(
        name,
        "status"
            | "bash"
            | "read"
            | "write"
            | "edit"
            | "apply_patch"
            | "grep"
            | "glob"
            | "search"
            | "outline"
            | "zoom"
            | "inspect"
            | "callgraph"
            | "conflicts"
            | "ast_search"
            | "ast_replace"
            | "delete"
            | "move"
            | "import"
            | "refactor"
            | "safety"
    )
}

/// Internal plumbing commands the harness consumer (NOT the agent) invokes over
/// a bound route. These are NOT agent-facing tools — they carry no agent surface
/// and never reach the model — so they're not in the manifest /
/// `is_subc_agent_core_tool`, but the plugin must reach dispatch with them over
/// subc for background-bash delivery and safety undo/restore to work.
///
/// This is a DELIBERATELY TIGHT allowlist, kept separate from the agent
/// core-tool gate so it cannot widen the fail-closed backstop in
/// `handle_tool_call`. Every entry is session-scoped (the bind session is
/// reinjected by `run_tool_call`, overriding any body `session_id`) and carries
/// NO config/trust surface, so admitting them does not reopen the
/// `configure`-bypass hole the gate exists to close. The untrusted-bind bash
/// denial fires BEFORE this allowlist (`is_bash_family_tool` matches every
/// `bash_*` name), so untrusted binds still cannot observe bash state:
/// - `bash_status`: read-only per-session task snapshot; required so a
///   respawned module can report rehydrated detached tasks by task id.
/// - `bash_drain_completions` / `bash_ack_completions`: per-session completion
///   registry plumbing for the bg_events wake lane (drain = PureRead,
///   ack = Mutating in `command_lane`).
/// - `undo_preview` / `checkpoint_paths`: read-only permission-preview reads
///   over the session's own backup/checkpoint state — the plugin safety tool
///   calls them BEFORE `aft_safety undo`/`restore` to know which paths to ask
///   permission for. Without them, safety undo/restore fails over subc.
pub(super) fn is_subc_native_plumbing_tool(name: &str) -> bool {
    matches!(
        name,
        "bash_status"
            | "bash_drain_completions"
            | "bash_ack_completions"
            | "undo_preview"
            | "checkpoint_paths"
    )
}

pub(super) fn command_lane(command: &str) -> Lane {
    match command {
        "ping"
        | "version"
        | "echo"
        | "bash_drain_completions"
        | "bash_regex_match"
        | "db_get_state"
        | "db_get_host_state"
        | "read"
        | "undo_preview"
        | "edit_history"
        | "checkpoint_paths"
        | "list_checkpoints"
        | "conflicts"
        | "glob"
        | "grep"
        | "git_conflicts"
        | "ast_search" => Lane::PureRead,

        // Lazy reads mutate parser/terminal/url caches on a miss, but are still
        // classified onto the reader pool; install races are handled at the
        // individual cache sites.
        "bash_status" | "outline" | "zoom" => Lane::PureRead,

        "status"
        | "inspect"
        | "lsp_diagnostics"
        | "lsp_inspect"
        | "lsp_hover"
        | "lsp_goto_definition"
        | "lsp_find_references"
        | "lsp_prepare_rename" => Lane::SerialLspStatus,

        "semantic_search" | "search" | "callgraph" | "callers" | "impact" | "call_tree"
        | "trace_to" | "trace_to_symbol" | "trace_data" | "inspect_tier2_run" => Lane::HeavyInit,

        "bash"
        | "bash_ack_completions"
        | "bash_notify"
        | "bash_unnotify"
        | "bash_promote"
        | "bash_wait_detach"
        | "bash_kill"
        | "bash_write"
        | "db_set_state"
        | "db_set_host_state"
        | "undo"
        | "checkpoint"
        | "restore_checkpoint"
        | "write"
        | "delete_file"
        | "move_file"
        | "edit"
        | "edit_symbol"
        | "edit_match"
        | "batch"
        | "add_import"
        | "remove_import"
        | "organize_imports"
        | "configure"
        | "move_symbol"
        | "extract_function"
        | "inline_symbol"
        | "ast_replace"
        | "lsp_rename"
        | "list_filters"
        | "trust_filter_project"
        | "untrust_filter_project"
        | "snapshot" => Lane::Mutating,

        _ => Lane::Mutating,
    }
}

static SUBC_TOOL_SCHEMAS: LazyLock<serde_json::Map<String, Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../subc_tool_schemas.json"))
        .unwrap_or_else(|e| panic!("subc_tool_schemas.json: {e}"))
});

fn tool_schema(name: &str) -> Value {
    SUBC_TOOL_SCHEMAS.get(name).cloned().unwrap_or_else(|| {
        log::warn!(
            "subc build_manifest: missing embedded schema for tool {name:?}; using placeholder"
        );
        json!({ "type": "object" })
    })
}

fn tool_description(name: &str) -> Option<String> {
    SUBC_TOOL_SCHEMAS
        .get(name)
        .and_then(|schema| schema.get("description"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// AFT's subc-mode capability manifest. It uses bare internal tool names
/// because the gateway adds any `aft_` prefix for agent-facing displays; AFT
/// schedules concurrent calls itself; the gateway runs AFT directly without a
/// sandbox. The manifest lists every tool an agent can call over subc.
pub(super) fn build_manifest() -> ModuleManifest {
    let tool = |name: &str, execution_mode: ExecutionMode| Tool {
        name: name.to_string(),
        description: tool_description(name),
        execution_mode,
        schema: tool_schema(name),
    };
    // execution_mode keys on externally-observable side effects, NOT internal
    // ctx mutation: the readers warm AFT's own index/cache/symbol artifacts
    // (internal), not the user's workspace, so they are Pure. Bash is Mutating
    // because spawning a detached process changes external state, and edit/write
    // produce observable file writes. Unfenceable stays unused here because AFT
    // schedules bash internally and releases the Mutating worker after spawn.
    ModuleManifest {
        module_id: "aft".to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![
                tool("status", ExecutionMode::Pure),
                tool("bash", ExecutionMode::Mutating),
                tool("read", ExecutionMode::Pure),
                tool("write", ExecutionMode::Mutating),
                tool("edit", ExecutionMode::Mutating),
                tool("apply_patch", ExecutionMode::Mutating),
                tool("grep", ExecutionMode::Pure),
                tool("glob", ExecutionMode::Pure),
                tool("search", ExecutionMode::Pure),
                tool("outline", ExecutionMode::Pure),
                tool("zoom", ExecutionMode::Pure),
                tool("inspect", ExecutionMode::Pure),
                tool("callgraph", ExecutionMode::Pure),
                tool("conflicts", ExecutionMode::Pure),
                tool("ast_search", ExecutionMode::Pure),
                tool("ast_replace", ExecutionMode::Mutating),
                tool("delete", ExecutionMode::Mutating),
                tool("move", ExecutionMode::Mutating),
                tool("import", ExecutionMode::Mutating),
                tool("refactor", ExecutionMode::Mutating),
                tool("safety", ExecutionMode::Mutating),
            ],
            identity_scope: vec![IdentityScope::Session, IdentityScope::Project],
            concurrency: Concurrency::ModuleManaged,
            emits_push: true,
            sub_supervises: true,
        }],
        consumes: Vec::new(),
        scheduled_tasks: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
    }
}

pub(super) fn control_ops() -> Option<Vec<String>> {
    Some(vec![
        "route.bind".to_string(),
        "route.status".to_string(),
        MODULE_CONTROL_OP_HEALTH_CHECK.to_string(),
    ])
}

pub(super) fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const CORE_TOOLS: [&str; 21] = [
        "status",
        "bash",
        "read",
        "write",
        "edit",
        "apply_patch",
        "grep",
        "glob",
        "search",
        "outline",
        "zoom",
        "inspect",
        "callgraph",
        "conflicts",
        "ast_search",
        "ast_replace",
        "delete",
        "move",
        "import",
        "refactor",
        "safety",
    ];

    fn is_bare_placeholder_schema(schema: &Value) -> bool {
        schema == &json!({ "type": "object" })
    }

    #[test]
    fn build_manifest_serves_embedded_tool_schemas() {
        let manifest = build_manifest();
        let tools = match manifest.provides.first() {
            Some(ProviderRole::ToolProvider { tools, .. }) => tools,
            _ => panic!("expected ToolProvider"),
        };
        let by_name: HashMap<&str, &Tool> = tools.iter().map(|t| (t.name.as_str(), t)).collect();
        for name in CORE_TOOLS {
            let tool = by_name
                .get(name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            assert!(
                tool.description
                    .as_deref()
                    .is_some_and(|description| !description.is_empty()),
                "{name} must carry a non-empty manifest description"
            );
            assert!(
                !is_bare_placeholder_schema(&tool.schema),
                "{name} must not use bare placeholder schema"
            );
            assert_eq!(
                tool.schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{name} schema must be an object"
            );
        }

        let read = by_name["read"]
            .schema
            .get("properties")
            .and_then(|p| p.as_object());
        let read_props = read.expect("read schema properties");
        assert!(
            read_props.contains_key("filePath"),
            "read schema must expose filePath"
        );

        let status = &by_name["status"].schema;
        assert_eq!(
            status.get("properties").and_then(|v| v.as_object()),
            Some(&serde_json::Map::new()),
            "status schema must have empty properties"
        );
        assert_eq!(
            status.get("additionalProperties").and_then(|v| v.as_bool()),
            Some(false),
            "status schema must forbid additionalProperties"
        );
    }

    #[test]
    fn build_manifest_classifies_execution_mode_by_observable_effect() {
        let manifest = build_manifest();
        let tools = match manifest.provides.first() {
            Some(ProviderRole::ToolProvider { tools, .. }) => tools,
            _ => panic!("expected ToolProvider"),
        };
        let by_name: HashMap<&str, &Tool> = tools.iter().map(|t| (t.name.as_str(), t)).collect();

        // Readers warm AFT's own index/cache/symbol artifacts (internal ctx
        // mutation), not the user's observable workspace, so they are Pure.
        for name in [
            "status",
            "read",
            "grep",
            "glob",
            "search",
            "outline",
            "zoom",
            "inspect",
            "callgraph",
            "conflicts",
            "ast_search",
        ] {
            assert_eq!(
                by_name[name].execution_mode,
                ExecutionMode::Pure,
                "{name} produces no observable side effect and must be Pure"
            );
        }
        // Mutating tools can write files, change safety state, or spawn processes.
        for name in [
            "bash",
            "write",
            "edit",
            "apply_patch",
            "ast_replace",
            "delete",
            "move",
            "import",
            "refactor",
            "safety",
        ] {
            assert_eq!(
                by_name[name].execution_mode,
                ExecutionMode::Mutating,
                "{name} writes files and must be Mutating"
            );
        }
    }

    #[test]
    fn subc_agent_lanes_classify_new_read_tools() {
        assert_eq!(command_lane("callgraph"), Lane::HeavyInit);
        assert_eq!(command_lane("conflicts"), Lane::PureRead);
        assert_eq!(command_lane("bash_status"), Lane::PureRead);
        assert!(is_subc_native_plumbing_tool("bash_status"));
    }

    #[test]
    fn native_plumbing_allowlist_admits_exactly_drain_ack_and_safety_previews() {
        // BC2: the route gate admits a name when it's an agent core tool OR a
        // native plumbing command. These carry no agent surface and no
        // config/trust surface, so they're admitted to dispatch over a bound
        // route while everything else (notably `configure`) stays fail-closed.
        assert!(is_subc_native_plumbing_tool("bash_drain_completions"));
        assert!(is_subc_native_plumbing_tool("bash_ack_completions"));
        // Safety-tool permission previews: read-only, session-scoped. Without
        // these, aft_safety undo/restore breaks over the subc transport.
        assert!(is_subc_native_plumbing_tool("undo_preview"));
        assert!(is_subc_native_plumbing_tool("checkpoint_paths"));

        // The allowlist is TIGHT — it must not admit the config-bypass vector
        // the fail-closed gate exists to block, nor any other native command.
        assert!(!is_subc_native_plumbing_tool("configure"));
        assert!(!is_subc_native_plumbing_tool("bash"));
        assert!(!is_subc_native_plumbing_tool("bash_kill"));
        assert!(!is_subc_native_plumbing_tool("db_set_state"));
        assert!(!is_subc_native_plumbing_tool("undo"));

        // The plumbing commands are NOT agent-facing tools — they must stay out
        // of the manifest gate so they never reach the model surface.
        assert!(!is_subc_agent_core_tool("bash_drain_completions"));
        assert!(!is_subc_agent_core_tool("bash_ack_completions"));

        // Lanes are already assigned (pre-existing): drain reads, ack mutates.
        assert_eq!(command_lane("bash_drain_completions"), Lane::PureRead);
        assert_eq!(command_lane("bash_ack_completions"), Lane::Mutating);
    }
}
