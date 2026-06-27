//! Agent-facing tool → native command translation (subc edge only).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Translated {
    pub command: String,
    pub args: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranslateContext {
    pub diagnostics_on_edit: bool,
    pub preview: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateError {
    pub code: &'static str,
    pub message: String,
}

fn invalid_request(message: impl Into<String>) -> TranslateError {
    TranslateError {
        code: "invalid_request",
        message: message.into(),
    }
}

fn unsupported_tool(message: impl Into<String>) -> TranslateError {
    TranslateError {
        code: "unsupported_tool",
        message: message.into(),
    }
}

fn resolve_home_dir() -> Option<PathBuf> {
    let raw = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(raw)
}

fn expand_tilde(target: &str) -> String {
    if target == "~" {
        return resolve_home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.to_string());
    }
    if let Some(rest) = target.strip_prefix("~/") {
        if let Some(home) = resolve_home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    target.to_string()
}

pub fn resolve_path_from_project_root(project_root: &Path, target: &str) -> PathBuf {
    let expanded = expand_tilde(target);
    let path = Path::new(&expanded);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    normalize_lexically(&joined)
}

fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn is_empty_param(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn coerce_optional_int_result(
    value: Option<&Value>,
    param_name: &str,
    min: i64,
    max: i64,
) -> Result<Option<u64>, TranslateError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null()
        || matches!(value, Value::String(s) if s.is_empty())
        || matches!(value, Value::Array(a) if a.is_empty())
        || matches!(value, Value::Object(o) if o.is_empty())
    {
        return Ok(None);
    }
    if matches!(value, Value::Number(num) if num.as_i64() == Some(0) && min > 0) {
        return Ok(None);
    }

    let int_error = || {
        invalid_request(format!(
            "{param_name} must be an integer between {min} and {max}"
        ))
    };
    let n = match value {
        Value::Number(num) => num.as_i64().ok_or_else(int_error)?,
        Value::String(s) => {
            let parsed = s.parse::<f64>().map_err(|_| int_error())?;
            if !parsed.is_finite() || parsed.fract() != 0.0 {
                return Err(int_error());
            }
            parsed as i64
        }
        _ => return Err(int_error()),
    };
    if n < min || n > max {
        return Err(invalid_request(format!(
            "{param_name} must be between {min} and {max}"
        )));
    }
    Ok(Some(n as u64))
}

fn agent_args_map(args: &Value) -> Map<String, Value> {
    args.as_object().cloned().unwrap_or_default()
}

fn insert_resolved_file(map: &mut Map<String, Value>, project_root: &Path, file_path: &str) {
    let resolved = resolve_path_from_project_root(project_root, file_path);
    map.insert(
        "file".to_string(),
        Value::String(resolved.to_string_lossy().into_owned()),
    );
}

pub fn subc_translate(
    bare_name: &str,
    agent_args: &Value,
    project_root: &Path,
) -> Result<Translated, TranslateError> {
    subc_translate_with_context(
        bare_name,
        agent_args,
        project_root,
        TranslateContext::default(),
    )
}

pub fn subc_translate_with_context(
    bare_name: &str,
    agent_args: &Value,
    project_root: &Path,
    ctx: TranslateContext,
) -> Result<Translated, TranslateError> {
    match bare_name {
        "status" => Ok(Translated {
            command: "status".into(),
            args: Map::new(),
        }),
        "read" => translate_read(agent_args, project_root),
        "write" => translate_write(agent_args, project_root, ctx),
        "edit" => translate_edit(agent_args, project_root, ctx),
        "grep" => translate_grep(agent_args, project_root),
        "search" => translate_search(agent_args),
        "outline" => translate_outline(agent_args, project_root),
        "zoom" => translate_zoom(agent_args, project_root),
        "inspect" => translate_inspect(agent_args, project_root),
        "callgraph" => translate_callgraph(agent_args, project_root),
        other => Err(unsupported_tool(format!(
            "subc_translate: unsupported tool {other:?}"
        ))),
    }
}

fn coerce_boolean(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(num) => num.as_i64() == Some(1) || num.as_u64() == Some(1),
        Value::String(raw) => {
            let normalized = raw.trim().to_ascii_lowercase();
            normalized == "true" || normalized == "1"
        }
        _ => false,
    }
}

fn translate_callgraph(args: &Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let op = map_in
        .get("op")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'op' is required"))?;
    if !matches!(
        op,
        "call_tree" | "callers" | "trace_to" | "trace_to_symbol" | "impact" | "trace_data"
    ) {
        return Err(invalid_request(format!("callgraph: invalid op '{op}'")));
    }

    let file_path = map_in
        .get("filePath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'filePath' is required"))?;
    let symbol = map_in
        .get("symbol")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'symbol' is required"))?;

    if op == "trace_data" && map_in.get("expression").is_none_or(is_empty_param) {
        return Err(invalid_request(
            "'expression' is required for 'trace_data' op",
        ));
    }
    if op == "trace_to_symbol" && map_in.get("toSymbol").is_none_or(is_empty_param) {
        return Err(invalid_request(
            "'toSymbol' is required for 'trace_to_symbol' op",
        ));
    }

    let mut out = Map::new();
    insert_resolved_file(&mut out, project_root, file_path);
    out.insert("symbol".to_string(), Value::String(symbol.to_string()));

    if let Some(depth) =
        coerce_optional_int_result(map_in.get("depth"), "depth", 1, 9_007_199_254_740_991)?
    {
        out.insert("depth".to_string(), Value::Number(depth.into()));
    }
    if let Some(expression) = map_in.get("expression") {
        if !is_empty_param(expression) {
            out.insert("expression".to_string(), expression.clone());
        }
    }
    if let Some(to_symbol) = map_in.get("toSymbol") {
        if !is_empty_param(to_symbol) {
            out.insert("toSymbol".to_string(), to_symbol.clone());
        }
    }
    if let Some(to_file) = map_in.get("toFile") {
        if !is_empty_param(to_file) {
            let to_file = to_file
                .as_str()
                .ok_or_else(|| invalid_request("'toFile' must be a string"))?;
            let resolved = resolve_path_from_project_root(project_root, to_file);
            out.insert(
                "toFile".to_string(),
                Value::String(resolved.to_string_lossy().into_owned()),
            );
        }
    }
    if let Some(include_tests) = map_in.get("includeTests") {
        if !is_empty_param(include_tests) {
            out.insert(
                "include_tests".to_string(),
                Value::Bool(coerce_boolean(include_tests)),
            );
        }
    }

    Ok(Translated {
        command: op.to_string(),
        args: out,
    })
}

fn insert_common_mutation_flags(out: &mut Map<String, Value>, ctx: TranslateContext) {
    out.insert(
        "diagnostics".to_string(),
        Value::Bool(ctx.diagnostics_on_edit),
    );
    out.insert("include_diff_content".to_string(), Value::Bool(true));
    out.insert("preview".to_string(), Value::Bool(ctx.preview));
}

fn translate_read(args: &Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let file_path = map_in
        .get("filePath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'filePath' is required"))?;

    let mut out = Map::new();
    insert_resolved_file(&mut out, project_root, file_path);

    let mut start_line = map_in.get("startLine").and_then(Value::as_u64);
    let mut end_line = map_in.get("endLine").and_then(Value::as_u64);

    if start_line.is_none() {
        if let Some(offset) = map_in.get("offset").and_then(Value::as_u64) {
            start_line = Some(offset);
            if let Some(limit) = map_in.get("limit").and_then(Value::as_u64) {
                end_line = Some(offset.saturating_add(limit).saturating_sub(1));
            }
        }
    }

    if let Some(sl) = start_line {
        out.insert("start_line".to_string(), Value::Number(sl.into()));
    }
    if let Some(el) = end_line {
        out.insert("end_line".to_string(), Value::Number(el.into()));
    }
    if map_in.get("offset").is_none() {
        if let Some(limit) = map_in.get("limit").and_then(Value::as_u64) {
            out.insert("limit".to_string(), Value::Number(limit.into()));
        }
    }

    Ok(Translated {
        command: "read".into(),
        args: out,
    })
}

fn translate_write(
    args: &Value,
    project_root: &Path,
    ctx: TranslateContext,
) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let file_path = map_in
        .get("filePath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'filePath' is required"))?;
    let content = map_in
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_request("write: missing required param 'content'"))?;

    let mut out = Map::new();
    insert_resolved_file(&mut out, project_root, file_path);
    out.insert("content".to_string(), Value::String(content.to_string()));
    out.insert("create_dirs".to_string(), Value::Bool(true));
    insert_common_mutation_flags(&mut out, ctx);

    Ok(Translated {
        command: "write".into(),
        args: out,
    })
}

fn translate_edit(
    args: &Value,
    project_root: &Path,
    ctx: TranslateContext,
) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);

    if map_in.get("startLine").is_some() || map_in.get("endLine").is_some() {
        return Err(invalid_request(
            "edit: 'startLine'/'endLine' are not top-level parameters. \
             For line-range edits, nest them inside the `edits` array. \
             For find/replace, use 'oldString'/'newString'.",
        ));
    }

    let file_path = map_in
        .get("filePath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'filePath' is required"))?;

    let file_str = resolve_path_from_project_root(project_root, file_path)
        .to_string_lossy()
        .into_owned();

    if let Some(append) = map_in.get("appendContent").and_then(Value::as_str) {
        let mut out = Map::new();
        out.insert("file".to_string(), Value::String(file_str));
        out.insert("op".to_string(), Value::String("append".into()));
        out.insert(
            "append_content".to_string(),
            Value::String(append.to_string()),
        );
        out.insert("create_dirs".to_string(), Value::Bool(true));
        insert_common_mutation_flags(&mut out, ctx);
        return Ok(Translated {
            command: "edit_match".into(),
            args: out,
        });
    }

    if let Some(edits) = map_in.get("edits").and_then(Value::as_array) {
        let mut out = Map::new();
        out.insert("file".to_string(), Value::String(file_str));
        let translated_edits: Vec<Value> = edits
            .iter()
            .filter_map(|edit| {
                let obj = edit.as_object()?;
                let mut t = Map::new();
                for (key, value) in obj {
                    let native_key = match key.as_str() {
                        "oldString" => "match",
                        "newString" => "replacement",
                        "startLine" => "line_start",
                        "endLine" => "line_end",
                        other => other,
                    };
                    t.insert(native_key.to_string(), value.clone());
                }
                Some(Value::Object(t))
            })
            .collect();
        out.insert("edits".to_string(), Value::Array(translated_edits));
        insert_common_mutation_flags(&mut out, ctx);
        return Ok(Translated {
            command: "batch".into(),
            args: out,
        });
    }

    let symbol_is_string = map_in.get("symbol").and_then(Value::as_str).is_some();
    let old_string_is_string = map_in.get("oldString").and_then(Value::as_str).is_some();
    let has_content = map_in.get("content").is_some();

    if symbol_is_string && !old_string_is_string && has_content {
        let mut out = Map::new();
        out.insert("file".to_string(), Value::String(file_str));
        out.insert(
            "symbol".to_string(),
            map_in.get("symbol").cloned().unwrap_or(Value::Null),
        );
        out.insert("operation".to_string(), Value::String("replace".into()));
        out.insert(
            "content".to_string(),
            map_in.get("content").cloned().unwrap_or(Value::Null),
        );
        insert_common_mutation_flags(&mut out, ctx);
        return Ok(Translated {
            command: "edit_symbol".into(),
            args: out,
        });
    }

    if old_string_is_string {
        let mut out = Map::new();
        out.insert("file".to_string(), Value::String(file_str));
        out.insert(
            "match".to_string(),
            Value::String(
                map_in
                    .get("oldString")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
        );
        let replacement = map_in
            .get("newString")
            .and_then(Value::as_str)
            .unwrap_or("");
        out.insert(
            "replacement".to_string(),
            Value::String(replacement.to_string()),
        );
        if let Some(v) = map_in.get("replaceAll") {
            out.insert("replace_all".to_string(), v.clone());
        }
        if map_in.contains_key("occurrence") {
            if let Some(v) = map_in.get("occurrence") {
                out.insert("occurrence".to_string(), v.clone());
            }
        }
        insert_common_mutation_flags(&mut out, ctx);
        return Ok(Translated {
            command: "edit_match".into(),
            args: out,
        });
    }

    Err(invalid_request(
        "edit: no edit mode resolved from arguments.",
    ))
}

fn translate_grep(args: &Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let pattern = map_in
        .get("pattern")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("grep: missing required param 'pattern'"))?;

    let mut out = Map::new();
    out.insert("pattern".to_string(), Value::String(pattern.to_string()));
    out.insert("case_sensitive".to_string(), Value::Bool(true));
    if let Some(include) = map_in.get("include") {
        if !is_empty_param(include) {
            let include_arg = include.as_str().ok_or_else(|| {
                invalid_request("grep: 'include' must be a comma-separated string")
            })?;
            let includes = split_include_arg(include_arg)
                .into_iter()
                .map(|pattern| Value::String(normalize_glob(&pattern)))
                .collect::<Vec<_>>();
            if !includes.is_empty() {
                out.insert("include".to_string(), Value::Array(includes));
            }
        }
    }
    if let Some(path_val) = map_in.get("path") {
        if !is_empty_param(path_val) {
            if let Some(path_str) = path_val.as_str() {
                out.insert(
                    "path".to_string(),
                    Value::String(resolve_grep_path_arg(project_root, path_str)),
                );
            }
        }
    }
    out.insert("max_results".to_string(), Value::Number(100u64.into()));

    Ok(Translated {
        command: "grep".into(),
        args: out,
    })
}

fn normalize_glob(pattern: &str) -> String {
    if !pattern.contains('/') && !pattern.starts_with("**/") {
        format!("**/{pattern}")
    } else {
        pattern.to_string()
    }
}

fn split_include_arg(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut buf = String::new();
    for ch in raw.chars() {
        match ch {
            '{' => {
                depth += 1;
                buf.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                buf.push(ch);
            }
            ',' if depth == 0 => {
                let trimmed = buf.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

fn search_path_exists(project_root: &Path, raw: &str) -> bool {
    resolve_path_from_project_root(project_root, raw).exists()
}

fn split_search_path_arg(project_root: &Path, raw: &str) -> Vec<String> {
    if search_path_exists(project_root, raw) || !raw.chars().any(char::is_whitespace) {
        return vec![raw.to_string()];
    }

    let fragments = raw
        .split_whitespace()
        .filter(|fragment| !fragment.is_empty())
        .collect::<Vec<_>>();
    if fragments.len() < 2 {
        return vec![raw.to_string()];
    }

    let existing = fragments
        .iter()
        .filter(|fragment| search_path_exists(project_root, fragment))
        .map(|fragment| (*fragment).to_string())
        .collect::<Vec<_>>();
    if existing.is_empty() {
        vec![raw.to_string()]
    } else {
        existing
    }
}

fn resolve_grep_path_arg(project_root: &Path, raw: &str) -> String {
    split_search_path_arg(project_root, raw)
        .iter()
        .map(|target| {
            resolve_path_from_project_root(project_root, target)
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn translate_search(args: &Value) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let query = map_in
        .get("query")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            invalid_request("semantic_search: invalid params: `query` must be a non-empty string")
        })?;

    let mut out = Map::new();
    out.insert("query".to_string(), Value::String(query.to_string()));
    let top_k = coerce_optional_int_result(map_in.get("topK"), "topK", 1, 100)?.unwrap_or(10);
    out.insert("top_k".to_string(), Value::Number(top_k.into()));
    if let Some(hint) = map_in.get("hint") {
        if !is_empty_param(hint) {
            out.insert("hint".to_string(), hint.clone());
        }
    }
    if let Some(include_tests) = map_in.get("includeTests").and_then(Value::as_bool) {
        out.insert("include_tests".to_string(), Value::Bool(include_tests));
    }

    Ok(Translated {
        command: "semantic_search".into(),
        args: out,
    })
}

fn translate_outline(args: &Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let files_flag = map_in
        .get("files")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let target = map_in
        .get("target")
        .ok_or_else(|| invalid_request("outline: missing required param 'target'"))?;

    if is_empty_param(target) {
        return Err(invalid_request(
            "'target' must be a non-empty string or array of strings",
        ));
    }

    let mut out = Map::new();
    if let Some(include_tests) = map_in
        .get("includeTests")
        .or_else(|| map_in.get("include_tests"))
        .and_then(Value::as_bool)
    {
        out.insert("includeTests".to_string(), Value::Bool(include_tests));
    }

    if let Some(arr) = target.as_array() {
        if arr.is_empty() {
            return Err(invalid_request(
                "'target' must be a non-empty string or array of strings",
            ));
        }
        if files_flag {
            let resolved: Vec<Value> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|entry| {
                    let p = resolve_path_from_project_root(project_root, entry);
                    Value::String(p.to_string_lossy().into_owned())
                })
                .collect();
            out.insert("target".to_string(), Value::Array(resolved));
            out.insert("files".to_string(), Value::Bool(true));
            return Ok(Translated {
                command: "outline".into(),
                args: out,
            });
        }
        let resolved: Vec<Value> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|entry| {
                let p = resolve_path_from_project_root(project_root, entry);
                Value::String(p.to_string_lossy().into_owned())
            })
            .collect();
        out.insert("files".to_string(), Value::Array(resolved));
        return Ok(Translated {
            command: "outline".into(),
            args: out,
        });
    }

    if let Some(url) = target.as_str() {
        if !files_flag && (url.starts_with("http://") || url.starts_with("https://")) {
            out.insert("file".to_string(), Value::String(url.to_string()));
            return Ok(Translated {
                command: "outline".into(),
                args: out,
            });
        }
    }

    let target_str = target.as_str().ok_or_else(|| {
        invalid_request("'target' must be a non-empty string or array of strings")
    })?;

    let resolved = resolve_path_from_project_root(project_root, target_str);
    let is_dir = std::fs::metadata(&resolved)
        .map(|m| m.is_dir())
        .unwrap_or(false);

    if files_flag {
        if is_dir {
            out.insert(
                "directory".to_string(),
                Value::String(resolved.to_string_lossy().into_owned()),
            );
        } else {
            out.insert(
                "file".to_string(),
                Value::String(resolved.to_string_lossy().into_owned()),
            );
        }
        out.insert("files".to_string(), Value::Bool(true));
    } else if is_dir {
        out.insert(
            "directory".to_string(),
            Value::String(resolved.to_string_lossy().into_owned()),
        );
    } else {
        out.insert(
            "file".to_string(),
            Value::String(resolved.to_string_lossy().into_owned()),
        );
    }

    Ok(Translated {
        command: "outline".into(),
        args: out,
    })
}

fn zoom_target_entry_is_empty(entry: &Value) -> bool {
    let Some(obj) = entry.as_object() else {
        return true;
    };
    let file_path_empty = obj
        .get("filePath")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty);
    let symbol_empty = obj
        .get("symbol")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty);
    file_path_empty && symbol_empty
}

fn zoom_targets_provided(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    if is_empty_param(value) {
        return false;
    }
    match value {
        Value::Array(items) => !items.iter().all(zoom_target_entry_is_empty),
        Value::Object(_) => !zoom_target_entry_is_empty(value),
        _ => false,
    }
}

fn translate_zoom_targets(
    targets_value: &Value,
    project_root: &Path,
) -> Result<Vec<Value>, TranslateError> {
    let target_values: Vec<&Value> = match targets_value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![targets_value],
        _ => {
            return Err(invalid_request(
                "'targets' must be a non-empty object or array",
            ))
        }
    };

    if target_values.is_empty() {
        return Err(invalid_request(
            "'targets' must be a non-empty object or array",
        ));
    }

    let mut out = Vec::with_capacity(target_values.len());
    for (index, target) in target_values.into_iter().enumerate() {
        let obj = target.as_object();
        let file_path = obj
            .and_then(|obj| obj.get("filePath"))
            .and_then(Value::as_str)
            .filter(|file_path| !file_path.is_empty())
            .ok_or_else(|| {
                invalid_request(format!(
                    "targets[{index}].filePath must be a non-empty string"
                ))
            })?;
        let symbol = obj
            .and_then(|obj| obj.get("symbol"))
            .and_then(Value::as_str)
            .filter(|symbol| !symbol.is_empty())
            .ok_or_else(|| {
                invalid_request(format!(
                    "targets[{index}].symbol must be a non-empty string"
                ))
            })?;
        let resolved = resolve_path_from_project_root(project_root, file_path);
        let mut target_out = Map::new();
        target_out.insert(
            "file".to_string(),
            Value::String(resolved.to_string_lossy().into_owned()),
        );
        target_out.insert("symbol".to_string(), Value::String(symbol.to_string()));
        target_out.insert(
            "target_label".to_string(),
            Value::String(file_path.to_string()),
        );
        out.push(Value::Object(target_out));
    }
    Ok(out)
}

fn translate_zoom(args: &Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);

    let has_targets = zoom_targets_provided(map_in.get("targets"));
    let has_file_path = map_in
        .get("filePath")
        .is_some_and(|value| !is_empty_param(value));
    let has_url = map_in
        .get("url")
        .is_some_and(|value| !is_empty_param(value));
    let has_symbols = map_in
        .get("symbols")
        .is_some_and(|value| !is_empty_param(value));

    let mut out = Map::new();

    if has_targets {
        if has_file_path || has_url || has_symbols {
            return Err(invalid_request(
                "'targets' is mutually exclusive with 'filePath', 'url', and 'symbols'",
            ));
        }
        let targets_value = map_in
            .get("targets")
            .expect("has_targets implies a targets value exists");
        out.insert(
            "targets".to_string(),
            Value::Array(translate_zoom_targets(targets_value, project_root)?),
        );

        if let Some(context_lines) = coerce_optional_int_result(
            map_in.get("contextLines"),
            "contextLines",
            1,
            9_007_199_254_740_991,
        )? {
            out.insert(
                "context_lines".to_string(),
                Value::Number(context_lines.into()),
            );
        }

        if map_in.get("callgraph").is_some_and(coerce_boolean) {
            out.insert("callgraph".to_string(), Value::Bool(true));
        }

        return Ok(Translated {
            command: "zoom".into(),
            args: out,
        });
    }

    let file_path = map_in
        .get("filePath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let url = map_in
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    match (file_path, url) {
        (None, None) => {
            return Err(invalid_request(
                "Provide exactly one of 'filePath', 'url', or 'targets'",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_request(
                "Provide exactly ONE of 'filePath' or 'url' — not both",
            ));
        }
        _ => {}
    }

    if let Some(url) = url {
        out.insert("file".to_string(), Value::String(url.to_string()));
    } else if let Some(file_path) = file_path {
        insert_resolved_file(&mut out, project_root, file_path);
    }

    if let Some(symbols) = map_in.get("symbols") {
        if !is_empty_param(symbols) {
            match symbols {
                Value::String(symbol) => {
                    out.insert("symbol".to_string(), Value::String(symbol.to_string()));
                }
                Value::Array(items) => {
                    let joined = items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !joined.is_empty() {
                        out.insert("symbol".to_string(), Value::String(joined));
                    }
                }
                _ => {
                    return Err(invalid_request(
                        "'symbols' must be a string or array of strings",
                    ))
                }
            }
        }
    }

    if let Some(context_lines) = coerce_optional_int_result(
        map_in.get("contextLines"),
        "contextLines",
        1,
        9_007_199_254_740_991,
    )? {
        out.insert(
            "context_lines".to_string(),
            Value::Number(context_lines.into()),
        );
    }

    if let Some(start_line) = coerce_optional_int_result(
        map_in.get("startLine"),
        "startLine",
        1,
        9_007_199_254_740_991,
    )? {
        out.insert("start_line".to_string(), Value::Number(start_line.into()));
    }
    if let Some(end_line) =
        coerce_optional_int_result(map_in.get("endLine"), "endLine", 1, 9_007_199_254_740_991)?
    {
        out.insert("end_line".to_string(), Value::Number(end_line.into()));
    }

    if map_in.get("callgraph").is_some_and(coerce_boolean) {
        out.insert("callgraph".to_string(), Value::Bool(true));
    }

    Ok(Translated {
        command: "zoom".into(),
        args: out,
    })
}

fn translate_inspect(args: &Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let mut out = Map::new();

    if let Some(sections) = map_in.get("sections") {
        if !is_empty_param(sections) {
            out.insert("sections".to_string(), sections.clone());
        }
    }

    if let Some(scope) = map_in.get("scope") {
        if !is_empty_param(scope) {
            match scope {
                Value::String(s) if !s.is_empty() => {
                    let resolved = resolve_path_from_project_root(project_root, s);
                    out.insert(
                        "scope".to_string(),
                        Value::String(resolved.to_string_lossy().into_owned()),
                    );
                }
                Value::Array(arr) => {
                    let resolved: Vec<Value> = arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|entry| {
                            let p = resolve_path_from_project_root(project_root, entry);
                            Value::String(p.to_string_lossy().into_owned())
                        })
                        .collect();
                    out.insert("scope".to_string(), Value::Array(resolved));
                }
                other => {
                    out.insert("scope".to_string(), other.clone());
                }
            }
        }
    }

    if let Some(top_k) = coerce_optional_int_result(map_in.get("topK"), "topK", 1, 100)? {
        out.insert("topK".to_string(), Value::Number(top_k.into()));
    }

    Ok(Translated {
        command: "inspect".into(),
        args: out,
    })
}
