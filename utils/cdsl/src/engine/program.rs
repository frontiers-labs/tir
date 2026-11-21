use super::*;

mod dependencies;
mod rust;

pub(super) use dependencies::{Dependencies, Selection};
pub(super) use rust::relation_columns;

impl CheckedProgram {
    /// Emit a Rust module with typed access to the selected executable CDSL exports.
    pub fn render_program(&self, selector: &[String]) -> Result<String, Error> {
        self.render_executable(selector, false)
    }

    pub(crate) fn render_catalog(&self, selector: &[String]) -> Result<String, Error> {
        self.render_executable(selector, true)
    }

    fn render_executable(&self, selector: &[String], catalog_only: bool) -> Result<String, Error> {
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
        schema.install(self, &dependencies)?;
        if catalog_only {
            return rust::emit(self, selected, &schema, &BTreeMap::new(), None, &[]);
        }
        let hosts = dependencies
            .needed
            .iter()
            .filter_map(|path| {
                let Declaration::Axiom {
                    ty,
                    engine: Some(EngineBinding::Host(_)),
                    ..
                } = dependencies.declarations[path]
                else {
                    return None;
                };
                let (inputs, output) = arrow(ty)?;
                Some((path.as_slice(), parts(inputs), output))
            })
            .collect::<Vec<_>>();
        let lowered = schema.lower_dependencies(self, &dependencies)?;
        let program = lowered.compile(schema.database())?;
        rust::emit(
            self,
            selected,
            &schema,
            &lowered.schedules,
            Some(&program.to_bytes()),
            &hosts,
        )
    }
}
