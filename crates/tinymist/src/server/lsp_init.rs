use std::path::PathBuf;

use anyhow::bail;
use itertools::Itertools;
use lsp_types::request::*;
use lsp_types::*;
use serde::Deserialize;
use serde_json::{Map, Value as JsonValue};
use tinymist_query::{get_semantic_tokens_options, PositionEncoding};
use tokio::sync::mpsc;
use typst::util::Deferred;
use typst_ts_core::ImmutPath;

use super::compiler_init::*;
use super::lsp::*;
use super::*;
use crate::actor::editor::EditorActor;
use crate::utils::{try_, try_or};
use crate::world::{ImmutDict, SharedFontResolver};

// todo: svelte-language-server responds to a Goto Definition request with
// LocationLink[] even if the client does not report the
// textDocument.definition.linkSupport capability.

/// The mode of the formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FormatterMode {
    /// Disable the formatter.
    #[default]
    Disable,
    /// Use `typstyle` formatter.
    Typstyle,
    /// Use `typstfmt` formatter.
    Typstfmt,
}

/// The mode of PDF/SVG/PNG export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportMode {
    #[default]
    Auto,
    /// Select best solution automatically. (Recommended)
    Never,
    /// Export on saving the document, i.e. on `textDocument/didSave` events.
    OnSave,
    /// Export on typing, i.e. on `textDocument/didChange` events.
    OnType,
    /// Export when a document has a title, which is useful to filter out
    /// template files.
    OnDocumentHasTitle,
}

/// The mode of semantic tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SemanticTokensMode {
    /// Disable the semantic tokens.
    Disable,
    /// Enable the semantic tokens.
    #[default]
    Enable,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompileExtraOpts {
    /// The root directory for compilation routine.
    pub root_dir: Option<PathBuf>,
    /// Path to entry
    pub entry: Option<ImmutPath>,
    /// Additional input arguments to compile the entry file.
    pub inputs: ImmutDict,
    /// will remove later
    pub font_paths: Vec<PathBuf>,
}

const CONFIG_ITEMS: &[&str] = &[
    "outputPath",
    "exportPdf",
    "rootPath",
    "semanticTokens",
    "formatterMode",
    "formatterPrintWidth",
    "typstExtraArgs",
    "compileStatus",
    "preferredTheme",
    "hoverPeriscope",
];

/// The user configuration read from the editor.
#[derive(Debug, Default, Clone)]
pub struct LanguageConfig {
    /// Specifies the root path of the project manually.
    pub notify_compile_status: bool,
    /// The compile configurations
    pub compile: CompileConfig,
    /// Dynamic configuration for semantic tokens.
    pub semantic_tokens: SemanticTokensMode,
    /// Dynamic configuration for the experimental formatter.
    pub formatter: FormatterMode,
    /// Dynamic configuration for the experimental formatter.
    pub formatter_print_width: u32,
}

impl LanguageConfig {
    /// Gets items for serialization.
    pub fn get_items() -> Vec<ConfigurationItem> {
        let sections = CONFIG_ITEMS
            .iter()
            .flat_map(|item| [format!("tinymist.{item}"), item.to_string()]);

        sections
            .map(|section| ConfigurationItem {
                section: Some(section),
                ..Default::default()
            })
            .collect()
    }

    /// Converts values to a map.
    pub fn values_to_map(values: Vec<JsonValue>) -> Map<String, JsonValue> {
        let unpaired_values = values
            .into_iter()
            .tuples()
            .map(|(a, b)| if !a.is_null() { a } else { b });

        CONFIG_ITEMS
            .iter()
            .map(|item| item.to_string())
            .zip(unpaired_values)
            .collect()
    }

    /// Updates the configuration with a JSON object.
    ///
    /// # Errors
    /// Errors if the update is invalid.
    pub fn update(&mut self, update: &JsonValue) -> anyhow::Result<()> {
        if let JsonValue::Object(update) = update {
            self.update_by_map(update)
        } else {
            bail!("got invalid configuration object {update}")
        }
    }

    /// Updates the configuration with a map.
    ///
    /// # Errors
    /// Errors if the update is invalid.
    pub fn update_by_map(&mut self, update: &Map<String, JsonValue>) -> anyhow::Result<()> {
        try_(|| SemanticTokensMode::deserialize(update.get("semanticTokens")?).ok())
            .inspect(|v| self.semantic_tokens = *v);
        try_(|| FormatterMode::deserialize(update.get("formatterMode")?).ok())
            .inspect(|v| self.formatter = *v);
        try_(|| u32::deserialize(update.get("formatterPrintWidth")?).ok())
            .inspect(|v| self.formatter_print_width = *v);
        self.compile.update_by_map(update)?;
        self.compile.validate()
    }
}

/// Configuration set at initialization that won't change within a single
/// session.
#[derive(Debug, Clone, Default)]
pub struct ConstLanguageConfig {
    /// Determined position encoding, either UTF-8 or UTF-16 (default).
    pub position_encoding: PositionEncoding,
    /// Allow dynamic registration of configuration changes.
    pub cfg_change_registration: bool,
    /// Allow dynamic registration of semantic tokens.
    pub tokens_dynamic_registration: bool,
    /// Allow overlapping tokens.
    pub tokens_overlapping_token_support: bool,
    /// Allow multiline tokens.
    pub tokens_multiline_token_support: bool,
    /// Allow line folding on documents.
    pub doc_line_folding_only: bool,
    /// Allow dynamic registration of document formatting.
    pub doc_fmt_dynamic_registration: bool,
}

impl From<&InitializeParams> for ConstLanguageConfig {
    fn from(params: &InitializeParams) -> Self {
        const DEFAULT_ENCODING: &[PositionEncodingKind] = &[PositionEncodingKind::UTF16];

        let position_encoding = {
            let general = params.capabilities.general.as_ref();
            let encodings = try_(|| Some(general?.position_encodings.as_ref()?.as_slice()));
            if encodings.is_some_and(|e| e.contains(&PositionEncodingKind::UTF8)) {
                PositionEncoding::Utf8
            } else {
                PositionEncoding::Utf16
            }
        };

        let workspace = params.capabilities.workspace.as_ref();
        let doc = params.capabilities.text_document.as_ref();
        let sema = try_(|| doc?.semantic_tokens.as_ref());
        let fold = try_(|| doc?.folding_range.as_ref());
        let format = try_(|| doc?.formatting.as_ref());

        Self {
            position_encoding,
            cfg_change_registration: try_or(|| workspace?.configuration, false),
            tokens_dynamic_registration: try_or(|| sema?.dynamic_registration, false),
            tokens_overlapping_token_support: try_or(|| sema?.overlapping_token_support, false),
            tokens_multiline_token_support: try_or(|| sema?.multiline_token_support, false),
            doc_line_folding_only: try_or(|| fold?.line_folding_only, true),
            doc_fmt_dynamic_registration: try_or(|| format?.dynamic_registration, false),
        }
    }
}

// todo: not yet fully migrated
impl LanguageState {
    pub(crate) fn init(&mut self, params: InitializeParams) -> ResponseResult<Initialize> {
        // Initialize configurations.
        let cc = ConstLanguageConfig::from(&params);
        log::info!("initialized with const_config {cc:?}");
        let mut config = LanguageConfig {
            compile: CompileConfig {
                roots: match params.workspace_folders.as_ref() {
                    Some(roots) => roots
                        .iter()
                        .filter_map(|root| root.uri.to_file_path().ok())
                        .collect::<Vec<_>>(),
                    #[allow(deprecated)] // `params.root_path` is marked as deprecated
                    None => params
                        .root_uri
                        .as_ref()
                        .map(|uri| uri.to_file_path().unwrap())
                        .or_else(|| params.root_path.clone().map(PathBuf::from))
                        .into_iter()
                        .collect(),
                },
                ..CompileConfig::default()
            },
            ..LanguageConfig::default()
        };
        if let Some(init) = &params.initialization_options {
            config.update(init).or_else(invalid_params)?;
        };

        // Prepare fonts.
        // todo: on font resolving failure, downgrade to a fake font book
        let font = {
            let mut opts = std::mem::take(&mut self.compile_opts);
            if opts.font_paths.is_empty() {
                if let Some(font_paths) = config
                    .compile
                    .typst_extra_args
                    .as_ref()
                    .map(|x| &x.font_paths)
                {
                    opts.font_paths.clone_from(font_paths);
                }
            }
            Deferred::new(|| SharedFontResolver::new(opts).expect("failed to create font book"))
        };

        // Bootstrap server.
        let (editor_tx, editor_rx) = mpsc::unbounded_channel();

        log::info!("initialized with config {:?}", config);
        self.primary.config = config.compile.clone();
        self.config = config;

        self.run_format_thread();
        self.run_user_action_thread();

        let editor_actor = EditorActor::new(
            self.host.clone(),
            editor_rx,
            self.config.compile.notify_compile_status,
        );

        let fallback = self.config.compile.determine_default_entry_path();
        let primary = self.server(
            "primary".to_owned(),
            self.config.compile.determine_entry(fallback),
            self.config.compile.determine_inputs(),
        );
        if self.primary.compiler.is_some() {
            panic!("primary already initialized");
        }
        self.primary.compiler = Some(primary);

        // Run the cluster in the background after we referencing it.
        tokio::spawn(editor_actor.run());

        // Register these capabilities statically if the client does not support dynamic
        // registration.
        let semantic_tokens_provider = (!cc.tokens_dynamic_registration
            && self.config.semantic_tokens == SemanticTokensMode::Enable)
            .then(|| get_semantic_tokens_options().into());
        let document_formatting_provider = (!cc.doc_fmt_dynamic_registration
            && self.config.formatter != FormatterMode::Disable)
            .then(|| OneOf::Left(true));

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // todo: respect position_encoding
                // position_encoding: Some(cc.position_encoding.into()),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    // Please update the language-configurations.json if you are changing this
                    // setting.
                    trigger_characters: Some(vec![
                        String::from("#"),
                        String::from("("),
                        String::from(","),
                        String::from("."),
                        String::from(":"),
                        String::from("/"),
                        String::from("\""),
                        String::from("@"),
                    ]),
                    ..Default::default()
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                semantic_tokens_provider,
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: self.exec_cmds.keys().map(ToString::to_string).collect(),
                    ..Default::default()
                }),
                color_provider: Some(ColorProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    ..Default::default()
                }),
                document_formatting_provider,
                inlay_hint_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    pub(crate) fn inited(&mut self, params: InitializedParams) {
        if self.const_config.tokens_dynamic_registration
            && self.config.semantic_tokens == SemanticTokensMode::Enable
        {
            let err = self.enable_sema_token_caps(true);
            if let Err(err) = err {
                log::error!("could not register semantic tokens for initialization: {err}");
            }
        }

        if self.const_config.doc_fmt_dynamic_registration
            && self.config.formatter != FormatterMode::Disable
        {
            let err = self.enable_formatter_caps(true);
            if let Err(err) = err {
                log::error!("could not register formatter for initialization: {err}");
            }
        }

        if self.const_config.cfg_change_registration {
            log::trace!("setting up to request config change notifications");

            const CONFIG_REGISTRATION_ID: &str = "config";
            const CONFIG_METHOD_ID: &str = "workspace/didChangeConfiguration";

            let err = self
                .client
                .register_capability(vec![Registration {
                    id: CONFIG_REGISTRATION_ID.to_owned(),
                    method: CONFIG_METHOD_ID.to_owned(),
                    register_options: None,
                }])
                .err();
            if let Some(err) = err {
                log::error!("could not register to watch config changes: {err}");
            }
        }

        self.primary.initialized(params);
        log::info!("server initialized");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_config_update() {
        let mut config = LanguageConfig::default();

        let root_path = if cfg!(windows) { "C:\\root" } else { "/root" };

        let update = json!({
            "outputPath": "out",
            "exportPdf": "onSave",
            "rootPath": root_path,
            "semanticTokens": "enable",
            "formatterMode": "typstyle",
            "typstExtraArgs": ["--root", root_path]
        });

        config.update(&update).unwrap();

        assert_eq!(config.compile.output_path, "out");
        assert_eq!(config.compile.export_pdf, ExportMode::OnSave);
        assert_eq!(config.compile.root_path, Some(PathBuf::from(root_path)));
        assert_eq!(config.semantic_tokens, SemanticTokensMode::Enable);
        assert_eq!(config.formatter, FormatterMode::Typstyle);
        assert_eq!(
            config.compile.typst_extra_args,
            Some(CompileExtraOpts {
                root_dir: Some(PathBuf::from(root_path)),
                ..Default::default()
            })
        );
    }

    #[test]
    fn test_empty_extra_args() {
        let mut config = LanguageConfig::default();
        let update = json!({
            "typstExtraArgs": []
        });

        config.update(&update).unwrap();
    }

    #[test]
    fn test_reject_abnormal_root() {
        let mut config = LanguageConfig::default();
        let update = json!({
            "rootPath": ".",
        });

        let err = format!("{}", config.update(&update).unwrap_err());
        assert!(err.contains("absolute path"), "unexpected error: {}", err);
    }

    #[test]
    fn test_reject_abnormal_root2() {
        let mut config = LanguageConfig::default();
        let update = json!({
            "typstExtraArgs": ["--root", "."]
        });

        let err = format!("{}", config.update(&update).unwrap_err());
        assert!(err.contains("absolute path"), "unexpected error: {}", err);
    }
}
