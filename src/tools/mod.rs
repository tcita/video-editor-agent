mod concat;
mod cut;
mod export;
mod probe;

use crate::tool::ToolRegistry;

/// Every atomic tool exposed to the model is registered here.
/// There is no shell, file-write, or argument-passthrough tool, by design.
pub fn standard_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(probe::ProbeTool),
        Box::new(cut::CutTool),
        Box::new(concat::ConcatTool),
        Box::new(export::ExportTool),
    ])
}
