use std::path::PathBuf;

use lsp_types::TextDocumentIdentifier;
use serde::Serialize;
use serde_json::to_value;
use tinymist_query::{self as q, url_to_path};
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
            ("tinymist.doGetTemplateEntry", Self::get_template_entry),
            ("tinymist.interactCodeContext", Self::interact_code_context),
            // ("tinymist.getDocumentTrace", Self::get_document_trace),
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
            return invalid_params("expect path at args[0]");
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
            return invalid_params("expect path at args[0]");
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
    pub fn init_template(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        #[derive(Debug, Serialize)]
        #[serde(rename_all = "camelCase")]
        struct InitResult {
            entry_path: PathBuf,
        }
        let Some(from_source) = parse_arg::<String>(&args, 0) else {
            return invalid_params("expect source at args[0]");
        };
        let Some(to_path) = parse_arg::<Option<ImmutPath>>(&args, 1) else {
            return invalid_params("expect path at args[1]");
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
                Ok(res) => match to_value(res) {
                    Ok(res) => Ok(res),
                    Err(err) => internal_error_("cannot serialize path"),
                },
                Err(err) => invalid_params_(format!("cannot determine template source: {err}")),
            }
        })
    }

    /// Get the entry of a template.
    pub fn get_template_entry(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(from_source) = parse_arg::<String>(&args, 0) else {
            return invalid_params("expect source at args[0]");
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
                .map_err(map_string_err("failed to parse package spec"))?;

            let from_source = TemplateSource::Package(spec);

            let entry = package::get_entry(c.compiler.world(), from_source)
                .map_err(map_string_err("failed to get template entry"))?;

            ZResult::Ok(entry)
        });
        Box::pin(async move {
            match fut.await.and_then(|e| e) {
                Ok(res) => match String::from_utf8(res.to_vec()) {
                    Ok(res) => Ok(JsonValue::String(res)),
                    Err(err) => invalid_params_("template entry is not a valid UTF-8 string"),
                },
                Err(err) => invalid_params_(format!("cannot determine template entry: {err}")),
            }
        })
    }

    /// Interact with the code context at the source file.
    pub fn interact_code_context(
        &mut self,
        args: Vec<JsonValue>,
    ) -> ResponseFuture<ExecuteCommand> {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct InteractCodeContextParams {
            pub text_document: TextDocumentIdentifier,
            pub query: Vec<tinymist_query::InteractCodeContextQuery>,
        }
        let Some(params) = parse_arg::<InteractCodeContextParams>(&args, 0) else {
            return invalid_params("expect code context queries at args[0]");
        };
        let req = q::InteractCodeContextRequest {
            path: url_to_path(params.text_document),
            query: params.query,
        };
        query_source!(self, req)
    }

    /// Get the metrics of the document.
    pub fn get_document_metrics(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(path) = parse_arg::<ImmutPath>(&args, 0) else {
            return invalid_params("expect path at args[0]");
        };
        let req = q::DocumentMetricsRequest { path };
        query_state!(self, req)
    }

    /// Get the server info.
    pub fn get_server_info(&mut self, _args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        self.primary().collect_server_info();
        todo!("make collect_server_info async")
    }

    // Get static resources with help of tinymist service, for example, a
    /// static help pages for some typst function.
    pub fn get_resources(&mut self, args: Vec<JsonValue>) -> ResponseFuture<ExecuteCommand> {
        let Some(path) = parse_arg::<ImmutPath>(&args, 0) else {
            return invalid_params("expect path at args[0]");
        };
        let Some(handler) = self.resource_routes.get(&path) else {
            return method_not_found(format!("unknown resource: {}", path));
        };
        handler(self, args)
    }
}

impl LanguageState {
    pub fn get_resource_routes() -> ExecCmdMap<Self> {
        HashMap::from_iter([
            ("/symbols", Self::resource_symbols),
            ("/tutorial", Self::resource_tutoral),
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
