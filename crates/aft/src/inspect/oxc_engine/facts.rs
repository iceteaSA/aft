use std::collections::BTreeSet;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{SourceType, Span};
use rustc_hash::FxHashSet;

use super::types::{
    DynamicImportFact, ExportFact, ExportName, FileFacts, FileId, ImportFact, ImportKind,
    ReExportFact, ReExportKind,
};

#[derive(Debug, Clone)]
struct PendingLocalExportSpecifier {
    local_name: String,
    exported_name: String,
    is_type_only: bool,
    line: u32,
}

#[derive(Default)]
struct Extractor {
    exports: Vec<ExportFact>,
    imports: Vec<ImportFact>,
    re_exports: Vec<ReExportFact>,
    dynamic_imports: Vec<DynamicImportFact>,
    local_declaration_names: FxHashSet<String>,
    pending_local_export_specifiers: Vec<PendingLocalExportSpecifier>,
    identifier_references: Vec<String>,
    line_index: LineIndex,
}

#[derive(Default)]
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        for (idx, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(idx + 1);
            }
        }
        Self { starts }
    }

    fn line_for_span(&self, span: Span) -> u32 {
        let offset = span.start as usize;
        match self.starts.binary_search(&offset) {
            Ok(idx) => (idx + 1) as u32,
            Err(idx) => idx as u32,
        }
        .max(1)
    }
}

pub fn parse_file_facts(
    file_id: FileId,
    path: &Path,
    source: &str,
    content_hash: String,
    source_type: SourceType,
) -> FileFacts {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        let joined = parsed
            .errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        return FileFacts::empty(file_id, path.to_path_buf(), content_hash, joined);
    }

    let mut extractor = Extractor {
        line_index: LineIndex::new(source),
        ..Extractor::default()
    };
    extractor.visit_program(&parsed.program);
    extractor.resolve_pending_local_export_specifiers();

    let semantic_usage = compute_semantic_usage(&parsed.program, &extractor.imports);
    let same_file_value_references = extractor.same_file_export_value_references();

    FileFacts {
        file_id,
        path: path.to_path_buf(),
        content_hash,
        exports: extractor.exports,
        imports: extractor.imports,
        re_exports: extractor.re_exports,
        dynamic_imports: extractor.dynamic_imports,
        same_file_value_references,
        used_import_bindings: semantic_usage.used,
        type_referenced_import_bindings: semantic_usage.type_referenced,
        value_referenced_import_bindings: semantic_usage.value_referenced,
        parse_error: None,
    }
}

impl Extractor {
    fn resolve_pending_local_export_specifiers(&mut self) {
        let pending = std::mem::take(&mut self.pending_local_export_specifiers);
        for spec in pending {
            let matching_import = if self.local_declaration_names.contains(&spec.local_name) {
                None
            } else {
                self.imports.iter().find(|import| {
                    import.local_name.as_deref() == Some(spec.local_name.as_str())
                        && matches!(import.kind, ImportKind::Named | ImportKind::Default)
                })
            };

            if let Some(import) = matching_import {
                let imported_name = match import.kind {
                    ImportKind::Named => import.imported_name.clone().unwrap_or_default(),
                    ImportKind::Default => "default".to_string(),
                    ImportKind::Namespace | ImportKind::SideEffect => continue,
                };
                self.re_exports.push(ReExportFact {
                    source: import.source.clone(),
                    kind: ReExportKind::Named,
                    imported_name: Some(imported_name),
                    exported_name: Some(spec.exported_name),
                    is_type_only: spec.is_type_only || import.is_type_only,
                    line: spec.line,
                });
            } else {
                self.exports.push(ExportFact {
                    name: ExportName::Named(spec.exported_name),
                    local_name: Some(spec.local_name),
                    kind: "value".to_string(),
                    is_type_only: spec.is_type_only,
                    line: spec.line,
                    declared: true,
                });
            }
        }
    }

    fn same_file_export_value_references(&self) -> BTreeSet<String> {
        let exported_locals = self
            .exports
            .iter()
            .filter_map(|export| export.local_name.as_deref())
            .collect::<FxHashSet<_>>();
        self.identifier_references
            .iter()
            .filter(|name| exported_locals.contains(name.as_str()))
            .cloned()
            .collect()
    }

    fn extract_declaration_exports(&mut self, decl: &Declaration<'_>, is_type_only: bool) {
        match decl {
            Declaration::VariableDeclaration(var) => {
                for declarator in &var.declarations {
                    self.extract_binding_pattern_names(&declarator.id, is_type_only, "variable");
                }
            }
            Declaration::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    self.push_named_export(
                        &id.name,
                        Some(id.name.to_string()),
                        "function",
                        is_type_only,
                        id.span,
                    );
                }
            }
            Declaration::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    self.push_named_export(
                        &id.name,
                        Some(id.name.to_string()),
                        "class",
                        is_type_only,
                        id.span,
                    );
                }
            }
            Declaration::TSTypeAliasDeclaration(alias) => {
                self.push_named_export(
                    &alias.id.name,
                    Some(alias.id.name.to_string()),
                    "type",
                    true,
                    alias.id.span,
                );
            }
            Declaration::TSInterfaceDeclaration(iface) => {
                self.push_named_export(
                    &iface.id.name,
                    Some(iface.id.name.to_string()),
                    "interface",
                    true,
                    iface.id.span,
                );
            }
            Declaration::TSEnumDeclaration(enum_decl) => {
                self.push_named_export(
                    &enum_decl.id.name,
                    Some(enum_decl.id.name.to_string()),
                    "enum",
                    is_type_only,
                    enum_decl.id.span,
                );
            }
            Declaration::TSModuleDeclaration(module) => match &module.id {
                TSModuleDeclarationName::Identifier(id) => {
                    self.push_named_export(
                        &id.name,
                        Some(id.name.to_string()),
                        "namespace",
                        module.declare || is_type_only,
                        id.span,
                    );
                }
                TSModuleDeclarationName::StringLiteral(lit) => {
                    self.push_named_export(
                        &lit.value,
                        Some(lit.value.to_string()),
                        "namespace",
                        module.declare || is_type_only,
                        lit.span,
                    );
                }
            },
            _ => {}
        }
    }

    fn extract_binding_pattern_names(
        &mut self,
        pattern: &BindingPattern<'_>,
        is_type_only: bool,
        kind: &str,
    ) {
        for id in pattern.get_binding_identifiers() {
            self.push_named_export(
                &id.name,
                Some(id.name.to_string()),
                kind,
                is_type_only,
                id.span,
            );
        }
    }

    fn push_named_export(
        &mut self,
        name: &str,
        local_name: Option<String>,
        kind: &str,
        is_type_only: bool,
        span: Span,
    ) {
        self.exports.push(ExportFact {
            name: ExportName::Named(name.to_string()),
            local_name,
            kind: kind.to_string(),
            is_type_only,
            line: self.line_index.line_for_span(span),
            declared: true,
        });
    }

    fn record_declaration_names(&mut self, decl: &Declaration<'_>) {
        match decl {
            Declaration::VariableDeclaration(var) => {
                for declarator in &var.declarations {
                    for id in declarator.id.get_binding_identifiers() {
                        self.local_declaration_names.insert(id.name.to_string());
                    }
                }
            }
            Declaration::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    self.local_declaration_names.insert(id.name.to_string());
                }
            }
            Declaration::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    self.local_declaration_names.insert(id.name.to_string());
                }
            }
            Declaration::TSTypeAliasDeclaration(alias) => {
                self.local_declaration_names
                    .insert(alias.id.name.to_string());
            }
            Declaration::TSInterfaceDeclaration(iface) => {
                self.local_declaration_names
                    .insert(iface.id.name.to_string());
            }
            Declaration::TSEnumDeclaration(enum_decl) => {
                self.local_declaration_names
                    .insert(enum_decl.id.name.to_string());
            }
            Declaration::TSModuleDeclaration(module) => match &module.id {
                TSModuleDeclarationName::Identifier(id) => {
                    self.local_declaration_names.insert(id.name.to_string());
                }
                TSModuleDeclarationName::StringLiteral(lit) => {
                    self.local_declaration_names.insert(lit.value.to_string());
                }
            },
            _ => {}
        }
    }
}

impl<'a> Visit<'a> for Extractor {
    fn visit_declaration(&mut self, decl: &Declaration<'a>) {
        self.record_declaration_names(decl);
        walk::walk_declaration(self, decl);
    }

    fn visit_identifier_reference(&mut self, ident: &IdentifierReference<'a>) {
        self.identifier_references.push(ident.name.to_string());
        walk::walk_identifier_reference(self, ident);
    }

    fn visit_import_declaration(&mut self, decl: &ImportDeclaration<'a>) {
        let source = decl.source.value.to_string();
        let is_type_only = decl.import_kind.is_type();
        let line = self.line_index.line_for_span(decl.source.span);

        if let Some(specifiers) = &decl.specifiers {
            for spec in specifiers {
                match spec {
                    ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                        self.imports.push(ImportFact {
                            source: source.clone(),
                            kind: ImportKind::Named,
                            imported_name: Some(specifier.imported.name().to_string()),
                            local_name: Some(specifier.local.name.to_string()),
                            is_type_only: is_type_only || specifier.import_kind.is_type(),
                            line,
                        });
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                        self.imports.push(ImportFact {
                            source: source.clone(),
                            kind: ImportKind::Default,
                            imported_name: Some("default".to_string()),
                            local_name: Some(specifier.local.name.to_string()),
                            is_type_only,
                            line,
                        });
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                        self.imports.push(ImportFact {
                            source: source.clone(),
                            kind: ImportKind::Namespace,
                            imported_name: Some("*".to_string()),
                            local_name: Some(specifier.local.name.to_string()),
                            is_type_only,
                            line,
                        });
                    }
                }
            }
        } else {
            self.imports.push(ImportFact {
                source,
                kind: ImportKind::SideEffect,
                imported_name: None,
                local_name: None,
                is_type_only: false,
                line,
            });
        }
    }

    fn visit_export_named_declaration(&mut self, decl: &ExportNamedDeclaration<'a>) {
        let is_type_only = decl.export_kind.is_type();
        if let Some(source) = &decl.source {
            for spec in &decl.specifiers {
                self.re_exports.push(ReExportFact {
                    source: source.value.to_string(),
                    kind: ReExportKind::Named,
                    imported_name: Some(spec.local.name().to_string()),
                    exported_name: Some(spec.exported.name().to_string()),
                    is_type_only: is_type_only || spec.export_kind.is_type(),
                    line: self.line_index.line_for_span(spec.span),
                });
            }
        } else {
            if let Some(declaration) = &decl.declaration {
                self.extract_declaration_exports(declaration, is_type_only);
            }
            for spec in &decl.specifiers {
                self.pending_local_export_specifiers
                    .push(PendingLocalExportSpecifier {
                        local_name: spec.local.name().to_string(),
                        exported_name: spec.exported.name().to_string(),
                        is_type_only: is_type_only || spec.export_kind.is_type(),
                        line: self.line_index.line_for_span(spec.span),
                    });
            }
        }
        walk::walk_export_named_declaration(self, decl);
    }

    fn visit_export_default_declaration(&mut self, decl: &ExportDefaultDeclaration<'a>) {
        let (local_name, kind) = match &decl.declaration {
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                (class.id.as_ref().map(|id| id.name.to_string()), "class")
            }
            ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                (func.id.as_ref().map(|id| id.name.to_string()), "function")
            }
            _ => (None, "default"),
        };
        self.exports.push(ExportFact {
            name: ExportName::Default,
            local_name,
            kind: kind.to_string(),
            is_type_only: false,
            line: self.line_index.line_for_span(decl.span),
            declared: true,
        });
        walk::walk_export_default_declaration(self, decl);
    }

    fn visit_export_all_declaration(&mut self, decl: &ExportAllDeclaration<'a>) {
        if let Some(exported) = &decl.exported {
            self.re_exports.push(ReExportFact {
                source: decl.source.value.to_string(),
                kind: ReExportKind::Namespace,
                imported_name: Some("*".to_string()),
                exported_name: Some(exported.name().to_string()),
                is_type_only: decl.export_kind.is_type(),
                line: self.line_index.line_for_span(decl.span),
            });
        } else {
            self.re_exports.push(ReExportFact {
                source: decl.source.value.to_string(),
                kind: ReExportKind::Star,
                imported_name: Some("*".to_string()),
                exported_name: None,
                is_type_only: decl.export_kind.is_type(),
                line: self.line_index.line_for_span(decl.span),
            });
        }
        walk::walk_export_all_declaration(self, decl);
    }

    fn visit_import_expression(&mut self, expr: &ImportExpression<'a>) {
        let source = match &expr.source {
            Expression::StringLiteral(lit) => Some(lit.value.to_string()),
            _ => None,
        };
        self.dynamic_imports.push(DynamicImportFact {
            is_literal: source.is_some(),
            source,
            line: self.line_index.line_for_span(expr.span),
        });
        walk::walk_import_expression(self, expr);
    }
}

#[derive(Default)]
struct SemanticUsage {
    used: BTreeSet<String>,
    type_referenced: BTreeSet<String>,
    value_referenced: BTreeSet<String>,
}

fn compute_semantic_usage(program: &Program<'_>, imports: &[ImportFact]) -> SemanticUsage {
    let semantic = SemanticBuilder::new().build(program).semantic;
    let scoping = semantic.scoping();
    let root_scope = scoping.root_scope_id();
    let mut usage = SemanticUsage::default();

    for import in imports {
        let Some(local_name) = import.local_name.as_deref() else {
            continue;
        };
        if local_name.is_empty() {
            continue;
        }
        let name = oxc_str::Ident::from(local_name);
        let Some(symbol_id) = scoping.get_binding(root_scope, name) else {
            continue;
        };
        for reference in scoping.get_resolved_references(symbol_id) {
            usage.used.insert(local_name.to_string());
            if reference.is_type() {
                usage.type_referenced.insert(local_name.to_string());
            }
            if reference.is_value() {
                usage.value_referenced.insert(local_name.to_string());
            }
        }
    }

    usage
}
