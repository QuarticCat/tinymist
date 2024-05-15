use serde::Deserialize;
use serde_json::{to_value, Value as JsonValue};
use tinymist_query::{ExportKind, PageSelection};
use typst_ts_core::ImmutPath;

use super::compile::*;
use super::*;

#[derive(Debug, Clone, Default, Deserialize)]
struct ExportOpts {
    page: PageSelection,
}

impl CompileState {
    pub fn get_exec_cmds() -> ExecCmdMap<Self> {
        HashMap::from_iter([
            ("tinymist.exportPdf", Self::export_pdf),
            ("tinymist.exportSvg", Self::export_svg),
            ("tinymist.exportPng", Self::export_png),
            ("tinymist.doClearCache", Self::clear_cache),
            ("tinymist.changeEntry", Self::change_entry),
        ])
    }

    /// Export the current document as a PDF file.
    pub fn export_pdf(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.export(ExportKind::Pdf, args)
    }

    /// Export the current document as a Svg file.
    pub fn export_svg(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(opts) = parse_arg_or_default::<ExportOpts>(&args, 1) else {
            return invalid_params("expect export opts at args[1]");
        };
        self.export(ExportKind::Svg { page: opts.page }, args)
    }

    /// Export the current document as a Png file.
    pub fn export_png(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(opts) = parse_arg_or_default::<ExportOpts>(&args, 1) else {
            return invalid_params("expect export opts at args[1]");
        };
        self.export(ExportKind::Png { page: opts.page }, args)
    }

    /// Export the current document as some format. The client is responsible
    /// for passing the correct absolute path of typst document.
    pub fn export(
        &mut self,
        kind: ExportKind,
        args: Vec<JsonValue>,
    ) -> ResponseFuture<ExecuteCommand> {
        let Some(path) = parse_arg::<ImmutPath>(&args, 0) else {
            return invalid_params("expect path at args[0]");
        };
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
    pub fn change_entry(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(entry) = parse_arg::<Option<ImmutPath>>(&args, 0) else {
            return invalid_params("expect path at args[0]");
        };
        if let Err(err) = self.do_change_entry(entry.clone()) {
            return internal_error(format!("cannot focus file: {err}"));
        };
        log::info!("entry changed: {entry:?}");
        ok(JsonValue::Null)
    }
}
