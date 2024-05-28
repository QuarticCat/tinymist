use std::path::PathBuf;

use lsp_types::TextDocumentIdentifier;
use serde::{Deserialize, Serialize};
use serde_json::to_value;
use tinymist_query::{self as q, url_to_path};
use typst::diag::StrResult;
use typst::syntax::package::{PackageSpec, VersionlessPackageSpec};
use typst_ts_compiler::service::Compiler;
use typst_ts_core::error::prelude::*;

use super::lsp::*;
use super::*;
use crate::tools::package::InitTask;
use crate::tools::package::{self, determine_latest_version, TemplateSource};

impl LanguageState {
    #[rustfmt::skip]
    pub fn get_exec_cmds() -> ExecCmdMap<Self> {
        HashMap::from_iter([
            ("tinymist.exportPdf", Self::export_pdf as _),
            ("tinymist.exportSvg", Self::export_svg as _),
            ("tinymist.exportPng", Self::export_png as _),
            ("tinymist.doClearCache", Self::clear_cache as _),
            ("tinymist.pinMain", Self::pin_document as _),
            ("tinymist.focusMain", Self::focus_document as _),
            ("tinymist.doInitTemplate", Self::init_template as _),
            ("tinymist.doGetTemplateEntry", Self::get_template_entry as _),
            ("tinymist.interactCodeContext", Self::interact_code_context as _),
            // ("tinymist.getDocumentTrace", Self::get_document_trace as _),
            ("tinymist.getDocumentMetrics", Self::get_document_metrics as _),
            ("tinymist.getServerInfo", Self::get_server_info as _),
            ("tinymist.getResources", Self::get_resources as _),
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
        Box::pin(ready(Ok(Some(JsonValue::Null))))
    }

    /// Pin main file to some path.
    pub fn pin_document(&mut self, mut args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let entry = get_arg!(args[0] as Option<PathBuf>).map(Into::into);
        Box::pin(async move {
            match self.pin_entry(entry).await {
                Ok(_) => Ok(Some(JsonValue::Null)),
                Err(err) => internal_error_(format!("cannot pin file: {err}")),
            }
        })
    }

    /// Focus main file to some path.
    pub fn focus_document(&mut self, mut args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let entry = get_arg!(args[0] as Option<PathBuf>);
        if !self.ever_manual_focusing {
            self.ever_manual_focusing = true;
            log::info!("first manual focusing is coming");
        }
        let entry = entry.map(Into::into);
        Box::pin(async move {
            match self.focus_entry(entry).await {
                Ok(_) => Ok(Some(JsonValue::Null)),
                Err(err) => internal_error_(format!("cannot focus file: {err}")),
            }
        })
    }

    /// Initialize a new template.
    pub fn init_template(&mut self, mut args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        #[derive(Debug, Serialize)]
        #[serde(rename_all = "camelCase")]
        struct InitResult {
            entry_path: PathBuf,
        }
        let from_source = get_arg!(args[0] as String);
        let to_path = get_arg!(args[1] as Option<PathBuf>);
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
                    tmpl: from_source,
                    dir: to_path.map(Into::into),
                },
            )
            .map_err(map_string_err("cannot initialize template"))?;

            ZResult::Ok(InitResult { entry_path })
        });
        Box::pin(async move {
            match fut.await.and_then(|e| e) {
                Ok(res) => match to_value(res) {
                    Ok(res) => Ok(Some(res)),
                    Err(err) => internal_error_("cannot serialize path"),
                },
                Err(err) => invalid_params_(format!("cannot determine template source: {err}")),
            }
        })
    }

    /// Get the entry of a template.
    pub fn get_template_entry(
        &mut self,
        mut args: Vec<JsonValue>,
    ) -> ResponseFuture<ExecuteCommand> {
        let from_source = get_arg!(args[0] as String);
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
                .map_err(map_string_err("failed to parse package spec"))?;

            let from_source = TemplateSource::Package(spec);

            let entry = package::get_entry(c.compiler.world(), from_source)
                .map_err(map_string_err("failed to get template entry"))?;

            ZResult::Ok(entry)
        });
        Box::pin(async move {
            match fut.await.and_then(|e| e) {
                Ok(res) => match String::from_utf8(res.to_vec()) {
                    Ok(res) => Ok(Some(JsonValue::String(res))),
                    Err(err) => invalid_params_("template entry is not a valid UTF-8 string"),
                },
                Err(err) => invalid_params_(format!("cannot determine template entry: {err}")),
            }
        })
    }

    /// Interact with the code context at the source file.
    pub fn interact_code_context(
        &mut self,
        mut args: Vec<JsonValue>,
    ) -> ResponseFuture<ExecuteCommand> {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct InteractCodeContextParams {
            pub text_document: TextDocumentIdentifier,
            pub query: Vec<tinymist_query::InteractCodeContextQuery>,
        }
        let params = get_arg!(args[0] as InteractCodeContextParams);
        let req = q::InteractCodeContextRequest {
            path: url_to_path(params.text_document.uri),
            query: params.query,
        };
        query_source!(self, req)
    }

    /// Get the metrics of the document.
    pub fn get_document_metrics(
        &mut self,
        mut args: Vec<JsonValue>,
    ) -> ResponseFuture<ExecuteCommand> {
        let path = get_arg!(args[0] as PathBuf);
        let req = q::DocumentMetricsRequest { path: path.into() };
        query_state!(self, req)
    }

    /// Get the server info.
    pub fn get_server_info(&mut self, _args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.primary().collect_server_info();
        todo!("make collect_server_info async")
    }

    // Get static resources with help of tinymist service, for example, a
    /// static help pages for some typst function.
    pub fn get_resources(&mut self, mut args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let path = get_arg!(args[0] as PathBuf);
        let Some(handler) = self.resource_routes.get(path.as_path()) else {
            return method_not_found(format!("unknown resource: {path:?}"));
        };
        handler(self, args)
    }
}

impl LanguageState {
    #[rustfmt::skip]
    pub fn get_resource_routes() -> ResourceMap<Self> {
        HashMap::from_iter([
            (Path::new("/symbols"), Self::resource_symbols as _),
            (Path::new("/tutorial"), Self::resource_tutoral as _),
        ])
    }

    /// Get the all valid symbols
    pub fn resource_symbols(&mut self, _args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        match self.get_symbol_resources() {
            Ok(res) => ok(res),
            Err(err) => internal_error(err),
        }
    }

    /// Get tutorial web page
    pub fn resource_tutoral(&mut self, _args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        method_not_found("unimplemented")
    }
}
