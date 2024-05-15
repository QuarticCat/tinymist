//! tinymist compile mode

use std::ops::ControlFlow;
use std::{collections::HashMap, path::Path, sync::Arc};

use async_lsp::{LanguageServer, ResponseError};
use lsp_types::request::*;
use lsp_types::*;
use tokio::sync::mpsc;
use typst::util::Deferred;

use super::*;
use crate::actor::{editor::EditorRequest, typ_client::CompileClientActor};
use crate::compile_init::{CompileConfig, ConstCompileConfig};
use crate::state::MemoryFileMeta;
use crate::world::SharedFontResolver;

/// The object providing the language server functionality.
pub struct CompileState {
    /* Configurations */
    /// User configuration from the editor.
    pub config: CompileConfig,
    /// Const configuration initialized at the start of the session.
    pub const_config: ConstCompileConfig,
    /// Extra commands provided with `textDocument/executeCommand`.
    pub exec_cmds: ExecCmdMap<Self>,

    /* Resources */
    /// The font resolver to use.
    pub font: Deferred<SharedFontResolver>,
    /// Source synchronized with client
    pub memory_changes: HashMap<Arc<Path>, MemoryFileMeta>,
    /// The diagnostics sender to send diagnostics to `crate::actor::cluster`.
    pub editor_tx: mpsc::UnboundedSender<EditorRequest>,
    /// The compiler actor.
    pub compiler: Option<CompileClientActor>,
}

impl CompileState {
    pub fn new(
        editor_tx: mpsc::UnboundedSender<EditorRequest>,
        font: Deferred<SharedFontResolver>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            config: Default::default(),
            const_config: Default::default(),
            exec_cmds: Self::get_exec_cmds(),

            editor_tx,
            font,
            compiler: None,
            memory_changes: HashMap::new(),
        }
    }

    pub fn compiler(&self) -> &CompileClientActor {
        self.compiler.as_ref().unwrap()
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
