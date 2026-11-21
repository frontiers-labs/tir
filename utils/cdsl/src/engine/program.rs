use super::*;

mod dependencies;
mod rust;

pub(super) use dependencies::{Dependencies, Selection};

impl CheckedProgram {
    /// Emit a Rust module with typed access to the selected executable CDSL exports.
    pub fn render_program(&self, selector: &[String]) -> Result<String, Error> {
        self.render_executable(selector, false, false)
    }

    pub(crate) fn render_catalog(&self, selector: &[String]) -> Result<String, Error> {
        self.render_executable(selector, true, false)
    }

    pub(crate) fn render_producers(&self, selector: &[String]) -> Result<String, Error> {
        self.render_executable(selector, false, true)
    }

    fn render_executable(
        &self,
        selector: &[String],
        catalog_only: bool,
        producers: bool,
    ) -> Result<String, Error> {
        let selected = self
            .artifact()
            .exported_module(selector)
            .ok_or_else(|| Error(format!("unknown module {}", path_name(selector))))?;
        let mut schema = Schema::from_engine(EGraph::default());
        let mode = if catalog_only {
            Selection::Catalog
        } else {
            Selection::Program
        };
        let dependencies = Dependencies::collect(self, selected, mode);
        schema.install(&dependencies, !catalog_only)?;
        if catalog_only {
            return rust::emit(self, selected, &schema, &BTreeMap::new(), None, &[], None);
        }
        let hosts = schema
            .hosts
            .iter()
            .filter_map(|path| {
                let Declaration::Axiom { ty, .. } = dependencies.declarations[path] else {
                    return None;
                };
                let (inputs, output) = arrow(ty)?;
                Some((path.as_slice(), parts(inputs), output))
            })
            .collect::<Vec<_>>();
        let lowered = schema.lower_dependencies(self, &dependencies)?;
        let (program, producers) = if producers {
            let (program, writers) = lowered
                .rules
                .compile_producers(schema.database())
                .map_err(error)?;
            (program, Some(writers))
        } else {
            (lowered.compile(schema.database())?, None)
        };
        rust::emit(
            self,
            selected,
            &schema,
            &lowered.schedules,
            Some(&program.to_bytes()),
            &hosts,
            producers.as_deref(),
        )
    }
}
