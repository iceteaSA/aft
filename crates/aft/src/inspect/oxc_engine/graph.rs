use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use super::resolver::ResolvedModule;
use super::types::{
    ExportFact, ExportName, FileId, ImportKind, LivenessVerdict, OxcExportVerdict, OxcFileVerdicts,
    ReExportKind, OXC_PROVENANCE,
};

#[derive(Debug, Clone)]
struct ExportState {
    fact: ExportFact,
    status: ExportStatus,
}

#[derive(Debug, Clone)]
enum ExportStatus {
    Used(String),
    Uncertain(String),
    Unused,
}

impl ExportStatus {
    fn verdict(&self) -> (LivenessVerdict, String) {
        match self {
            Self::Used(reason) => (LivenessVerdict::Used, reason.clone()),
            Self::Uncertain(reason) => (LivenessVerdict::Uncertain, reason.clone()),
            Self::Unused => (LivenessVerdict::Unused, "no_references".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
struct ModuleState {
    path: PathBuf,
    exports: Vec<ExportState>,
}

pub fn compute_verdicts(
    project_root: &Path,
    modules: &[ResolvedModule],
    entry_points: &BTreeSet<PathBuf>,
    public_api_files: &BTreeSet<PathBuf>,
    entry_reachability: bool,
) -> Vec<OxcFileVerdicts> {
    let mut graph = GraphBuilder::new(modules, entry_points, public_api_files);
    if entry_reachability {
        graph.apply_entry_reachability();
    } else {
        graph.apply_same_file_references();
        graph.apply_imports();
        graph.apply_re_exports();
        graph.apply_dynamic_imports();
    }
    graph.into_verdicts(project_root)
}

struct GraphBuilder<'a> {
    modules: &'a [ResolvedModule],
    states: Vec<ModuleState>,
    root_modules: BTreeSet<usize>,
}

impl<'a> GraphBuilder<'a> {
    fn new(
        modules: &'a [ResolvedModule],
        entry_points: &BTreeSet<PathBuf>,
        public_api_files: &BTreeSet<PathBuf>,
    ) -> Self {
        let mut root_modules = BTreeSet::new();
        let states = modules
            .iter()
            .map(|module| {
                let mut exports = module
                    .facts
                    .exports
                    .iter()
                    .cloned()
                    .map(|fact| ExportState {
                        fact,
                        status: ExportStatus::Unused,
                    })
                    .collect::<Vec<_>>();

                for re_export in &module.re_exports {
                    match re_export.fact.kind {
                        ReExportKind::Named => {
                            if let Some(exported_name) = &re_export.fact.exported_name {
                                push_synthetic_export(
                                    &mut exports,
                                    ExportName::Named(exported_name.clone()),
                                    "re_export",
                                    re_export.fact.is_type_only,
                                    re_export.fact.line,
                                );
                            }
                        }
                        ReExportKind::Namespace => {
                            if let Some(exported_name) = &re_export.fact.exported_name {
                                push_synthetic_export(
                                    &mut exports,
                                    ExportName::Named(exported_name.clone()),
                                    "namespace",
                                    re_export.fact.is_type_only,
                                    re_export.fact.line,
                                );
                            }
                        }
                        ReExportKind::Star => {}
                    }
                }

                let mut state = ModuleState {
                    path: module.facts.path.clone(),
                    exports,
                };
                if entry_points.contains(&module.facts.path)
                    || public_api_files.contains(&module.facts.path)
                {
                    root_modules.insert(module.facts.file_id.0);
                    for export in &mut state.exports {
                        mark_used(export, "entry_point");
                    }
                }
                state
            })
            .collect::<Vec<_>>();
        Self {
            modules,
            states,
            root_modules,
        }
    }

    fn apply_same_file_references(&mut self) {
        for idx in 0..self.modules.len() {
            self.apply_same_file_references_for_module(idx);
        }
    }

    fn apply_same_file_references_for_module(&mut self, idx: usize) {
        let mut newly_live_modules = BTreeSet::new();
        self.apply_same_file_references_for_module_collect(idx, &mut newly_live_modules);
    }

    fn apply_same_file_references_for_module_collect(
        &mut self,
        idx: usize,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        let Some(module) = self.modules.get(idx) else {
            return;
        };
        if module.facts.same_file_value_references.is_empty() {
            return;
        }
        let Some(state) = self.states.get_mut(idx) else {
            return;
        };
        for export in &mut state.exports {
            let Some(local_name) = export.fact.local_name.as_deref() else {
                continue;
            };
            if module.facts.same_file_value_references.contains(local_name)
                && mark_used(export, "same_file_value_reference")
            {
                newly_live_modules.insert(idx);
            }
        }
    }

    fn apply_entry_reachability(&mut self) {
        let mut live_modules = self.root_modules.clone();
        let mut queue = live_modules.iter().copied().collect::<VecDeque<_>>();

        while let Some(module_idx) = queue.pop_front() {
            let mut newly_live_modules = BTreeSet::new();
            self.apply_same_file_references_for_module_collect(module_idx, &mut newly_live_modules);
            enqueue_newly_live_modules(newly_live_modules, &mut live_modules, &mut queue);

            let Some(module) = self.modules.get(module_idx).cloned() else {
                continue;
            };

            for import in &module.imports {
                let Some(target) = import.target else {
                    continue;
                };
                if !import_binding_is_used(&module, import) {
                    continue;
                }
                let mut newly_live_modules = BTreeSet::new();
                match import.fact.kind {
                    ImportKind::Named => {
                        if let Some(name) = import.fact.imported_name.as_deref() {
                            let mut visited = BTreeSet::new();
                            self.mark_imported_name_collect(
                                target,
                                name,
                                "import",
                                &mut visited,
                                &mut newly_live_modules,
                            );
                        }
                    }
                    ImportKind::Default => {
                        let mut visited = BTreeSet::new();
                        self.mark_imported_name_collect(
                            target,
                            "default",
                            "import",
                            &mut visited,
                            &mut newly_live_modules,
                        );
                    }
                    ImportKind::Namespace => {
                        let mut visited = BTreeSet::new();
                        self.mark_all_uncertain_collect(
                            target,
                            "namespace_import",
                            &mut visited,
                            &mut newly_live_modules,
                        );
                    }
                    ImportKind::SideEffect => {}
                }
                enqueue_module(target, &mut live_modules, &mut queue);
                enqueue_newly_live_modules(newly_live_modules, &mut live_modules, &mut queue);
            }

            for re_export in &module.re_exports {
                let Some(target) = re_export.target else {
                    continue;
                };
                match re_export.fact.kind {
                    ReExportKind::Named => {
                        let exported_name = re_export.fact.exported_name.as_deref();
                        let exported_is_live = exported_name.is_some_and(|name| {
                            self.export_status(module.facts.file_id, name)
                                .is_some_and(|status| {
                                    matches!(
                                        status,
                                        ExportStatus::Used(_) | ExportStatus::Uncertain(_)
                                    )
                                })
                        });
                        if exported_is_live {
                            if let Some(imported_name) = re_export.fact.imported_name.as_deref() {
                                let mut visited = BTreeSet::new();
                                let mut newly_live_modules = BTreeSet::new();
                                self.mark_imported_name_collect(
                                    target,
                                    imported_name,
                                    "re_export",
                                    &mut visited,
                                    &mut newly_live_modules,
                                );
                                enqueue_module(target, &mut live_modules, &mut queue);
                                enqueue_newly_live_modules(
                                    newly_live_modules,
                                    &mut live_modules,
                                    &mut queue,
                                );
                            }
                        }
                    }
                    ReExportKind::Star => {
                        if self.root_modules.contains(&module_idx) {
                            let mut visited = BTreeSet::new();
                            let mut newly_live_modules = BTreeSet::new();
                            self.mark_all_uncertain_collect(
                                target,
                                "wildcard_import",
                                &mut visited,
                                &mut newly_live_modules,
                            );
                            enqueue_module(target, &mut live_modules, &mut queue);
                            enqueue_newly_live_modules(
                                newly_live_modules,
                                &mut live_modules,
                                &mut queue,
                            );
                        }
                    }
                    ReExportKind::Namespace => {
                        let exported_name = re_export.fact.exported_name.as_deref();
                        let namespace_is_live = exported_name.is_some_and(|name| {
                            self.export_status(module.facts.file_id, name)
                                .is_some_and(|status| {
                                    matches!(
                                        status,
                                        ExportStatus::Used(_) | ExportStatus::Uncertain(_)
                                    )
                                })
                        });
                        if namespace_is_live {
                            let mut visited = BTreeSet::new();
                            let mut newly_live_modules = BTreeSet::new();
                            self.mark_all_uncertain_collect(
                                target,
                                "namespace_import",
                                &mut visited,
                                &mut newly_live_modules,
                            );
                            enqueue_module(target, &mut live_modules, &mut queue);
                            enqueue_newly_live_modules(
                                newly_live_modules,
                                &mut live_modules,
                                &mut queue,
                            );
                        }
                    }
                }
            }

            for dynamic in &module.dynamic_imports {
                if dynamic.fact.is_literal {
                    if let Some(target) = dynamic.target {
                        let mut visited = BTreeSet::new();
                        let mut newly_live_modules = BTreeSet::new();
                        self.mark_all_uncertain_collect(
                            target,
                            "dynamic_import",
                            &mut visited,
                            &mut newly_live_modules,
                        );
                        enqueue_module(target, &mut live_modules, &mut queue);
                        enqueue_newly_live_modules(
                            newly_live_modules,
                            &mut live_modules,
                            &mut queue,
                        );
                    }
                }
            }
        }
    }

    fn apply_imports(&mut self) {
        for module in self.modules {
            for import in &module.imports {
                let Some(target) = import.target else {
                    continue;
                };
                if !import_binding_is_used(module, import) {
                    continue;
                }
                match import.fact.kind {
                    ImportKind::Named => {
                        if let Some(name) = import.fact.imported_name.as_deref() {
                            let mut visited = BTreeSet::new();
                            self.mark_imported_name(target, name, "import", &mut visited);
                        }
                    }
                    ImportKind::Default => {
                        let mut visited = BTreeSet::new();
                        self.mark_imported_name(target, "default", "import", &mut visited);
                    }
                    ImportKind::Namespace => {
                        let mut visited = BTreeSet::new();
                        self.mark_all_uncertain(target, "namespace_import", &mut visited);
                    }
                    ImportKind::SideEffect => {}
                }
            }
        }
    }

    fn apply_re_exports(&mut self) {
        for module in self.modules {
            for re_export in &module.re_exports {
                let Some(target) = re_export.target else {
                    continue;
                };
                match re_export.fact.kind {
                    ReExportKind::Named => {
                        if let Some(imported_name) = re_export.fact.imported_name.as_deref() {
                            let mut visited = BTreeSet::new();
                            self.mark_imported_name(
                                target,
                                imported_name,
                                "re_export",
                                &mut visited,
                            );
                        }
                    }
                    ReExportKind::Star => {
                        let mut visited = BTreeSet::new();
                        self.mark_all_uncertain(target, "wildcard_import", &mut visited);
                    }
                    ReExportKind::Namespace => {
                        let exported_name = re_export.fact.exported_name.as_deref();
                        let namespace_is_used = exported_name.is_some_and(|name| {
                            self.export_status(module.facts.file_id, name)
                                .is_some_and(|status| matches!(status, ExportStatus::Used(_)))
                        });
                        if namespace_is_used {
                            let mut visited = BTreeSet::new();
                            self.mark_all_uncertain(target, "namespace_import", &mut visited);
                        }
                    }
                }
            }
        }
    }

    fn apply_dynamic_imports(&mut self) {
        let mut literal_targets = Vec::new();
        for module in self.modules {
            for dynamic in &module.dynamic_imports {
                if dynamic.fact.is_literal {
                    if let Some(target) = dynamic.target {
                        literal_targets.push(target);
                    }
                }
            }
        }
        for target in literal_targets {
            let mut visited = BTreeSet::new();
            self.mark_all_uncertain(target, "dynamic_import", &mut visited);
        }
    }

    fn mark_imported_name(
        &mut self,
        target: FileId,
        name: &str,
        reason: &str,
        visited: &mut BTreeSet<(usize, String)>,
    ) {
        let mut newly_live_modules = BTreeSet::new();
        self.mark_imported_name_collect(target, name, reason, visited, &mut newly_live_modules);
    }

    fn mark_imported_name_collect(
        &mut self,
        target: FileId,
        name: &str,
        reason: &str,
        visited: &mut BTreeSet<(usize, String)>,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        if !visited.insert((target.0, name.to_string())) {
            return;
        }
        if let Some(state) = self.states.get_mut(target.0) {
            for export in state
                .exports
                .iter_mut()
                .filter(|export| export.fact.name.matches_str(name))
            {
                if mark_used(export, reason) {
                    newly_live_modules.insert(target.0);
                }
            }
        }

        let Some(module) = self.modules.get(target.0) else {
            return;
        };
        let re_exports = module.re_exports.clone();
        for re_export in re_exports {
            let Some(source) = re_export.target else {
                continue;
            };
            match re_export.fact.kind {
                ReExportKind::Named => {
                    if re_export.fact.exported_name.as_deref() == Some(name) {
                        if let Some(imported_name) = re_export.fact.imported_name.as_deref() {
                            self.mark_imported_name_collect(
                                source,
                                imported_name,
                                "re_export",
                                visited,
                                newly_live_modules,
                            );
                        }
                    }
                }
                ReExportKind::Star => {
                    self.mark_imported_name_collect(
                        source,
                        name,
                        "re_export",
                        visited,
                        newly_live_modules,
                    );
                }
                ReExportKind::Namespace => {
                    if re_export.fact.exported_name.as_deref() == Some(name) {
                        let mut uncertain_visited = BTreeSet::new();
                        self.mark_all_uncertain_collect(
                            source,
                            "namespace_import",
                            &mut uncertain_visited,
                            newly_live_modules,
                        );
                    }
                }
            }
        }
    }

    fn mark_imported_name_uncertain_collect(
        &mut self,
        target: FileId,
        name: &str,
        reason: &str,
        visited: &mut BTreeSet<(usize, String)>,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        if !visited.insert((target.0, name.to_string())) {
            return;
        }
        if let Some(state) = self.states.get_mut(target.0) {
            for export in state
                .exports
                .iter_mut()
                .filter(|export| export.fact.name.matches_str(name))
            {
                if mark_uncertain(export, reason) {
                    newly_live_modules.insert(target.0);
                }
            }
        }
        let Some(module) = self.modules.get(target.0) else {
            return;
        };
        let re_exports = module.re_exports.clone();
        for re_export in re_exports {
            let Some(source) = re_export.target else {
                continue;
            };
            match re_export.fact.kind {
                ReExportKind::Named => {
                    if re_export.fact.exported_name.as_deref() == Some(name) {
                        if let Some(imported_name) = re_export.fact.imported_name.as_deref() {
                            self.mark_imported_name_uncertain_collect(
                                source,
                                imported_name,
                                reason,
                                visited,
                                newly_live_modules,
                            );
                        }
                    }
                }
                ReExportKind::Star => {
                    self.mark_imported_name_uncertain_collect(
                        source,
                        name,
                        reason,
                        visited,
                        newly_live_modules,
                    );
                }
                ReExportKind::Namespace => {}
            }
        }
    }

    fn mark_all_uncertain(&mut self, target: FileId, reason: &str, visited: &mut BTreeSet<usize>) {
        let mut newly_live_modules = BTreeSet::new();
        self.mark_all_uncertain_collect(target, reason, visited, &mut newly_live_modules);
    }

    fn mark_all_uncertain_collect(
        &mut self,
        target: FileId,
        reason: &str,
        visited: &mut BTreeSet<usize>,
        newly_live_modules: &mut BTreeSet<usize>,
    ) {
        if !visited.insert(target.0) {
            return;
        }
        if let Some(state) = self.states.get_mut(target.0) {
            for export in &mut state.exports {
                if mark_uncertain(export, reason) {
                    newly_live_modules.insert(target.0);
                }
            }
        }
        let Some(module) = self.modules.get(target.0) else {
            return;
        };
        let re_exports = module.re_exports.clone();
        for re_export in re_exports {
            let Some(source) = re_export.target else {
                continue;
            };
            match re_export.fact.kind {
                ReExportKind::Star => {
                    self.mark_all_uncertain_collect(source, reason, visited, newly_live_modules)
                }
                ReExportKind::Named => {
                    if let Some(imported_name) = re_export.fact.imported_name.as_deref() {
                        let mut name_visited = BTreeSet::new();
                        self.mark_imported_name_uncertain_collect(
                            source,
                            imported_name,
                            reason,
                            &mut name_visited,
                            newly_live_modules,
                        );
                    }
                }
                ReExportKind::Namespace => {}
            }
        }
    }

    fn export_status(&self, file_id: FileId, name: &str) -> Option<&ExportStatus> {
        self.states.get(file_id.0).and_then(|state| {
            state
                .exports
                .iter()
                .find(|export| export.fact.name.matches_str(name))
                .map(|export| &export.status)
        })
    }

    fn into_verdicts(self, project_root: &Path) -> Vec<OxcFileVerdicts> {
        self.states
            .into_iter()
            .map(|state| {
                let relative_file = relative_string(project_root, &state.path);
                let mut seen = BTreeSet::new();
                let exports = state
                    .exports
                    .into_iter()
                    .filter(|export| seen.insert(export.fact.name.as_symbol()))
                    .map(|export| {
                        let (verdict, reason) = export.status.verdict();
                        OxcExportVerdict {
                            symbol: export.fact.name.as_symbol(),
                            kind: export.fact.kind,
                            line: export.fact.line,
                            verdict,
                            reason,
                            provenance: OXC_PROVENANCE.to_string(),
                        }
                    })
                    .collect::<Vec<_>>();
                OxcFileVerdicts {
                    file: state.path,
                    relative_file,
                    exports,
                }
            })
            .collect()
    }
}

fn enqueue_newly_live_modules(
    newly_live_modules: BTreeSet<usize>,
    live_modules: &mut BTreeSet<usize>,
    queue: &mut VecDeque<usize>,
) {
    for module_idx in newly_live_modules {
        enqueue_module_idx(module_idx, live_modules, queue);
    }
}

fn enqueue_module(target: FileId, live_modules: &mut BTreeSet<usize>, queue: &mut VecDeque<usize>) {
    enqueue_module_idx(target.0, live_modules, queue);
}

fn enqueue_module_idx(
    module_idx: usize,
    live_modules: &mut BTreeSet<usize>,
    queue: &mut VecDeque<usize>,
) {
    if live_modules.insert(module_idx) {
        queue.push_back(module_idx);
    }
}

fn import_binding_is_used(
    module: &ResolvedModule,
    import: &super::resolver::ResolvedImport,
) -> bool {
    match import.fact.kind {
        ImportKind::SideEffect => false,
        ImportKind::Named | ImportKind::Default | ImportKind::Namespace => import
            .fact
            .local_name
            .as_ref()
            .is_some_and(|local| module.facts.used_import_bindings.contains(local)),
    }
}

fn push_synthetic_export(
    exports: &mut Vec<ExportState>,
    name: ExportName,
    kind: &str,
    is_type_only: bool,
    line: u32,
) {
    if exports.iter().any(|export| export.fact.name == name) {
        return;
    }
    exports.push(ExportState {
        fact: ExportFact {
            name,
            local_name: None,
            kind: kind.to_string(),
            is_type_only,
            line,
            declared: true,
        },
        status: ExportStatus::Unused,
    });
}

fn mark_used(export: &mut ExportState, reason: &str) -> bool {
    match export.status {
        ExportStatus::Used(_) => false,
        ExportStatus::Uncertain(_) => {
            export.status = ExportStatus::Used(reason.to_string());
            false
        }
        ExportStatus::Unused => {
            export.status = ExportStatus::Used(reason.to_string());
            true
        }
    }
}

fn mark_uncertain(export: &mut ExportState, reason: &str) -> bool {
    if matches!(export.status, ExportStatus::Unused) {
        export.status = ExportStatus::Uncertain(reason.to_string());
        true
    } else {
        false
    }
}

fn relative_string(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
