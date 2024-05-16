//! Bootstrap actors for Tinymist.

pub mod editor;
pub mod export;
pub mod typ_client;
pub mod typ_server;

use std::path::Path;

use tinymist_query::analysis::Analysis;
use tinymist_query::ExportKind;
use tinymist_render::PeriscopeRenderer;
use tokio::sync::{mpsc, watch};
use typst::util::Deferred;
use typst_ts_compiler::{
    service::CompileDriverImpl,
    vfs::notify::{FileChangeSet, MemoryEvent},
};
use typst_ts_core::config::compiler::EntryState;

use self::{
    export::{ExportActor, ExportConfig},
    typ_client::{CompileClientActor, CompileDriver, CompileHandler},
    typ_server::CompileServerActor,
};
use crate::{
    compile::CompileState,
    world::{ImmutDict, LspWorld, LspWorldBuilder},
};

type CompileDriverInner = CompileDriverImpl<LspWorld>;

impl CompileState {
    pub fn server(
        &self,
        editor_group: String,
        entry: EntryState,
        inputs: ImmutDict,
        snapshot: FileChangeSet,
    ) -> CompileClientActor {
        let (doc_tx, doc_rx) = watch::channel(None);
        let (export_tx, export_rx) = mpsc::unbounded_channel();

        // Run Export actors before preparing cluster to avoid loss of events
        self.handle.spawn(
            ExportActor::new(
                editor_group.clone(),
                doc_rx,
                self.editor_tx.clone(),
                export_rx,
                ExportConfig {
                    substitute_pattern: self.config.output_path.clone(),
                    entry: entry.clone(),
                    mode: self.config.export_pdf,
                },
                ExportKind::Pdf,
                self.config.notify_compile_status,
            )
            .run(),
        );

        // Create the server
        let inner = Deferred::new({
            let current_runtime = self.handle.clone();
            let handler = CompileHandler {
                #[cfg(feature = "preview")]
                inner: std::sync::Arc::new(parking_lot::Mutex::new(None)),
                diag_group: editor_group.clone(),
                doc_tx,
                export_tx: export_tx.clone(),
                editor_tx: self.editor_tx.clone(),
            };

            let position_encoding = self.const_config().position_encoding;
            let enable_periscope = self.config.periscope_args.is_some();
            let periscope_args = self.config.periscope_args.clone();
            let diag_group = editor_group.clone();
            let entry = entry.clone();
            let font_resolver = self.font.clone();
            move || {
                log::info!("TypstActor: creating server for {diag_group}, entry: {entry:?}, inputs: {inputs:?}");

                // Create the world
                let font_resolver = font_resolver.wait().clone();
                let world = LspWorldBuilder::build(entry.clone(), font_resolver, inputs)
                    .expect("incorrect options");

                // Create the compiler
                let driver = CompileDriverInner::new(world);
                let driver = CompileDriver {
                    inner: driver,
                    handler,
                    analysis: Analysis {
                        position_encoding,
                        root: Path::new("").into(),
                        enable_periscope,
                        caches: Default::default(),
                    },
                    periscope: PeriscopeRenderer::new(periscope_args.unwrap_or_default()),
                };

                // Create the actor
                let server = CompileServerActor::new(driver, entry).with_watch(true);
                let client = server.client();

                // We do send memory changes instead of initializing compiler with them.
                // This is because there are state recorded inside of the compiler actor, and we
                // must update them.
                client.add_memory_changes(MemoryEvent::Update(snapshot));

                current_runtime.spawn(server.spawn());

                client
            }
        });

        CompileClientActor::new(editor_group, self.config.clone(), entry, inner, export_tx)
    }
}
