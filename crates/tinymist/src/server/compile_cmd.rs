use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{to_value, Value as JsonValue};
use tinymist_query::{ExportKind, PageSelection};

use super::compile::*;
use super::*;

#[derive(Debug, Clone, Default, Deserialize)]
struct ExportOpts {
    page: PageSelection,
}

impl CompileState {
    #[rustfmt::skip]
    pub fn get_exec_cmds() -> ExecCmdMap<Self> {
        HashMap::from_iter([
            ("tinymist.exportPdf", Self::export_pdf as _),
            ("tinymist.exportSvg", Self::export_svg as _),
            ("tinymist.exportPng", Self::export_png as _),
            ("tinymist.doClearCache", Self::clear_cache as _),
            ("tinymist.changeEntry", Self::change_entry as _),
        ])
    }

    /// Export the current document as a PDF file.
    pub fn export_pdf(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.export(ExportKind::Pdf, args)
    }

    /// Export the current document as a Svg file.
    pub fn export_svg(&mut self, mut args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let opts = get_arg_or_default!(args[1] as ExportOpts);
        self.export(ExportKind::Svg { page: opts.page }, args)
    }

    /// Export the current document as a Png file.
    pub fn export_png(&mut self, mut args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let opts = get_arg_or_default!(args[1] as ExportOpts);
        self.export(ExportKind::Png { page: opts.page }, args)
    }

    /// Export the current document as some format. The client is responsible
    /// for passing the correct absolute path of typst document.
    pub fn export(
        &mut self,
        kind: ExportKind,
        mut args: Vec<JsonValue>,
    ) -> ResponseFuture<ExecuteCommand> {
        let path = get_arg!(args[0] as PathBuf);
        match self.compiler().on_export(kind, path) {
            Ok(res) => ok(to_value(res).unwrap()),
            Err(err) => internal_error("failed to export: {err}"),
        }
    }

    /// Clear all cached resources.
    pub fn clear_cache(&mut self, _args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        comemo::evict(0);
        self.compiler().clear_cache();
        ok(JsonValue::Null)
    }

    /// Focus main file to some path.
    pub fn change_entry(&mut self, mut args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let entry = get_arg!(args[0] as Option<PathBuf>);
        if let Err(err) = self.do_change_entry(entry.map(Into::into)) {
            return internal_error(format!("cannot change entry: {err}"));
        };
        ok(JsonValue::Null)
    }
}
