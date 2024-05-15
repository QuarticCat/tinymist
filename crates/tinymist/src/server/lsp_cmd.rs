use std::path::PathBuf;

use serde::Serialize;
use serde_json::to_value;
use typst::diag::StrResult;
use typst::syntax::package::{PackageSpec, VersionlessPackageSpec};
use typst_ts_core::{error::prelude::*, ImmutPath};

use super::lsp::*;
use super::*;
use crate::tools::package::InitTask;
use crate::tools::package::{self, determine_latest_version, TemplateSource};

impl LanguageState {
    pub fn get_exec_cmds() -> ExecCmdMap<Self> {
        HashMap::from_iter([
            ("tinymist.exportPdf", Self::export_pdf),
            ("tinymist.exportSvg", Self::export_svg),
            ("tinymist.exportPng", Self::export_png),
            ("tinymist.doClearCache", Self::clear_cache),
            ("tinymist.pinMain", Self::pin_document),
            ("tinymist.focusMain", Self::focus_document),
            ("tinymist.doInitTemplate", Self::init_template),
            ("tinymist.doGetTemplateEntry", Self::do_get_template_entry),
            ("tinymist.interactCodeContext", Self::interact_code_context),
            ("tinymist.getDocumentTrace", Self::get_document_trace),
            ("tinymist.getDocumentMetrics", Self::get_document_metrics),
            ("tinymist.getServerInfo", Self::get_server_info),
            ("tinymist.getResources", Self::get_resources),
        ])
    }

    /// Export the current document as a PDF file.
    pub fn export_pdf(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.primary.export_pdf(args)
    }

    /// Export the current document as a Svg file.
    pub fn export_svg(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.primary.export_svg(args)
    }

    /// Export the current document as a Png file.
    pub fn export_png(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.primary.export_png(args)
    }

    /// Clear all cached resources.
    pub fn clear_cache(&mut self, _args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.primary.clear_cache(Vec::new());
        for v in &mut self.dedicates {
            v.clear_cache(Vec::new());
        }
        ok(JsonValue::Null)
    }

    /// Pin main file to some path.
    pub fn pin_document(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(entry) = parse_arg::<Option<ImmutPath>>(&args, 0) else {
            return invalid_params("expect path at arg[0]");
        };
        if let Err(err) = self.pin_entry(entry.clone()) {
            return internal_error(format!("cannot pin file: {err}"));
        }
        log::info!("file pinned: {entry:?}");
        ok(JsonValue::Null)
    }

    /// Focus main file to some path.
    pub fn focus_document(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(entry) = parse_arg::<Option<ImmutPath>>(&args, 0) else {
            return invalid_params("expect path at arg[0]");
        };
        if !self.ever_manual_focusing {
            self.ever_manual_focusing = true;
            log::info!("first manual focusing is coming");
        }
        if let Err(err) = self.focus_entry(entry.clone()) {
            return internal_error(format!("cannot focus file: {err}"));
        }
        log::info!("file focused: {entry:?}");
        ok(JsonValue::Null)
    }

    /// Initialize a new template.
    pub fn init_template(&self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        #[derive(Debug, Serialize)]
        #[serde(rename_all = "camelCase")]
        struct InitResult {
            entry_path: PathBuf,
        }
        let Some(from_source) = parse_arg::<String>(&args, 0) else {
            return invalid_params("expect source at arg[0]");
        };
        let Some(to_path) = parse_arg::<Option<ImmutPath>>(&args, 1) else {
            return invalid_params("expect path at arg[1]");
        };
        let fut = self.primary().steal(move |c| {
            // Parse the package specification. If the user didn't specify the version,
            // we try to figure it out automatically by downloading the package index
            // or searching the disk.
            let spec: PackageSpec = from_source
                .parse()
                .or_else(|err| {
                    // Try to parse without version, but prefer the error message of the
                    // normal package spec parsing if it fails.
                    let spec: VersionlessPackageSpec = from_source.parse().map_err(|_| err)?;
                    let version = determine_latest_version(c.compiler.world(), &spec)?;
                    StrResult::Ok(spec.at(version))
                })
                .map_err(map_string_err("cannot parse package spec"))?;

            let from_source = TemplateSource::Package(spec);

            let entry_path = package::init(
                c.compiler.world(),
                InitTask {
                    tmpl: from_source.clone(),
                    dir: to_path.clone(),
                },
            )
            .map_err(map_string_err("cannot initialize template"))?;

            log::info!("template initialized: {from_source:?} to {to_path:?}");

            ZResult::Ok(InitResult { entry_path })
        });
        Box::pin(async move {
            match fut.await.and_then(|e| e) {
                Ok(res) => to_value(res).map_err(internal_error_),
                Err(err) => invalid_params_(format!("cannot determine template source: {err}")),
            }
        })
    }
}
