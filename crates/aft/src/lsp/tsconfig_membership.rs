use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde_json::Value;

use crate::lsp::roots::find_workspace_root;

const TSCONFIG_JSON: &str = "tsconfig.json";
const MAX_EXTENDS_DEPTH: usize = 16;
const TS_JS_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "d.ts", "js", "jsx", "mjs", "cjs", "mts", "cts", "d.mts", "d.cts",
];

/// Per-inspect-call cache for TypeScript project membership decisions.
///
/// `typescript-language-server` falls back to an inferred project when AFT opens
/// a TS/JS file that is excluded from the nearest tsconfig. That inferred project
/// does not inherit the build's `types`, `paths`, or other compiler options, so
/// diagnostics can diverge from `tsc -p`. This cache resolves the nearest
/// tsconfig once and lets callers suppress diagnostics for files that the build
/// would not check.
#[derive(Default)]
pub(crate) struct TsconfigMembershipCache {
    projects: HashMap<PathBuf, Option<ResolvedTsConfig>>,
}

impl TsconfigMembershipCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drop all memoized project resolutions. Called when a tsconfig-like file
    /// changes (watcher) or on `configure`, so the next membership query
    /// re-reads from disk. A wholesale clear is intentional: nearest-tsconfig
    /// resolution and `extends` chains make per-key invalidation unsound (a new
    /// nested tsconfig or an edited `extends` parent changes membership for
    /// files keyed under a different directory).
    pub(crate) fn clear(&mut self) {
        self.projects.clear();
    }

    pub(crate) fn should_skip_diagnostics(&mut self, file: &Path) -> bool {
        if !is_ts_js_file(file) {
            return false;
        }

        let Some(tsconfig_dir) = find_workspace_root(file, &[TSCONFIG_JSON]) else {
            return false;
        };

        let project = self
            .projects
            .entry(tsconfig_dir.clone())
            .or_insert_with(|| load_project(&tsconfig_dir));

        match project {
            Some(project) => !project.contains(file),
            None => false,
        }
    }
}

#[derive(Debug)]
struct ResolvedTsConfig {
    files: Vec<PathBuf>,
    include: PatternGroup,
    exclude: PatternGroup,
}

impl ResolvedTsConfig {
    fn contains(&self, file: &Path) -> bool {
        let file = canonical_or_normalized(file);
        if self.files.iter().any(|member| *member == file) {
            return true;
        }

        self.include.is_match(&file) && !self.exclude.is_match(&file)
    }
}

#[derive(Debug)]
struct PatternGroup {
    groups: Vec<OriginGlobSet>,
}

impl PatternGroup {
    fn new(groups: Vec<OriginGlobSet>) -> Self {
        Self { groups }
    }

    fn is_match(&self, file: &Path) -> bool {
        self.groups.iter().any(|group| group.is_match(file))
    }
}

#[derive(Debug)]
struct OriginGlobSet {
    origin_dir: PathBuf,
    glob_set: GlobSet,
}

impl OriginGlobSet {
    fn is_match(&self, file: &Path) -> bool {
        let Ok(relative) = file.strip_prefix(&self.origin_dir) else {
            return false;
        };
        self.glob_set.is_match(relative)
    }
}

#[derive(Debug, Clone)]
struct Field<T> {
    origin_dir: PathBuf,
    value: T,
}

#[derive(Debug, Clone, Default)]
struct RawTsConfig {
    extends: Option<String>,
    files: Option<Vec<String>>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    out_dir: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ResolvedFields {
    files: Option<Field<Vec<String>>>,
    include: Option<Field<Vec<String>>>,
    exclude: Option<Field<Vec<String>>>,
    out_dir: Option<Field<String>>,
}

fn load_project(tsconfig_dir: &Path) -> Option<ResolvedTsConfig> {
    let tsconfig_path = tsconfig_dir.join(TSCONFIG_JSON);
    let mut visiting = HashSet::new();
    match resolve_tsconfig_fields(&tsconfig_path, 0, &mut visiting) {
        Ok(fields) => build_resolved_config(tsconfig_dir, fields),
        Err(message) => {
            crate::slog_warn!(
                "[inspect:diagnostics] unable to resolve {}: {message}",
                tsconfig_path.display()
            );
            None
        }
    }
}

fn resolve_tsconfig_fields(
    tsconfig_path: &Path,
    depth: usize,
    visiting: &mut HashSet<PathBuf>,
) -> Result<ResolvedFields, String> {
    if depth > MAX_EXTENDS_DEPTH {
        return Err(format!(
            "tsconfig extends depth exceeded {MAX_EXTENDS_DEPTH} at {}",
            tsconfig_path.display()
        ));
    }

    let tsconfig_path = canonical_or_normalized(tsconfig_path);
    if !visiting.insert(tsconfig_path.clone()) {
        return Err(format!(
            "tsconfig extends cycle involving {}",
            tsconfig_path.display()
        ));
    }

    let raw = parse_tsconfig(&tsconfig_path)?;
    let origin_dir = tsconfig_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut resolved = if let Some(extends) = raw.extends.as_deref() {
        let parent = resolve_extends_path(&origin_dir, extends).ok_or_else(|| {
            format!(
                "unsupported or missing tsconfig extends '{extends}' from {}",
                tsconfig_path.display()
            )
        })?;
        resolve_tsconfig_fields(&parent, depth + 1, visiting)?
    } else {
        ResolvedFields::default()
    };

    if let Some(files) = raw.files {
        resolved.files = Some(Field {
            origin_dir: origin_dir.clone(),
            value: files,
        });
    }
    if let Some(include) = raw.include {
        resolved.include = Some(Field {
            origin_dir: origin_dir.clone(),
            value: include,
        });
    }
    if let Some(exclude) = raw.exclude {
        resolved.exclude = Some(Field {
            origin_dir: origin_dir.clone(),
            value: exclude,
        });
    }
    if let Some(out_dir) = raw.out_dir {
        resolved.out_dir = Some(Field {
            origin_dir,
            value: out_dir,
        });
    }

    visiting.remove(&tsconfig_path);
    Ok(resolved)
}

fn parse_tsconfig(tsconfig_path: &Path) -> Result<RawTsConfig, String> {
    let source = fs::read_to_string(tsconfig_path)
        .map_err(|err| format!("read {}: {err}", tsconfig_path.display()))?;
    let stripped = strip_jsonc(&source);
    let value = serde_json::from_str::<Value>(&stripped)
        .map_err(|err| format!("parse {}: {err}", tsconfig_path.display()))?;

    Ok(RawTsConfig {
        extends: string_field(&value, "extends"),
        files: string_array_field(&value, "files"),
        include: string_array_field(&value, "include"),
        exclude: string_array_field(&value, "exclude"),
        out_dir: value
            .get("compilerOptions")
            .and_then(|compiler_options| string_field(compiler_options, "outDir")),
    })
}

fn build_resolved_config(tsconfig_dir: &Path, fields: ResolvedFields) -> Option<ResolvedTsConfig> {
    let files = fields
        .files
        .as_ref()
        .map(|field| {
            field
                .value
                .iter()
                .map(|file| canonical_or_normalized(&field.origin_dir.join(file)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let include = if let Some(field) = fields.include.as_ref() {
        PatternGroup::new(vec![compile_origin_globs(&field.origin_dir, &field.value)?])
    } else if fields.files.is_some() {
        // TypeScript semantics: when `files` is specified and `include` is
        // absent, ONLY the listed files are members — `include` does NOT default
        // to `**/*`. Defaulting it here made a files-only tsconfig match every
        // file, so build-excluded files leaked into the status-bar / aft_inspect
        // diagnostic counts. An empty include matches nothing; `files` membership
        // is still handled by the explicit list in ResolvedTsConfig::contains.
        PatternGroup::new(Vec::new())
    } else {
        PatternGroup::new(vec![compile_origin_globs(
            tsconfig_dir,
            &["**/*".to_string()],
        )?])
    };

    let exclude = if let Some(field) = fields.exclude.as_ref() {
        PatternGroup::new(vec![compile_origin_globs(&field.origin_dir, &field.value)?])
    } else {
        let mut defaults = vec![
            "node_modules".to_string(),
            "bower_components".to_string(),
            "jspm_packages".to_string(),
        ];
        if let Some(out_dir) = fields.out_dir.as_ref() {
            defaults.push(path_relative_to(
                tsconfig_dir,
                &out_dir.origin_dir.join(&out_dir.value),
            ));
        }
        PatternGroup::new(vec![compile_origin_globs(tsconfig_dir, &defaults)?])
    };

    Some(ResolvedTsConfig {
        files,
        include,
        exclude,
    })
}

fn compile_origin_globs(origin_dir: &Path, patterns: &[String]) -> Option<OriginGlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let Some(pattern) = ts_pattern_to_glob(pattern) else {
            continue;
        };
        match glob(&pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(err) => {
                crate::slog_warn!(
                    "[inspect:diagnostics] invalid tsconfig glob '{}' from {}: {err}",
                    pattern,
                    origin_dir.display()
                );
                continue;
            }
        }
    }

    let glob_set = match builder.build() {
        Ok(glob_set) => glob_set,
        Err(err) => {
            crate::slog_warn!(
                "[inspect:diagnostics] failed to build tsconfig glob set from {}: {err}",
                origin_dir.display()
            );
            return None;
        }
    };

    Some(OriginGlobSet {
        origin_dir: canonical_or_normalized(origin_dir),
        glob_set,
    })
}

fn glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
}

fn ts_pattern_to_glob(pattern: &str) -> Option<String> {
    let trimmed = pattern.trim().replace('\\', "/");
    if trimmed.is_empty() {
        return None;
    }

    let trimmed = trimmed.trim_start_matches("./").trim_end_matches('/');
    if trimmed.is_empty() {
        return Some("**/*".to_string());
    }

    if !has_wildcard(trimmed) && Path::new(trimmed).extension().is_none() {
        return Some(format!("{trimmed}/**/*"));
    }

    Some(trimmed.to_string())
}

fn resolve_extends_path(origin_dir: &Path, extends: &str) -> Option<PathBuf> {
    let raw = extends.trim();
    if raw.is_empty() {
        return None;
    }

    let raw_path = Path::new(raw);
    if !raw_path.is_absolute()
        && !raw.starts_with("./")
        && !raw.starts_with("../")
        && raw != "."
        && raw != ".."
    {
        return None;
    }

    let base = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        origin_dir.join(raw_path)
    };

    extends_candidates(&base)
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| canonical_or_normalized(&candidate))
}

fn extends_candidates(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(base.to_path_buf());

    if base.extension().and_then(|extension| extension.to_str()) != Some("json") {
        let mut with_json = base.as_os_str().to_os_string();
        with_json.push(".json");
        candidates.push(PathBuf::from(with_json));
    }

    if base.extension().is_none() {
        candidates.push(base.join(TSCONFIG_JSON));
    }

    candidates
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_array_field(value: &Value, key: &str) -> Option<Vec<String>> {
    let array = value.get(key)?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn has_wildcard(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

fn is_ts_js_file(path: &Path) -> bool {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    TS_JS_EXTENSIONS
        .iter()
        .any(|extension| filename.ends_with(&format!(".{extension}")))
}

fn path_relative_to(base: &Path, path: &Path) -> String {
    let normalized_base = canonical_or_normalized(base);
    let normalized_path = canonical_or_normalized(path);
    normalized_path
        .strip_prefix(&normalized_base)
        .unwrap_or(&normalized_path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn canonical_or_normalized(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn strip_jsonc(source: &str) -> String {
    strip_trailing_commas(&strip_jsonc_comments(source))
}

fn strip_jsonc_comments(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                        }
                        if previous == '*' && next == '/' {
                            break;
                        }
                        previous = next;
                    }
                }
                _ => output.push(ch),
            }
            continue;
        }

        output.push(ch);
    }

    output
}

fn strip_trailing_commas(source: &str) -> String {
    let chars = source.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(source.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            index += 1;
            continue;
        }

        if ch == ',' {
            let mut next = index + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next < chars.len() && matches!(chars[next], '}' | ']') {
                index += 1;
                continue;
            }
        }

        output.push(ch);
        index += 1;
    }

    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::TsconfigMembershipCache;

    fn write(path: &std::path::Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn skips_file_excluded_by_nearest_tsconfig() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("tsconfig.json"),
            r#"{
              "include": ["src/**/*.ts"],
              "exclude": ["src/**/*.test.ts"],
            }"#,
        );
        let test_file = root.join("src/foo.test.ts");
        let src_file = root.join("src/foo.ts");
        write(&test_file, "test('x', () => {});\n");
        write(&src_file, "export const x = 1;\n");

        let mut cache = TsconfigMembershipCache::new();
        assert!(cache.should_skip_diagnostics(&test_file));
        assert!(!cache.should_skip_diagnostics(&src_file));
    }

    #[test]
    fn files_are_not_subject_to_exclude() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("tsconfig.json"),
            r#"{
              "files": ["src/foo.test.ts"],
              "exclude": ["src/**/*.test.ts"]
            }"#,
        );
        let test_file = root.join("src/foo.test.ts");
        write(&test_file, "export const x = 1;\n");

        let mut cache = TsconfigMembershipCache::new();
        assert!(!cache.should_skip_diagnostics(&test_file));
    }

    #[test]
    fn files_only_tsconfig_excludes_unlisted_files() {
        // `files` present + `include` absent → ONLY the listed files are members;
        // `include` must NOT default to **/*. An unlisted sibling should be
        // skipped (not counted in diagnostics).
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("tsconfig.json"),
            r#"{
              "files": ["src/entry.ts"]
            }"#,
        );
        let listed = root.join("src/entry.ts");
        let unlisted = root.join("src/other.ts");
        write(&listed, "export const a = 1;\n");
        write(&unlisted, "export const b = 2;\n");

        let mut cache = TsconfigMembershipCache::new();
        // Listed file is a member (not skipped).
        assert!(!cache.should_skip_diagnostics(&listed));
        // Unlisted file is NOT a member → skipped.
        assert!(cache.should_skip_diagnostics(&unlisted));
    }

    #[test]
    fn malformed_tsconfig_falls_through() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("tsconfig.json"), "{ not valid jsonc");
        let file = root.join("src/foo.test.ts");
        write(&file, "export const x = 1;\n");

        let mut cache = TsconfigMembershipCache::new();
        assert!(!cache.should_skip_diagnostics(&file));
    }
}
