//! tinymist LSP mode

use std::ops::ControlFlow;

use async_lsp::{LanguageServer, ResponseError};
use lsp_types::request::*;
use lsp_types::*;
use tinymist_query::{self as q, url_to_path, SemanticTokenContext};
use typst_ts_compiler::service::Compiler;
use typst_ts_core::{error::prelude::*, ImmutPath};

use super::lsp_init::*;
use super::*;
use crate::actor::format::FormatRequest;
use crate::actor::user_action::UserActionRequest;
use crate::compiler::CompileState;
use crate::world::CompileFontOpts;

/// The object providing the language server functionality.
pub struct LanguageState {
    /* States to synchronize with the client */
    /// Whether the server has registered semantic tokens capabilities.
    pub sema_tokens_registered: bool,
    /// Whether the server has registered document formatter capabilities.
    pub formatter_registered: bool,
    /// Whether client is pinning a file.
    pub pinning: bool,
    /// The client focusing file.
    pub focusing: Option<ImmutPath>,
    /// The client ever focused implicitly by activities.
    pub ever_focusing_by_activities: bool,
    /// The client ever sent manual focusing request.
    pub ever_manual_focusing: bool,

    /* Configurations */
    /// User configuration from the editor.
    pub config: LanguageConfig,
    /// Const configuration initialized at the start of the session.
    pub const_config: ConstLanguageConfig,
    /// Font configuration from CLI args.
    pub font_opts: CompileFontOpts,

    /* Resources */
    /// The semantic token context.
    pub tokens_ctx: SemanticTokenContext,
    /// The compiler for general purpose.
    pub primary: CompileState,
    /// The compilers for tasks
    pub dedicates: Vec<CompileState>,
    /// The formatter thread running in backend.
    /// Note: The thread will exit if you drop the sender.
    pub format_thread: Option<crossbeam_channel::Sender<FormatRequest>>,
    /// The user action thread running in backend.
    /// Note: The thread will exit if you drop the sender.
    pub user_action_thread: Option<crossbeam_channel::Sender<UserActionRequest>>,
}

impl LanguageState {
    pub fn new(font_opts: CompileFontOpts) -> Self {
        Self {
            sema_tokens_registered: false,
            formatter_registered: false,
            ever_focusing_by_activities: false,
            ever_manual_focusing: false,
            pinning: false,
            focusing: None,

            config: Default::default(),
            const_config: Default::default(),
            font_opts,

            tokens_ctx: Default::default(),
            primary: todo!(),
            dedicates: Vec::new(),
            format_thread: None,
            user_action_thread: None,
        }
    }
}

// todo: parallelization
// todo: create a trait for these requests and make it a function
macro_rules! query_source {
    ($self:ident, $req:ident) => {{
        let Some(mem_file) = $self.primary.memory_changes.get(&$req.path) else {
            return internal_error(format!("file missing: {:?}", $req.path));
        };
        let source = mem_file.content.clone();
        // todo: pass source by value to avoid one extra clone
        ok($req.request(&source, $self.const_config.position_encoding))
    }};
}

// todo: parallelization (snapshot self.tokens_ctx)
// todo: create a trait for these requests and make it a function
macro_rules! query_tokens_cache {
    ($self:ident, $req:ident) => {{
        let Some(mem_file) = $self.primary.memory_changes.get(&$req.path) else {
            return internal_error(format!("file missing: {:?}", $req.path));
        };
        let source = mem_file.content.clone();
        ok($req.request(&$self.tokens_ctx, source))
    }};
}

// todo: create a trait for these requests and make it a function
macro_rules! query_state {
    ($self:ident, $req:ident) => {{
        let fut = $self
            .primary
            .compiler()
            .steal_state(move |w, doc| $req.request(w, doc));
        Box::pin(async move { fut.await.or_else(internal_error) })
    }};
}

// todo: create a trait for these requests and make it a function
macro_rules! query_world {
    ($self:ident, $req:ident) => {{
        let fut = $self
            .primary
            .compiler()
            .steal_world(move |w| $req.request(w));
        Box::pin(async move { fut.await.or_else(internal_error) })
    }};
}

// todo: complete logging or implement a logging layer

impl LanguageServer for LanguageState {
    type Error = ResponseError;
    type NotifyResult = ControlFlow<async_lsp::Result<()>>;

    /* Lifecycle */

    fn initialize(&mut self, params: InitializeParams) -> ResponseFuture<Initialize> {
        let res = self.init(params);
        ok(res)
    }

    fn initialized(&mut self, params: InitializedParams) -> Self::NotifyResult {
        self.inited(params);
        ControlFlow::Continue(())
    }

    /* Notifications */

    fn did_open(&mut self, params: DidOpenTextDocumentParams) -> Self::NotifyResult {
        log::info!("did open {:?}", params.text_document.uri);
        let path = url_to_path(params.text_document.uri);
        let text = params.text_document.text;
        self.create_source(path, text).unwrap();

        // Focus after opening
        self.implicit_focus_entry(|| Some(path.as_path().into()), 'o');
        ControlFlow::Continue(())
    }

    fn did_close(&mut self, params: DidCloseTextDocumentParams) -> Self::NotifyResult {
        log::info!("did close {:?}", params.text_document.uri);
        let path = url_to_path(params.text_document.uri);
        self.remove_source(path).unwrap();
        ControlFlow::Continue(())
    }

    fn did_change(&mut self, params: DidChangeTextDocumentParams) -> Self::NotifyResult {
        log::info!("did change {:?}", params.text_document.uri);
        let path = url_to_path(params.text_document.uri);
        let changes = params.content_changes;
        let position_encoding = self.const_config.position_encoding;
        self.edit_source(path, changes, position_encoding).unwrap();
        ControlFlow::Continue(())
    }

    fn did_save(&mut self, params: DidSaveTextDocumentParams) -> Self::NotifyResult {
        log::info!("did save {:?}", params.text_document.uri);
        let req = q::OnSaveExportRequest {
            path: url_to_path(params.text_document.uri),
        };
        todo!();
        ControlFlow::Continue(())
    }

    fn did_change_configuration(
        &mut self,
        params: DidChangeConfigurationParams,
    ) -> Self::NotifyResult {
        todo!();
        ControlFlow::Continue(())
    }

    /* Latency Sensitive Requests */

    fn completion(&mut self, params: CompletionParams) -> ResponseFuture<Completion> {
        let invoked = CompletionTriggerKind::INVOKED;
        let req = q::CompletionRequest {
            path: url_to_path(params.text_document_position.text_document.uri),
            position: params.text_document_position.position,
            explicit: params.context.is_some_and(|c| c.trigger_kind == invoked),
        };
        query_state!(self, req)
    }

    fn semantic_tokens_full(
        &mut self,
        params: SemanticTokensParams,
    ) -> ResponseFuture<SemanticTokensFullRequest> {
        let req = q::SemanticTokensFullRequest {
            path: url_to_path(params.text_document),
        };
        self.implicit_focus_entry(|| Some(req.path.as_path().into()), 't');
        query_tokens_cache!(self, req)
    }

    fn semantic_tokens_full_delta(
        &mut self,
        params: SemanticTokensDeltaParams,
    ) -> ResponseFuture<SemanticTokensFullDeltaRequest> {
        let req = q::SemanticTokensDeltaRequest {
            path: url_to_path(params.text_document),
            previous_result_id: params.previous_result_id,
        };
        self.implicit_focus_entry(|| Some(req.path.as_path().into()), 't');
        query_tokens_cache!(self, req)
    }

    fn document_symbol(
        &mut self,
        params: DocumentSymbolParams,
    ) -> ResponseFuture<DocumentSymbolRequest> {
        let req = q::DocumentSymbolRequest {
            path: url_to_path(params.text_document.uri),
        };
        query_source!(self, req)
    }

    fn selection_range(
        &mut self,
        params: SelectionRangeParams,
    ) -> ResponseFuture<SelectionRangeRequest> {
        let req = q::SelectionRangeRequest {
            path: url_to_path(params.text_document.uri),
            positions: params.positions,
        };
        query_source!(self, req)
    }

    fn formatting(&mut self, params: DocumentFormattingParams) -> ResponseFuture<Formatting> {
        // todo: remove format thread
        if self.config.formatter == FormatterMode::Disable {
            return ok(None);
        }
        todo!()
    }

    /* Latency Insensitive Requests */

    fn inlay_hint(&mut self, params: InlayHintParams) -> ResponseFuture<InlayHintRequest> {
        let req = q::InlayHintRequest {
            path: url_to_path(params.text_document.uri),
            range: params.range,
        };
        query_world!(self, req)
    }

    fn document_color(&mut self, params: DocumentColorParams) -> ResponseFuture<DocumentColor> {
        let req = q::DocumentColorRequest {
            path: url_to_path(params.text_document.uri),
        };
        query_world!(self, req)
    }

    fn color_presentation(
        &mut self,
        params: ColorPresentationParams,
    ) -> ResponseFuture<ColorPresentationRequest> {
        let req = q::ColorPresentationRequest {
            path: url_to_path(params.text_document.uri),
            color: params.color,
            range: params.range,
        };
        ok(req.request())
    }

    fn code_action(&mut self, params: CodeActionParams) -> ResponseFuture<CodeActionRequest> {
        let req = q::CodeActionRequest {
            path: url_to_path(params.text_document),
            range: params.range,
        };
        query_world!(self, req)
    }

    fn hover(&mut self, params: HoverParams) -> ResponseFuture<HoverRequest> {
        let req = q::HoverRequest {
            path: url_to_path(params.text_document_position_params.text_document.uri),
            position: params.text_document_position_params.position,
        };
        self.implicit_focus_entry(|| Some(req.path.as_path().into()), 'h');
        query_state!(self, req)
    }

    fn code_lens(&mut self, params: CodeLensParams) -> ResponseFuture<CodeLensRequest> {
        let req = q::CodeLensRequest {
            path: url_to_path(params.text_document.uri),
        };
        query_world!(self, req)
    }

    fn folding_range(&mut self, params: FoldingRangeParams) -> ResponseFuture<FoldingRangeRequest> {
        let req = q::FoldingRangeRequest {
            path: url_to_path(params.text_document.uri),
            line_folding_only: self.const_config.doc_line_folding_only,
        };
        self.implicit_focus_entry(|| Some(req.path.as_path().into()), 'f');
        query_source!(self, req)
    }

    fn signature_help(
        &mut self,
        params: SignatureHelpParams,
    ) -> ResponseFuture<SignatureHelpRequest> {
        let req = q::SignatureHelpRequest {
            path: url_to_path(params.text_document_position_params.text_document.uri),
            position: params.text_document_position_params.position,
        };
        query_world!(self, req)
    }

    fn prepare_rename(
        &mut self,
        params: TextDocumentPositionParams,
    ) -> ResponseFuture<PrepareRenameRequest> {
        let req = q::PrepareRenameRequest {
            path: url_to_path(params.text_document.uri),
            position: params.position,
        };
        query_state!(self, req)
    }

    fn rename(&mut self, params: RenameParams) -> ResponseFuture<Rename> {
        let req = q::RenameRequest {
            path: url_to_path(params.text_document_position.text_document.uri),
            position: params.text_document_position.position,
            new_name: params.new_name,
        };
        query_state!(self, req)
    }

    fn definition(&mut self, params: GotoDefinitionParams) -> ResponseFuture<GotoDefinition> {
        let req = q::GotoDefinitionRequest {
            path: url_to_path(params.text_document_position_params.text_document.uri),
            position: params.text_document_position_params.position,
        };
        query_state!(self, req)
    }

    fn declaration(&mut self, params: GotoDeclarationParams) -> ResponseFuture<GotoDeclaration> {
        let req = q::GotoDeclarationRequest {
            path: url_to_path(params.text_document_position_params.text_document.uri),
            position: params.text_document_position_params.position,
        };
        query_world!(self, req)
    }

    fn references(&mut self, params: ReferenceParams) -> ResponseFuture<References> {
        let req = q::ReferencesRequest {
            path: url_to_path(params.text_document_position.text_document.uri),
            position: params.text_document_position.position,
        };
        query_world!(self, req)
    }

    fn symbol(&mut self, params: WorkspaceSymbolParams) -> ResponseFuture<WorkspaceSymbolRequest> {
        let req = q::SymbolRequest {
            pattern: (!params.query.is_empty()).then_some(params.query),
        };
        query_world!(self, req)
    }

    fn execute_command(&mut self, params: ExecuteCommandParams) -> ResponseFuture<ExecuteCommand> {
        todo!()
    }
}
