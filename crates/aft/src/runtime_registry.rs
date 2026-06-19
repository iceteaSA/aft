use crate::context::AppContext;

pub type ProjectRuntime = AppContext;

pub struct RuntimeRegistry {
    single: ProjectRuntime,
}

impl RuntimeRegistry {
    pub fn standalone(rt: ProjectRuntime) -> Self {
        Self { single: rt }
    }

    pub fn current(&self) -> &ProjectRuntime {
        &self.single
    }

    pub fn current_mut(&mut self) -> &mut ProjectRuntime {
        &mut self.single
    }

    pub fn iter(&self) -> impl Iterator<Item = &ProjectRuntime> {
        std::iter::once(&self.single)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, parser::TreeSitterProvider};

    #[test]
    fn standalone_current_and_iter_return_single_runtime() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let mut registry = RuntimeRegistry::standalone(ctx);

        let current_ptr = registry.current() as *const ProjectRuntime;
        let iter_ptrs = registry
            .iter()
            .map(|runtime| runtime as *const ProjectRuntime)
            .collect::<Vec<_>>();

        assert_eq!(iter_ptrs, vec![current_ptr]);

        let current_mut_ptr = registry.current_mut() as *mut ProjectRuntime;
        assert_eq!(current_mut_ptr as *const ProjectRuntime, current_ptr);
    }
}
