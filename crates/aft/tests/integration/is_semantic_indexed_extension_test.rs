use std::path::Path;

use aft::parser::detect_language;
use aft::semantic_index::is_semantic_indexed_extension;

#[test]
fn semantic_extension_policy_stays_in_sync_with_parser_code_arms() {
    let code_extensions = [
        "ts", "tsx", "js", "jsx", "py", "rs", "go", "c", "h", "cc", "cpp", "cxx", "hpp", "hh",
        "zig", "cs", "sh", "bash", "zsh", "sol", "vue", "pas", "pp", "dpr", "dpk", "lpr", "java",
        "kt", "kts", "rb", "swift", "scala", "sc", "lua", "pl", "pm", "t",
    ];
    for extension in code_extensions {
        let path = format!("fixture.{extension}");
        assert!(
            detect_language(Path::new(&path)).is_some(),
            "parser arm for {extension}"
        );
        assert!(
            is_semantic_indexed_extension(Path::new(&path)),
            "semantic indexed extension for {extension}"
        );
    }

    let parser_only_code_extensions = ["R", "r"];
    for extension in parser_only_code_extensions {
        let path = format!("fixture.{extension}");
        assert!(
            detect_language(Path::new(&path)).is_some(),
            "parser arm for {extension}"
        );
        assert!(
            !is_semantic_indexed_extension(Path::new(&path)),
            "R is outline/zoom/ast-grep only, not semantic-indexed ({extension})"
        );
    }

    let doc_extensions = ["md", "markdown", "mdx", "html", "htm"];
    for extension in doc_extensions {
        let path = format!("fixture.{extension}");
        assert!(
            detect_language(Path::new(&path)).is_some(),
            "parser arm for {extension}"
        );
        assert!(
            !is_semantic_indexed_extension(Path::new(&path)),
            "docs/html excluded for {extension}"
        );
    }

    assert!(!is_semantic_indexed_extension(Path::new("package.json")));
}
