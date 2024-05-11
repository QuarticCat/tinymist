//! tinymist compile mode

use std::ops::ControlFlow;
use std::{collections::HashMap, path::Path, sync::Arc};

use async_lsp::{LanguageServer, ResponseError};
use lsp_types::request::*;
use lsp_types::*;
use serde::Deserialize;
use serde_json::{from_value, to_value, Map, Value as JsonValue};
use tinymist_query::{ExportKind, PageSelection};
use tokio::sync::mpsc;
use typst::{diag::FileResult, syntax::Source, util::Deferred};
use typst_ts_compiler::vfs::notify::FileChangeSet;
use typst_ts_core::{config::compiler::DETACHED_ENTRY, ImmutPath};

use super::*;
use crate::actor::{editor::EditorRequest, export::ExportConfig, typ_client::CompileClientActor};
use crate::compiler_init::{CompileConfig, ConstCompileConfig};
use crate::state::MemoryFileMeta;
use crate::world::SharedFontResolver;

#[derive(Debug, Clone, Default, Deserialize)]
struct ExportOpts {
    page: PageSelection,
}

/// The object providing the language server functionality.
pub struct CompileState {
    /* Resources */
    /// The font resolver to use.
    pub font: Deferred<SharedFontResolver>,
    /// Source synchronized with client
    pub memory_changes: HashMap<Arc<Path>, MemoryFileMeta>,
    /// The diagnostics sender to send diagnostics to `crate::actor::cluster`.
    pub editor_tx: mpsc::UnboundedSender<EditorRequest>,
    /// The compiler actor.
    pub compiler: Option<CompileClientActor>,

    /* Configurations */
    /// User configuration from the editor.
    pub config: CompileConfig,
    /// Const configuration initialized at the start of the session.
    pub const_config: ConstCompileConfig,
    /// Extra commands provided with `textDocument/executeCommand`.
    pub exec_cmds: ExecCmdMap<CompileState>,
}

impl CompileState {
    pub fn new(
        editor_tx: mpsc::UnboundedSender<EditorRequest>,
        font: Deferred<SharedFontResolver>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            editor_tx,
            font,
            compiler: None,
            memory_changes: HashMap::new(),

            config: Default::default(),
            const_config: Default::default(),
            exec_cmds: HashMap::from_iter([
                ("tinymist.exportPdf", Self::export_pdf),
                ("tinymist.exportSvg", Self::export_svg),
                ("tinymist.exportPng", Self::export_png),
                ("tinymist.doClearCache", Self::clear_cache),
                ("tinymist.changeEntry", Self::change_entry),
            ]),
        }
    }

    pub fn compiler(&self) -> &CompileClientActor {
        self.compiler.as_ref().unwrap()
    }

    /* Extra Commands */

    /// Export the current document as a PDF file.
    pub fn export_pdf(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.export(ExportKind::Pdf, args)
    }

    /// Export the current document as a Svg file.
    pub fn export_svg(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(opts) = parse_arg_or_default::<ExportOpts>(&args, 1) else {
            return invalid_params("expect export opts at arg[1]");
        };
        self.export(ExportKind::Svg { page: opts.page }, args)
    }

    /// Export the current document as a Png file.
    pub fn export_png(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(opts) = parse_arg_or_default::<ExportOpts>(&args, 1) else {
            return invalid_params("expect export opts at arg[1]");
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
            return invalid_params("expect path at arg[0]");
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
            return invalid_params("expect path at arg[0]");
        };
        if let Err(err) = self.do_change_entry(entry.clone()) {
            return internal_error(format!("cannot focus file: {err}"));
        };
        log::info!("entry changed: {entry:?}");
        ok(JsonValue::Null)
    }
}

impl LanguageServer for CompileState {
    type Error = ResponseError;
    type NotifyResult = ControlFlow<async_lsp::Result<()>>;

    fn initialize(&mut self, params: InitializeParams) -> ResponseFuture<Initialize> {
        todo!()
    }

    fn execute_command(&mut self, params: ExecuteCommandParams) -> ResponseFuture<ExecuteCommand> {
        let Some(handler) = self.exec_cmds.get(&params.command) else {
            return method_not_found(format!("unknown command: {}", params.command));
        };
        handler(self, params.arguments)
    }
}
