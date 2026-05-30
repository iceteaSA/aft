use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

pub(crate) fn classify_group_php(_module_path: &str) -> ImportGroup {
    ImportGroup::External
}

pub(crate) fn parse_php_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    collect_php_imports(source, root, &mut imports);
    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

fn collect_php_imports(source: &str, node: Node, imports: &mut Vec<ImportStatement>) {
    if node.kind() == "namespace_use_declaration" {
        if let Some(imp) = parse_php_namespace_use_declaration(source, &node) {
            imports.push(imp);
        }
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_php_imports(source, cursor.node(), imports);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn parse_php_namespace_use_declaration(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    let clause = find_direct_child(node, "namespace_use_clause")?;
    let (module_path, alias, import_kind) = parse_php_namespace_use_clause(source, &clause)?;
    let names = Vec::new();
    let group = classify_group_php(&module_path);

    Some(ImportStatement {
        module_path,
        names: names.clone(),
        default_import: None,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: names,
            namespace: None,
            alias,
            modifiers: vec![],
            import_kind,
        },
    })
}

fn parse_php_namespace_use_clause(
    source: &str,
    node: &Node,
) -> Option<(String, Option<String>, Option<String>)> {
    let mut module_path: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut import_kind: Option<String> = None;
    let mut saw_as = false;
    let mut leading_absolute = false;

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let text = source[child.byte_range()].trim();
            match child.kind() {
                "function" | "const" => import_kind = Some(text.to_string()),
                "\\" if module_path.is_none() => leading_absolute = true,
                "qualified_name" => {
                    if module_path.is_none() {
                        module_path = Some(text.to_string());
                    }
                }
                "name" => {
                    if saw_as {
                        alias = Some(text.to_string());
                    } else if module_path.is_none() {
                        module_path = Some(text.to_string());
                    }
                }
                "as" => saw_as = true,
                _ => {}
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let mut module_path = module_path?;
    if leading_absolute && !module_path.starts_with('\\') {
        module_path.insert(0, '\\');
    }
    if module_path.is_empty() {
        return None;
    }

    Some((module_path, alias, import_kind))
}

fn find_direct_child<'tree>(node: &Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == kind {
                return Some(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

pub(crate) fn generate_php_import_line(req: &ImportRequest) -> String {
    let mut line = String::from("use ");
    if let Some(kind) = req.import_kind {
        if !kind.is_empty() {
            line.push_str(kind);
            line.push(' ');
        }
    }
    line.push_str(req.module_path);
    if let Some(alias) = req.alias {
        if !alias.is_empty() {
            line.push_str(" as ");
            line.push_str(alias);
        }
    }
    line.push(';');
    line
}

pub struct PhpSyntax;

impl ImportSyntax for PhpSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_php_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_php_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_php(module_path)
    }
}

pub static PHP_SYNTAX: PhpSyntax = PhpSyntax;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};

    fn parse_php(src: &str) -> (Tree, ImportBlock) {
        let g = grammar_for(LangId::Php);
        let mut p = tree_sitter::Parser::new();
        p.set_language(&g).unwrap();
        let tree = p.parse(src, None).unwrap();
        let block = parse_imports(src, &tree, LangId::Php);
        (tree, block)
    }

    #[test]
    fn php_grammar_node_kinds_are_stable() {
        let src = "<?php\nuse App\\Foo;\nuse App\\Foo as Bar;\nuse function App\\helper;\nuse const App\\VERSION;\nuse App\\{Foo, Bar as Baz};\n";
        let (tree, _) = parse_php(src);
        let mut kinds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        fn walk(node: Node, kinds: &mut std::collections::BTreeSet<String>) {
            kinds.insert(node.kind().to_string());
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    walk(cursor.node(), kinds);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        walk(tree.root_node(), &mut kinds);
        for required in [
            "namespace_use_declaration",
            "namespace_use_clause",
            "qualified_name",
            "namespace_name",
            "namespace_use_group",
            "name",
            "as",
            "function",
            "const",
            "\\",
        ] {
            assert!(
                kinds.contains(required),
                "php grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_php_all_supported_forms() {
        let (_, block) = parse_php(
            "<?php\nuse App\\Foo;\nuse App\\Foo as Bar;\nuse function App\\helper;\nuse const App\\VERSION;\n",
        );
        assert_eq!(block.imports.len(), 4);

        assert_php_import(&block.imports[0], "App\\Foo", None, None);
        assert_php_import(&block.imports[1], "App\\Foo", Some("Bar"), None);
        assert_php_import(&block.imports[2], "App\\helper", None, Some("function"));
        assert_php_import(&block.imports[3], "App\\VERSION", None, Some("const"));
    }

    fn assert_php_import(
        imp: &ImportStatement,
        module_path: &str,
        expected_alias: Option<&str>,
        expected_import_kind: Option<&str>,
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert_eq!(imp.default_import, None);
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::Value);
        assert_eq!(imp.group, ImportGroup::External);

        assert_eq!(
            imp.form,
            ImportForm::Structured {
                named: vec![],
                namespace: None,
                alias: expected_alias.map(str::to_string),
                modifiers: vec![],
                import_kind: expected_import_kind.map(str::to_string),
            }
        );
    }

    #[test]
    fn generate_php_all_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Php,
                &ImportRequest::legacy("App\\Foo", &[], None, None, false)
            ),
            "use App\\Foo;"
        );
        assert_eq!(
            generate_import(
                LangId::Php,
                &ImportRequest {
                    module_path: "App\\Foo",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("Bar"),
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                }
            ),
            "use App\\Foo as Bar;"
        );
        assert_eq!(
            generate_import(
                LangId::Php,
                &ImportRequest {
                    module_path: "App\\helper",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some("function"),
                }
            ),
            "use function App\\helper;"
        );
        assert_eq!(
            generate_import(
                LangId::Php,
                &ImportRequest {
                    module_path: "App\\VERSION",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some("const"),
                }
            ),
            "use const App\\VERSION;"
        );
    }

    #[test]
    fn classify_group_php_always_external() {
        assert_eq!(classify_group_php("App\\Foo"), ImportGroup::External);
        assert_eq!(classify_group_php("\\App\\Foo"), ImportGroup::External);
        assert_eq!(classify_group_php("Vendor\\Package"), ImportGroup::External);
    }

    #[test]
    fn php_round_trips_through_parse_generate() {
        for src in [
            "use App\\Foo;",
            "use App\\Foo as Bar;",
            "use function App\\helper;",
            "use const App\\VERSION;",
        ] {
            let php_src = format!("<?php\n{src}\n");
            let (_, block) = parse_php(&php_src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (alias, import_kind) = match &imp.form {
                ImportForm::Structured {
                    alias, import_kind, ..
                } => (alias.as_deref(), import_kind.as_deref()),
                other => panic!("expected PHP structured form, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Php,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: None,
                    namespace: None,
                    alias,
                    type_only: false,
                    modifiers: &[],
                    import_kind,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
