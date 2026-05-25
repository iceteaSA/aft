use std::path::{Path, PathBuf};

use aft::language::LanguageProvider;
use aft::parser::{detect_language, FileParser, LangId, TreeSitterProvider};
use aft::symbols::{Symbol, SymbolKind};

fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(&path, content).expect("write file");
    path
}

fn extract(file: &Path) -> Vec<Symbol> {
    FileParser::new()
        .extract_symbols(file)
        .expect("extract symbols")
}

fn symbol<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
    symbols
        .iter()
        .find(|symbol| symbol.name == name)
        .unwrap_or_else(|| panic!("missing symbol {name}; got {symbols:?}"))
}

#[test]
fn ts_js_anonymous_default_exports_surface_and_resolve_as_default() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let ts = write_file(
        tmp.path(),
        "anonymous.ts",
        "export default function () { return 1; }\n",
    );
    let js = write_file(
        tmp.path(),
        "anonymous.js",
        "export default class { run() {} }\n",
    );

    for (file, expected_kind) in [(&ts, SymbolKind::Function), (&js, SymbolKind::Class)] {
        let symbols = extract(file);
        let default_symbol = symbol(&symbols, "default");
        assert_eq!(default_symbol.kind, expected_kind);
        assert!(default_symbol.exported);

        let resolved = TreeSitterProvider::new()
            .resolve_symbol(file, "default")
            .expect("resolve default export");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].symbol.name, "default");
        assert_eq!(resolved[0].symbol.kind, expected_kind);
    }
}

#[test]
fn common_module_and_stub_extensions_are_detected() {
    for (file, expected) in [
        ("module.mjs", LangId::JavaScript),
        ("module.cjs", LangId::JavaScript),
        ("module.mts", LangId::TypeScript),
        ("module.cts", LangId::TypeScript),
        ("module.pyi", LangId::Python),
    ] {
        assert_eq!(detect_language(Path::new(file)), Some(expected), "{file}");
    }
}

#[test]
fn ts_js_top_level_function_expression_initializers_are_functions() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let ts = write_file(
        tmp.path(),
        "functions.ts",
        "const fnValue = function () { return 1; };\nconst genValue = function* () { yield 1; };\n",
    );
    let js = write_file(
        tmp.path(),
        "functions.js",
        "const fnValue = function () { return 1; };\nconst genValue = function* () { yield 1; };\n",
    );

    for file in [&ts, &js] {
        let symbols = extract(file);
        assert_eq!(symbol(&symbols, "fnValue").kind, SymbolKind::Function);
        assert_eq!(symbol(&symbols, "genValue").kind, SymbolKind::Function);
    }
}

#[test]
fn solidity_receive_and_fallback_are_synthetic_methods() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = write_file(
        tmp.path(),
        "Vault.sol",
        r#"contract Vault {
    receive() external payable {}
    fallback() external {}
}
"#,
    );

    let symbols = extract(&file);
    let receive = symbol(&symbols, "receive");
    assert_eq!(receive.kind, SymbolKind::Method);
    assert_eq!(receive.parent.as_deref(), Some("Vault"));

    let fallback = symbol(&symbols, "fallback");
    assert_eq!(fallback.kind, SymbolKind::Method);
    assert_eq!(fallback.parent.as_deref(), Some("Vault"));
}

#[test]
fn lua_top_level_local_variables_are_captured() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = write_file(
        tmp.path(),
        "module.lua",
        "local M = {}\nlocal count = 0\nfunction M.run() end\nreturn M\n",
    );

    let symbols = extract(&file);
    assert_eq!(symbol(&symbols, "M").kind, SymbolKind::Variable);
    assert_eq!(symbol(&symbols, "count").kind, SymbolKind::Variable);
    assert_eq!(symbol(&symbols, "run").kind, SymbolKind::Method);
}

#[test]
fn swift_modifier_prefixed_structs_and_enums_keep_their_kind() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = write_file(
        tmp.path(),
        "Model.swift",
        "public struct Box { let value: Int }\nindirect enum Tree { case leaf }\npublic class Worker {}\n",
    );

    let symbols = extract(&file);
    assert_eq!(symbol(&symbols, "Box").kind, SymbolKind::Struct);
    assert_eq!(symbol(&symbols, "Tree").kind, SymbolKind::Enum);
    assert_eq!(symbol(&symbols, "Worker").kind, SymbolKind::Class);
}

#[test]
fn scala3_enums_are_reported_as_enums() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = write_file(
        tmp.path(),
        "Color.scala",
        "enum Color {\n  case Red, Green\n}\n",
    );

    let symbols = extract(&file);
    assert_eq!(symbol(&symbols, "Color").kind, SymbolKind::Enum);
}
