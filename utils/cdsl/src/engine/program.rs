use super::*;

mod dependencies;
mod rust;

pub(super) use dependencies::{Dependencies, Selection};

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
        schema.install(&dependencies, !catalog_only)?;
        schema.bind_operation_imports(dependencies.operation_imports(self))?;
        if catalog_only {
            return rust::emit(self, selected, &schema, &[], &schema.to_bytes()?, None);
        }
        schema.install_local_tables(&dependencies)?;
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
        let mut lowered = schema.lower_dependencies(self, &dependencies, true)?;
        let templates = schema.table_templates(&dependencies, selected);
        let frames = lowered
            .scoped
            .iter()
            .map(|(path, frame)| {
                Ok((
                    path.clone(),
                    frame.compile(schema.database(), &lowered.rules, &templates)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, Error>>()?;
        let local = frames
            .values()
            .flat_map(|frame| frame.locals.iter().cloned())
            .collect::<BTreeSet<_>>();
        let hidden: BTreeSet<tir_egglog::TableId> = local
            .iter()
            .map(|name| schema.database().table(name).map_err(error))
            .collect::<Result<_, _>>()?;
        if !hidden.is_empty() {
            lowered
                .schedules
                .retain(|_, schedule| !lowered.rules.touches_tables(schedule, &hidden));
        }
        let catalog = Rules::compile_schema_without(schema.database(), &hidden)
            .map_err(error)?
            .to_bytes();
        let program = lowered.compile(schema.database())?;
        let binary = program.to_bytes();
        rust::emit(
            self,
            selected,
            &schema,
            &hosts,
            &catalog,
            Some(rust::Executable {
                lowered: &lowered,
                binary: &binary,
                frames: &frames,
                local_tables: &local,
            }),
        )
    }
}
