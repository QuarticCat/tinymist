//! The typst actors running compilations.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as SyncMutex},
};

use anyhow::anyhow;
use futures::future::join_all;
use log::{debug, error, info, trace, warn};
use lsp_types::{Diagnostic, TextDocumentContentChangeEvent, Url};
use parking_lot::{Mutex, RwLock};
use tinymist_query::{
    lsp_to_typst, CompilerQueryRequest, CompilerQueryResponse, DiagnosticsMap, FoldRequestFeature,
    LspDiagnostic, OnSaveExportRequest, PositionEncoding, SemanticTokenCache,
};
use tokio::sync::{broadcast, mpsc, watch};
use typst::{
    diag::{FileResult, SourceDiagnostic, SourceResult},
    layout::Position,
    syntax::{Source, Span, VirtualPath},
    util::Deferred,
};
use typst_preview::{
    CompilationHandle, CompilationHandleImpl, CompileHost, CompileStatus, DocToSrcJumpInfo,
    EditorServer, Location, MemoryFiles, MemoryFilesShort, SourceFileServer,
};
use typst_ts_compiler::{
    service::{
        CompileActor, CompileClient as TsCompileClient, CompileDriver as CompileDriverInner,
        CompileExporter, CompileMiddleware, Compiler, WorkspaceProvider, WorldExporter,
    },
    vfs::notify::{FileChangeSet, MemoryEvent},
    Time, TypstSystemWorld,
};
use typst_ts_core::{
    config::CompileOpts, debug_loc::SourceSpanOffset, error::prelude::*, typst::prelude::EcoVec,
    Bytes, Error, ImmutPath, TypstDocument, TypstWorld,
};

use crate::actor::render::RenderActorRequest;
use crate::ConstConfig;
use crate::LspHost;

use super::ActorFactory;

type CompileService<H> = CompileActor<Reporter<CompileExporter<CompileDriver>, H>>;
type CompileClient<H> = TsCompileClient<CompileService<H>>;
type Node = CompileNode<CompileHandler>;

type DiagnosticsSender = mpsc::UnboundedSender<(String, Option<DiagnosticsMap>)>;

pub struct CompileCluster {
    roots: Vec<PathBuf>,
    actor_factory: ActorFactory,
    position_encoding: PositionEncoding,
    memory_changes: RwLock<HashMap<Arc<Path>, MemoryFileMeta>>,
    primary: Deferred<Node>,
    main: Arc<Mutex<Option<Deferred<Node>>>>,
    pub tokens_cache: SemanticTokenCache,
    actor: Option<CompileClusterActor>,
}

impl CompileCluster {
    pub fn new(
        actor_factory: ActorFactory,
        host: LspHost,
        roots: Vec<PathBuf>,
        cfg: &ConstConfig,
        primary: Deferred<Node>,
        diag_rx: mpsc::UnboundedReceiver<(String, Option<DiagnosticsMap>)>,
    ) -> Self {
        Self {
            roots,
            actor_factory,
            position_encoding: cfg.position_encoding,
            memory_changes: RwLock::new(HashMap::new()),
            primary,
            main: Arc::new(Mutex::new(None)),
            tokens_cache: Default::default(),
            actor: Some(CompileClusterActor {
                host,
                diag_rx,
                diagnostics: HashMap::new(),
                affect_map: HashMap::new(),
                published_primary: false,
            }),
        }
    }

    pub fn split(mut self) -> (Self, CompileClusterActor) {
        let actor = self.actor.take().expect("actor is poisoned");
        (self, actor)
    }

    pub fn activate_doc(&self, new_entry: Option<ImmutPath>) -> Result<(), Error> {
        match new_entry {
            Some(new_entry) => self.primary.wait().change_entry(new_entry)?,
            None => {
                self.primary.wait().disable();
            }
        }

        Ok(())
    }

    pub fn pin_main(&self, new_entry: Option<Url>) -> Result<(), Error> {
        let mut m = self.main.lock();
        match (new_entry, m.is_some()) {
            (Some(new_entry), true) => {
                let path = new_entry
                    .to_file_path()
                    .map_err(|_| error_once!("invalid url"))?;
                let path = path.as_path().into();

                m.as_mut().unwrap().wait().change_entry(path)
            }
            (Some(new_entry), false) => {
                let path = new_entry
                    .to_file_path()
                    .map_err(|_| error_once!("invalid url"))?;
                let path = path.as_path().into();

                let main_node =
                    self.actor_factory
                        .server("main".to_owned(), self.roots.clone(), Some(path));

                *m = Some(main_node);
                Ok(())
            }
            (None, true) => {
                // todo: unpin main
                m.as_mut().unwrap().wait().disable();

                Ok(())
            }
            (None, false) => Ok(()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn create_server(
    diag_group: String,
    cfg: &ConstConfig,
    roots: Vec<PathBuf>,
    opts: CompileOpts,
    entry: Option<PathBuf>,
    diag_tx: DiagnosticsSender,
    doc_sender: watch::Sender<Option<Arc<TypstDocument>>>,
    render_tx: broadcast::Sender<RenderActorRequest>,
) -> Deferred<Node> {
    let cfg = cfg.clone();
    let current_runtime = tokio::runtime::Handle::current();
    Deferred::new(move || {
        let compiler_driver = CompileDriver::new(roots.clone(), opts, entry.clone());
        let root = compiler_driver.inner.world.root.as_ref().to_owned();
        let handler: CompileHandler = compiler_driver.handler.clone();

        let driver = CompileExporter::new(compiler_driver).with_exporter(Box::new(
            move |_w: &dyn TypstWorld, doc| {
                let _ = doc_sender.send(Some(doc));
                // todo: is it right that ignore zero broadcast receiver?
                let _ = render_tx.send(RenderActorRequest::Render);

                Ok(())
            },
        ));
        let driver = Reporter {
            diag_group: diag_group.clone(),
            position_encoding: cfg.position_encoding,
            diag_tx,
            inner: driver,
            cb: handler.clone(),
        };
        let driver = CompileActor::new(driver, root).with_watch(true);

        let (server, client) = driver.split();

        current_runtime.spawn(server.spawn());

        let this = CompileNode::new(diag_group, cfg.position_encoding, handler, client);

        // todo: less bug-prone code
        if let Some(entry) = entry {
            this.entry.lock().unwrap().replace(entry.into());
        }

        this
    })
}

pub struct CompileClusterActor {
    host: LspHost,
    diag_rx: mpsc::UnboundedReceiver<(String, Option<DiagnosticsMap>)>,

    diagnostics: HashMap<Url, HashMap<String, Vec<LspDiagnostic>>>,
    affect_map: HashMap<String, Vec<Url>>,
    published_primary: bool,
}

impl CompileClusterActor {
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                e = self.diag_rx.recv() => {
                    let Some((group, diagnostics)) = e else {
                        break;
                    };
                    info!("received diagnostics from {}: diag({:?})", group, diagnostics.as_ref().map(|e| e.len()));
                    let with_primary = (self.affect_map.len() <= 1 && self.affect_map.contains_key("primary")) && group == "primary";
                    self.publish(group, diagnostics, with_primary).await;
                    if !with_primary {
                        let again_with_primary = self.affect_map.len() == 1 && self.affect_map.contains_key("primary");
                        if self.published_primary != again_with_primary {
                            self.flush_primary_diagnostics(again_with_primary).await;
                            self.published_primary = again_with_primary;
                        }
                    }
                }
            }
            info!("compile cluster actor is stopped");
        }
    }

    pub async fn do_publish_diagnostics(
        host: &LspHost,
        uri: Url,
        diags: Vec<Diagnostic>,
        version: Option<i32>,
        ignored: bool,
    ) {
        if ignored {
            return;
        }

        host.publish_diagnostics(uri, diags, version)
    }

    async fn flush_primary_diagnostics(&mut self, enable: bool) {
        let affected = self.affect_map.get("primary");

        let tasks = affected.into_iter().flatten().map(|url| {
            let path_diags = self.diagnostics.get(url);

            let diags = path_diags.into_iter().flatten().filter_map(|(g, diags)| {
                if g == "primary" {
                    return enable.then_some(diags);
                }
                Some(diags)
            });
            // todo: .flatten() removed
            // let to_publish = diags.flatten().cloned().collect();
            let to_publish = diags.flatten().cloned().collect();

            Self::do_publish_diagnostics(&self.host, url.clone(), to_publish, None, false)
        });

        join_all(tasks).await;
    }

    pub async fn publish(
        &mut self,
        group: String,
        next_diagnostics: Option<DiagnosticsMap>,
        with_primary: bool,
    ) {
        let is_primary = group == "primary";

        let affected = self.affect_map.get_mut(&group);

        let affected = affected.map(std::mem::take);

        // Gets sources which had some diagnostic published last time, but not this
        // time. The LSP specifies that files will not have diagnostics
        // updated, including removed, without an explicit update, so we need
        // to send an empty `Vec` of diagnostics to these sources.
        // todo: merge
        let clear_list = if let Some(n) = next_diagnostics.as_ref() {
            affected
                .into_iter()
                .flatten()
                .filter(|e| !n.contains_key(e))
                .map(|e| (e, None))
                .collect::<Vec<_>>()
        } else {
            affected
                .into_iter()
                .flatten()
                .map(|e| (e, None))
                .collect::<Vec<_>>()
        };
        let next_affected = if let Some(n) = next_diagnostics.as_ref() {
            n.keys().cloned().collect()
        } else {
            Vec::new()
        };
        let clear_all = next_diagnostics.is_none();
        // Gets touched updates
        let update_list = next_diagnostics
            .into_iter()
            .flatten()
            .map(|(x, y)| (x, Some(y)));

        let tasks = clear_list.into_iter().chain(update_list);
        let tasks = tasks.map(|(url, next)| {
            let path_diags = self.diagnostics.entry(url.clone()).or_default();
            let rest_all = path_diags
                .iter()
                .filter_map(|(g, diags)| {
                    if !with_primary && g == "primary" {
                        return None;
                    }
                    if g != &group {
                        Some(diags)
                    } else {
                        None
                    }
                })
                .flatten()
                .cloned();

            let next_all = next.clone().into_iter().flatten();
            let to_publish = rest_all.chain(next_all).collect();

            match next {
                Some(next) => {
                    path_diags.insert(group.clone(), next);
                }
                None => {
                    path_diags.remove(&group);
                }
            }

            Self::do_publish_diagnostics(
                &self.host,
                url,
                to_publish,
                None,
                is_primary && !with_primary,
            )
        });

        join_all(tasks).await;

        if clear_all {
            // We just used the cache, and won't need it again, so we can update it now
            self.affect_map.remove(&group);
        } else {
            // We just used the cache, and won't need it again, so we can update it now
            self.affect_map.insert(group, next_affected);
        }
    }
}

#[derive(Debug, Clone)]
struct MemoryFileMeta {
    mt: Time,
    content: Source,
}

impl CompileCluster {
    fn update_source(&self, files: FileChangeSet) -> Result<(), Error> {
        let primary = self.primary.clone();
        let main = self.main.clone();
        let primary = Some(&primary);
        let main = main.lock();
        let main = main.as_ref();
        let clients_to_notify = (primary.iter()).chain(main.iter());

        for client in clients_to_notify {
            let iw = client.wait().inner.lock();
            iw.add_memory_changes(MemoryEvent::Update(files.clone()));
        }

        Ok(())
    }

    pub fn create_source(&self, path: PathBuf, content: String) -> Result<(), Error> {
        let now = Time::now();
        let path: ImmutPath = path.into();

        self.memory_changes.write().insert(
            path.clone(),
            MemoryFileMeta {
                mt: now,
                content: Source::detached(content.clone()),
            },
        );

        let content: Bytes = content.as_bytes().into();
        log::info!("create source: {:?}", path);

        // todo: is it safe to believe that the path is normalized?
        let files = FileChangeSet::new_inserts(vec![(path, FileResult::Ok((now, content)).into())]);

        self.update_source(files)
    }

    pub fn remove_source(&self, path: PathBuf) -> Result<(), Error> {
        let path: ImmutPath = path.into();

        self.memory_changes.write().remove(&path);
        log::info!("remove source: {:?}", path);

        // todo: is it safe to believe that the path is normalized?
        let files = FileChangeSet::new_removes(vec![path]);

        self.update_source(files)
    }

    pub fn edit_source(
        &self,
        path: PathBuf,
        content: Vec<TextDocumentContentChangeEvent>,
        position_encoding: PositionEncoding,
    ) -> Result<(), Error> {
        let now = Time::now();
        let path: ImmutPath = path.into();

        let mut memory_changes = self.memory_changes.write();

        let meta = memory_changes
            .get_mut(&path)
            .ok_or_else(|| error_once!("file missing", path: path.display()))?;

        for change in content {
            let replacement = change.text;
            match change.range {
                Some(lsp_range) => {
                    let range = lsp_to_typst::range(lsp_range, position_encoding, &meta.content)
                        .expect("invalid range");
                    meta.content.edit(range, &replacement);
                }
                None => {
                    meta.content.replace(&replacement);
                }
            }
        }

        meta.mt = now;

        let snapshot = FileResult::Ok((now, meta.content.text().as_bytes().into())).into();

        drop(memory_changes);

        let files = FileChangeSet::new_inserts(vec![(path.clone(), snapshot)]);

        self.update_source(files)
    }
}

macro_rules! query_state {
    ($self:ident, $method:ident, $req:expr) => {{
        let doc = $self.handler.result.lock().unwrap().clone().ok();
        let enc = $self.position_encoding;
        let res = $self.steal_world(move |w| $req.request(w, doc, enc));
        res.map(CompilerQueryResponse::$method)
    }};
}

macro_rules! query_world {
    ($self:ident, $method:ident, $req:expr) => {{
        let enc = $self.position_encoding;
        let res = $self.steal_world(move |w| $req.request(w, enc));
        res.map(CompilerQueryResponse::$method)
    }};
}

macro_rules! query_source {
    ($self:ident, $method:ident, $req:expr) => {{
        let path: ImmutPath = $req.path.clone().into();
        let vfs = $self.memory_changes.read();
        let snapshot = vfs
            .get(&path)
            .ok_or_else(|| anyhow!("file missing {:?}", $self.memory_changes))?;
        let source = snapshot.content.clone();

        let enc = $self.position_encoding;
        let res = $req.request(source, enc);
        Ok(CompilerQueryResponse::$method(res))
    }};
}

macro_rules! query_tokens_cache {
    ($self:ident, $method:ident, $req:expr) => {{
        let path: ImmutPath = $req.path.clone().into();
        let vfs = $self.memory_changes.read();
        let snapshot = vfs.get(&path).ok_or_else(|| anyhow!("file missing"))?;
        let source = snapshot.content.clone();

        let enc = $self.position_encoding;
        let res = $req.request(&$self.tokens_cache, source, enc);
        Ok(CompilerQueryResponse::$method(res))
    }};
}

impl CompileCluster {
    pub fn query(&self, query: CompilerQueryRequest) -> anyhow::Result<CompilerQueryResponse> {
        use CompilerQueryRequest::*;

        match query {
            SemanticTokensFull(req) => query_tokens_cache!(self, SemanticTokensFull, req),
            SemanticTokensDelta(req) => query_tokens_cache!(self, SemanticTokensDelta, req),
            FoldingRange(req) => query_source!(self, FoldingRange, req),
            SelectionRange(req) => query_source!(self, SelectionRange, req),
            DocumentSymbol(req) => query_source!(self, DocumentSymbol, req),
            _ => {
                let main = self.main.lock();

                let query_target = match main.as_ref() {
                    Some(main) => main,
                    None => {
                        // todo: race condition, we need atomic primary query
                        if let Some(path) = query.associated_path() {
                            self.primary.wait().change_entry(path.into())?;
                        }
                        &self.primary
                    }
                };

                query_target.wait().query(query)
            }
        }
    }
}

#[derive(Clone)]
pub struct CompileHandler {
    result: Arc<SyncMutex<Result<Arc<TypstDocument>, CompileStatus>>>,
    inner: Arc<SyncMutex<Option<CompilationHandleImpl>>>,
}

impl CompilationHandle for CompileHandler {
    fn status(&self, status: CompileStatus) {
        let inner = self.inner.lock().unwrap();
        if let Some(inner) = inner.as_ref() {
            inner.status(status);
        }
    }

    fn notify_compile(&self, result: Result<Arc<TypstDocument>, CompileStatus>) {
        *self.result.lock().unwrap() = result.clone();

        let inner = self.inner.lock().unwrap();
        if let Some(inner) = inner.as_ref() {
            inner.notify_compile(result.clone());
        }
    }
}

pub struct CompileDriver {
    inner: CompileDriverInner,
    roots: Vec<PathBuf>,
    handler: CompileHandler,
}

impl CompileMiddleware for CompileDriver {
    type Compiler = CompileDriverInner;

    fn inner(&self) -> &Self::Compiler {
        &self.inner
    }

    fn inner_mut(&mut self) -> &mut Self::Compiler {
        &mut self.inner
    }
}

impl CompileDriver {
    pub fn new(roots: Vec<PathBuf>, opts: CompileOpts, entry: Option<PathBuf>) -> Self {
        let world = TypstSystemWorld::new(opts).expect("incorrect options");
        let mut driver = CompileDriverInner::new(world);

        driver.entry_file = "detached.typ".into();
        // todo: suitable approach to avoid panic
        driver.notify_fs_event(typst_ts_compiler::vfs::notify::FilesystemEvent::Update(
            typst_ts_compiler::vfs::notify::FileChangeSet::new_inserts(vec![(
                driver.world.root.join("detached.typ").into(),
                Ok((Time::now(), Bytes::from("".as_bytes()))).into(),
            )]),
        ));

        let mut this = Self {
            inner: driver,
            roots,
            handler: CompileHandler {
                result: Arc::new(SyncMutex::new(Err(CompileStatus::Compiling))),
                inner: Arc::new(SyncMutex::new(None)),
            },
        };

        if let Some(entry) = entry {
            this.set_entry_file(entry);
        }

        this
    }

    // todo: determine root
    fn set_entry_file(&mut self, entry: PathBuf) {
        let _ = &self.roots;
        // let candidates = self
        //     .current
        //     .iter()
        //     .filter_map(|(root, package)| Some((root,
        // package.uri_to_vpath(uri).ok()?)))     .inspect(|(package_root,
        // path)| trace!(%package_root, ?path, %uri, "considering
        // candidate for full id"));

        // // Our candidates are projects containing a URI, so we expect to get
        // a set of // subdirectories. The "best" is the "most
        // specific", that is, the project that is a // subdirectory of
        // the rest. This should have the longest length.
        // let (best_package_root, best_path) =
        //     candidates.max_by_key(|(_, path)|
        // path.as_rootless_path().components().count())?;

        // let package_id = PackageId::new_current(best_package_root.clone());
        // let full_file_id = FullFileId::new(package_id, best_path);

        self.inner.set_entry_file(entry);
    }
}

pub struct Reporter<C, H> {
    diag_group: String,
    position_encoding: PositionEncoding,
    diag_tx: DiagnosticsSender,
    inner: C,
    cb: H,
}

impl<C: Compiler<World = TypstSystemWorld>, H: CompilationHandle> CompileMiddleware
    for Reporter<C, H>
{
    type Compiler = C;

    fn inner(&self) -> &Self::Compiler {
        &self.inner
    }

    fn inner_mut(&mut self) -> &mut Self::Compiler {
        &mut self.inner
    }

    fn wrap_compile(
        &mut self,
        env: &mut typst_ts_compiler::service::CompileEnv,
    ) -> SourceResult<Arc<TypstDocument>> {
        self.cb.status(CompileStatus::Compiling);
        match self.inner_mut().compile(env) {
            Ok(doc) => {
                self.cb.notify_compile(Ok(doc.clone()));

                self.push_diagnostics(EcoVec::new());
                Ok(doc)
            }
            Err(err) => {
                self.cb.notify_compile(Err(CompileStatus::CompileError));

                self.push_diagnostics(err);
                Err(EcoVec::new())
            }
        }
    }
}

impl<C: Compiler + WorldExporter, H> WorldExporter for Reporter<C, H> {
    fn export(&mut self, output: Arc<typst::model::Document>) -> SourceResult<()> {
        self.inner.export(output)
    }
}

impl<C: Compiler<World = TypstSystemWorld>, H> Reporter<C, H> {
    fn push_diagnostics(&mut self, diagnostics: EcoVec<SourceDiagnostic>) {
        trace!("send diagnostics: {:#?}", diagnostics);

        // todo encoding
        let diagnostics = tinymist_query::convert_diagnostics(
            self.inner.world(),
            diagnostics.as_ref(),
            self.position_encoding,
        );

        // todo: better way to remove diagnostics
        // todo: check all errors in this file

        let main = self.inner.world().main;
        let valid = main.is_some_and(|e| e.vpath() != &VirtualPath::new("detached.typ"));

        let err = self
            .diag_tx
            .send((self.diag_group.clone(), valid.then_some(diagnostics)));
        if let Err(err) = err {
            error!("failed to send diagnostics: {:#}", err);
        }
    }
}

pub struct CompileNode<H: CompilationHandle> {
    diag_group: String,
    position_encoding: PositionEncoding,
    handler: CompileHandler,
    entry: Arc<SyncMutex<Option<ImmutPath>>>,
    inner: Mutex<CompileClient<H>>,
}

// todo: remove unsafe impl send
/// SAFETY:
/// This is safe because the not send types are only used in compiler time
/// hints.
unsafe impl<H: CompilationHandle> Send for CompileNode<H> {}
/// SAFETY:
/// This is safe because the not sync types are only used in compiler time
/// hints.
unsafe impl<H: CompilationHandle> Sync for CompileNode<H> {}

impl<H: CompilationHandle> CompileNode<H> {
    fn inner(&mut self) -> &mut CompileClient<H> {
        self.inner.get_mut()
    }

    /// Steal the compiler thread and run the given function.
    pub fn steal<Ret: Send + 'static>(
        &self,
        f: impl FnOnce(&mut CompileService<H>) -> Ret + Send + 'static,
    ) -> ZResult<Ret> {
        self.inner.lock().steal(f)
    }

    // todo: stop main
    fn disable(&self) {
        let res = self.steal(move |compiler| {
            let path = Path::new("detached.typ");
            let root = compiler.compiler.world().workspace_root();

            let driver = &mut compiler.compiler.compiler.inner.compiler;
            driver.set_entry_file(path.to_owned());

            // todo: suitable approach to avoid panic
            driver.notify_fs_event(typst_ts_compiler::vfs::notify::FilesystemEvent::Update(
                typst_ts_compiler::vfs::notify::FileChangeSet::new_inserts(vec![(
                    root.join("detached.typ").into(),
                    Ok((Time::now(), Bytes::from("".as_bytes()))).into(),
                )]),
            ));
        });
        if let Err(err) = res {
            error!("failed to disable main: {:#}", err);
        }
    }

    fn change_entry(&self, path: ImmutPath) -> Result<(), Error> {
        if !path.is_absolute() {
            return Err(error_once!("entry file must be absolute", path: path.display()));
        }

        // todo: more robust rollback logic
        let entry = self.entry.clone();
        let should_change = {
            let mut entry = entry.lock().unwrap();
            let should_change = entry.as_ref().map(|e| e != &path).unwrap_or(true);
            let prev = entry.clone();
            *entry = Some(path.clone());

            should_change.then_some(prev)
        };

        if let Some(prev) = should_change {
            let next = path.clone();

            debug!(
                "the entry file of TypstActor({}) is changed to {}",
                self.diag_group,
                next.display()
            );

            let res = self.steal(move |compiler| {
                let root = compiler.compiler.world().workspace_root();
                if !path.starts_with(&root) {
                    warn!("entry file is not in workspace root {}", path.display());
                    return;
                }

                let driver = &mut compiler.compiler.compiler.inner.compiler;
                driver.set_entry_file(path.as_ref().to_owned());
            });

            if res.is_err() {
                let mut entry = entry.lock().unwrap();
                if *entry == Some(next) {
                    *entry = prev;
                }

                return res;
            }

            // todo: trigger recompile
            let files = FileChangeSet::new_inserts(vec![]);
            let inner = self.inner.lock();
            inner.add_memory_changes(MemoryEvent::Update(files))
        }

        Ok(())
    }
}

impl<H: CompilationHandle> SourceFileServer for CompileNode<H> {
    async fn resolve_source_span(
        &mut self,
        loc: Location,
    ) -> Result<Option<SourceSpanOffset>, Error> {
        let Location::Src(src_loc) = loc;
        self.inner().resolve_src_location(src_loc).await
    }

    async fn resolve_document_position(
        &mut self,
        loc: Location,
    ) -> Result<Option<Position>, Error> {
        let Location::Src(src_loc) = loc;

        let path = Path::new(&src_loc.filepath).to_owned();
        let line = src_loc.pos.line;
        let column = src_loc.pos.column;

        self.inner()
            .resolve_src_to_doc_jump(path, line, column)
            .await
    }

    async fn resolve_source_location(
        &mut self,
        s: Span,
        offset: Option<usize>,
    ) -> Result<Option<DocToSrcJumpInfo>, Error> {
        Ok(self
            .inner()
            .resolve_span_and_offset(s, offset)
            .await
            .map_err(|err| {
                error!("TypstActor: failed to resolve span and offset: {:#}", err);
            })
            .ok()
            .flatten()
            .map(|e| DocToSrcJumpInfo {
                filepath: e.filepath,
                start: e.start,
                end: e.end,
            }))
    }
}

impl<H: CompilationHandle> EditorServer for CompileNode<H> {
    async fn update_memory_files(
        &mut self,
        files: MemoryFiles,
        reset_shadow: bool,
    ) -> Result<(), Error> {
        // todo: is it safe to believe that the path is normalized?
        let now = std::time::SystemTime::now();
        let files = FileChangeSet::new_inserts(
            files
                .files
                .into_iter()
                .map(|(path, content)| {
                    let content = content.as_bytes().into();
                    // todo: cloning PathBuf -> Arc<Path>
                    (path.into(), Ok((now, content)).into())
                })
                .collect(),
        );
        self.inner().add_memory_changes(if reset_shadow {
            MemoryEvent::Sync(files)
        } else {
            MemoryEvent::Update(files)
        });

        Ok(())
    }

    async fn remove_shadow_files(&mut self, files: MemoryFilesShort) -> Result<(), Error> {
        // todo: is it safe to believe that the path is normalized?
        let files = FileChangeSet::new_removes(files.files.into_iter().map(From::from).collect());
        self.inner().add_memory_changes(MemoryEvent::Update(files));

        Ok(())
    }
}

impl<H: CompilationHandle> CompileHost for CompileNode<H> {}

impl<H: CompilationHandle> CompileNode<H> {
    fn new(
        diag_group: String,
        position_encoding: PositionEncoding,
        handler: CompileHandler,
        inner: CompileClient<H>,
    ) -> Self {
        Self {
            diag_group,
            position_encoding,
            handler,
            entry: Arc::new(SyncMutex::new(None)),
            inner: Mutex::new(inner),
        }
    }

    pub fn query(&self, query: CompilerQueryRequest) -> anyhow::Result<CompilerQueryResponse> {
        use CompilerQueryRequest::*;
        assert!(query.fold_feature() != FoldRequestFeature::ContextFreeUnique);

        match query {
            CompilerQueryRequest::OnSaveExport(OnSaveExportRequest { path }) => {
                self.on_save_export(path)?;
                Ok(CompilerQueryResponse::OnSaveExport(()))
            }
            Hover(req) => query_state!(self, Hover, req),
            GotoDefinition(req) => query_world!(self, GotoDefinition, req),
            InlayHint(req) => query_world!(self, InlayHint, req),
            Completion(req) => query_state!(self, Completion, req),
            SignatureHelp(req) => query_world!(self, SignatureHelp, req),
            Rename(req) => query_world!(self, Rename, req),
            PrepareRename(req) => query_world!(self, PrepareRename, req),
            Symbol(req) => query_world!(self, Symbol, req),
            FoldingRange(..)
            | SelectionRange(..)
            | SemanticTokensDelta(..)
            | DocumentSymbol(..)
            | SemanticTokensFull(..) => unreachable!(),
        }
    }

    fn on_save_export(&self, _path: PathBuf) -> anyhow::Result<()> {
        Ok(())
    }

    fn steal_world<T: Send + Sync + 'static>(
        &self,
        f: impl FnOnce(&TypstSystemWorld) -> T + Send + Sync + 'static,
    ) -> anyhow::Result<T> {
        let mut client = self.inner.lock();
        let fut = client.steal(move |compiler| f(compiler.compiler.world()));

        Ok(fut?)
    }
}
