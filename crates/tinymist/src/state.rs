//! Bootstrap actors for Tinymist.

use std::path::PathBuf;

use lsp_types::TextDocumentContentChangeEvent;
use tinymist_query::{lsp_to_typst, PositionEncoding};
use typst::{diag::FileResult, syntax::Source};
use typst_ts_compiler::vfs::notify::{FileChangeSet, MemoryEvent};
use typst_ts_compiler::Time;
use typst_ts_core::{error::prelude::*, Bytes, Error as TypError, ImmutPath};

use crate::{compile::CompileState, LanguageState};

impl CompileState {
    /// Focus main file to some path.
    pub async fn do_change_entry(
        &mut self,
        new_entry: Option<ImmutPath>,
    ) -> Result<bool, TypError> {
        self.compiler
            .as_mut()
            .unwrap()
            .change_entry(new_entry)
            .await
    }
}

impl LanguageState {
    /// Pin the entry to the given path
    pub async fn pin_entry(&mut self, new_entry: Option<ImmutPath>) -> Result<(), TypError> {
        self.pinning = new_entry.is_some();
        self.primary.do_change_entry(new_entry).await?;

        if !self.pinning {
            let fallback = self.config.compile.determine_default_entry_path();
            let fallback = fallback.or_else(|| self.focusing.clone());
            if let Some(e) = fallback {
                self.primary.do_change_entry(Some(e)).await?;
            }
        }

        Ok(())
    }

    /// Updates the primary (focusing) entry
    pub async fn focus_entry(&mut self, new_entry: Option<ImmutPath>) -> Result<bool, TypError> {
        if self.pinning || self.config.compile.has_default_entry_path {
            self.focusing = new_entry;
            return Ok(false);
        }

        self.primary.do_change_entry(new_entry.clone()).await
    }

    /// This is used for tracking activating document status if a client is not
    /// performing any focus command request.
    ///
    /// See https://github.com/microsoft/language-server-protocol/issues/718
    ///
    /// We do want to focus the file implicitly by `textDocument/diagnostic`
    /// (pullDiagnostics mode), as suggested by language-server-protocol#718,
    /// however, this has poor support, e.g. since neovim 0.10.0.
    pub async fn implicit_focus_entry(
        &mut self,
        new_entry: impl FnOnce() -> Option<ImmutPath>,
        site: char,
    ) {
        if self.ever_manual_focusing {
            return;
        }
        // didOpen
        match site {
            // foldingRange, hover, semanticTokens
            'f' | 'h' | 't' => {
                self.ever_focusing_by_activities = true;
            }
            // didOpen
            _ => {
                if self.ever_focusing_by_activities {
                    return;
                }
            }
        }

        let new_entry = new_entry();

        match self.focus_entry(new_entry.clone()).await {
            Ok(true) => log::info!("file focused[implicit,{site}]: {new_entry:?}"),
            Err(err) => log::warn!("could not focus file: {err}"),
            Ok(false) => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryFileMeta {
    pub mt: Time,
    pub content: Source,
}

impl LanguageState {
    fn update_source(&self, files: FileChangeSet) -> Result<(), TypError> {
        let primary = Some(self.primary());
        let clients_to_notify =
            (primary.into_iter()).chain(self.dedicates.iter().map(CompileState::compiler));

        for client in clients_to_notify {
            client.add_memory_changes(MemoryEvent::Update(files.clone()));
        }

        Ok(())
    }

    pub fn create_source(&mut self, path: PathBuf, content: String) -> Result<(), TypError> {
        let now = Time::now();
        let path: ImmutPath = path.into();

        self.primary.memory_changes.insert(
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

    pub fn remove_source(&mut self, path: PathBuf) -> Result<(), TypError> {
        let path: ImmutPath = path.into();

        self.primary.memory_changes.remove(&path);
        log::info!("remove source: {:?}", path);

        // todo: is it safe to believe that the path is normalized?
        let files = FileChangeSet::new_removes(vec![path]);

        self.update_source(files)
    }

    pub fn edit_source(
        &mut self,
        path: PathBuf,
        content: Vec<TextDocumentContentChangeEvent>,
        position_encoding: PositionEncoding,
    ) -> Result<(), TypError> {
        let now = Time::now();
        let path: ImmutPath = path.into();

        let meta = self
            .primary
            .memory_changes
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

        let files = FileChangeSet::new_inserts(vec![(path.clone(), snapshot)]);

        self.update_source(files)
    }
}
