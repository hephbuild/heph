/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Based on the reference lsp-server example at <https://github.com/rust-analyzer/lsp-server/blob/master/examples/goto_def.rs>.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::RwLock;

use derivative::Derivative;
use derive_more::Display;
use dupe::Dupe;
use dupe::OptionDupedExt;
use itertools::Itertools;
use lsp_types::CompletionItem;
use lsp_types::CompletionItemKind;
use lsp_types::CompletionOptions;
use lsp_types::CompletionParams;
use lsp_types::CompletionResponse;
use lsp_types::DefinitionOptions;
use lsp_types::Diagnostic;
use lsp_types::Documentation;
use lsp_types::GotoDefinitionParams;
use lsp_types::GotoDefinitionResponse;
use lsp_types::Hover;
use lsp_types::HoverContents;
use lsp_types::HoverParams;
use lsp_types::HoverProviderCapability;
use lsp_types::InitializeParams;
use lsp_types::LanguageString;
use lsp_types::LocationLink;
use lsp_types::MarkedString;
use lsp_types::MarkupContent;
use lsp_types::MarkupKind;
use lsp_types::OneOf;
use lsp_types::Position;
use lsp_types::Range;
use lsp_types::ServerCapabilities;
use lsp_types::TextDocumentSyncCapability;
use lsp_types::TextDocumentSyncKind;
use lsp_types::TextEdit;
use lsp_types::Uri;
use lsp_types::WorkDoneProgressOptions;
use lsp_types::WorkspaceFolder;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use starlark::codemap::ResolvedSpan;
use starlark::codemap::Span;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocModule;
use starlark::docs::markdown::render_doc_item_no_link;
use starlark::docs::markdown::render_doc_param;
use starlark::syntax::AstModule;
use starlark_syntax::codemap::ResolvedPos;
use starlark_syntax::syntax::ast::AstPayload;
use starlark_syntax::syntax::ast::LoadArgP;
use starlark_syntax::syntax::module::AstModuleFields;

use crate::completion::StringCompletionResult;
use crate::completion::StringCompletionType;
use crate::definition::Definition;
use crate::definition::DottedDefinition;
use crate::definition::IdentifierDefinition;
use crate::definition::LspModule;
use crate::inspect::AstModuleInspect;
use crate::inspect::AutocompleteType;
use crate::symbols::find_symbols_at_location;
use crate::uri::file_path_to_uri;
use crate::uri::uri_to_file_path;

/// Errors that can happen when converting LspUri and Uri to/from each other.
#[derive(thiserror::Error, Debug)]
pub enum LspUriError {
    /// The path component of the URI was not absolute. This is required for all supported
    /// URI types.
    #[error("`{}` does not have an absolute path component", **.0)]
    NotAbsolute(Uri),
    /// For some reason the PathBuf/Uri in the LspUri could not be converted back to a URI.
    #[error("`{}` could not be converted back to a URI", .0)]
    Unparsable(LspUri),
    #[error("invalid URI for file:// schema (possibly not absolute?): `{}`", **.0)]
    InvalidFileUri(Uri),
}

/// A URI that represents the two types (plus an "Other") of URIs that are supported.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Display)]
pub enum LspUri {
    /// A "file://" URI with a path sent from the LSP client.
    #[display("file://{}", _0.display())]
    File(PathBuf),
    /// A "starlark:" URI. This is mostly used for native types that don't actually
    /// exist on the filesystem. The path component always has a leading slash.
    #[display("starlark:{}", _0.display())]
    Starlark(PathBuf),
    /// Any other type. Often should just be ignored, or return an error.
    #[display("{}", **_0)]
    Other(Uri),
}

impl Serialize for LspUri {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match Uri::try_from(self) {
            Ok(uri) => uri.serialize(serializer),
            Err(e) => Err(serde::ser::Error::custom(e.to_string())),
        }
    }
}

impl<'de> Deserialize<'de> for LspUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let uri = Uri::deserialize(deserializer)?;
        LspUri::try_from(uri).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

impl LspUri {
    /// Returns the path component of the underlying URI
    pub fn path(&self) -> &Path {
        match self {
            LspUri::File(p) => p.as_path(),
            LspUri::Starlark(p) => p.as_path(),
            LspUri::Other(u) => Path::new(u.path().as_str()),
        }
    }
}

impl TryFrom<Uri> for LspUri {
    type Error = LspUriError;

    fn try_from(uri: Uri) -> Result<Self, Self::Error> {
        match uri.scheme().map(|scheme| scheme.as_str()) {
            Some("file") => {
                let file_path =
                    uri_to_file_path(&uri).ok_or_else(|| LspUriError::InvalidFileUri(uri.clone()))?;
                if file_path.is_absolute() {
                    Ok(Self::File(file_path))
                } else {
                    Err(LspUriError::NotAbsolute(uri))
                }
            }
            Some("starlark") => {
                // Need to perform the replace to standardize on / instead of \ on windows.
                let path = PathBuf::from(uri.path().as_str().replace('\\', "/"));
                // Use "starts with a /" because, while leading slashes are accepted on
                // windows, they do not report "true" from `is_absolute()`.
                if path.to_string_lossy().starts_with('/') {
                    Ok(Self::Starlark(path))
                } else {
                    Err(LspUriError::NotAbsolute(uri))
                }
            }
            _ => Ok(Self::Other(uri)),
        }
    }
}

impl TryFrom<LspUri> for Uri {
    type Error = LspUriError;

    fn try_from(uri: LspUri) -> Result<Self, Self::Error> {
        Uri::try_from(&uri)
    }
}

impl TryFrom<&LspUri> for Uri {
    type Error = LspUriError;

    fn try_from(uri: &LspUri) -> Result<Self, Self::Error> {
        match &uri {
            LspUri::File(p) => {
                file_path_to_uri(p).ok_or_else(|| LspUriError::Unparsable(uri.clone()))
            }
            LspUri::Starlark(p) => Uri::from_str(&format!("starlark:{}", p.display()))
                .map_err(|_| LspUriError::Unparsable(uri.clone())),
            LspUri::Other(u) => Ok(u.clone()),
        }
    }
}

/// The result of resolving a StringLiteral when looking up a definition.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct StringLiteralResult {
    /// The path that a string literal resolves to.
    pub uri: LspUri,
    /// A function that takes the AstModule at path specified by `uri`, and
    /// allows resolving a location to jump to within the specific URI if desired.
    ///
    /// If `None`, then just jump to the URI. Do not attempt to load the file.
    #[derivative(Debug = "ignore")]
    pub location_finder: Option<Box<dyn FnOnce(&AstModule) -> Result<Option<Span>, String> + Send>>,
}

fn _assert_string_literal_result_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<StringLiteralResult>();
}

/// The result of evaluating a starlark program for use in the LSP.
#[derive(Default)]
pub struct LspEvalResult {
    /// The list of diagnostic issues that were encountered while evaluating a starlark program.
    pub diagnostics: Vec<Diagnostic>,
    /// If the program could be parsed, the parsed module.
    pub ast: Option<AstModule>,
}

/// Settings that the LspContext can provide to change what capabilities the server enables
/// or disables.
#[derive(Dupe, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LspServerSettings {
    /// Whether goto definition should work.
    pub enable_goto_definition: bool,
}

impl Default for LspServerSettings {
    fn default() -> Self {
        Self {
            enable_goto_definition: true,
        }
    }
}

/// An error encountered during one of the LSP operations
///
/// Consumed at the top level LSP loop and turned into an error that'll be shown to the user in an
/// IDE.
#[derive(derive_more::From)]
pub enum LspOpError {
    #[from(skip)]
    FromContext(String),
    Uri(LspUriError),
    Other(String),
}

impl LspOpError {
    /// The error as the text an editor shows.
    pub fn format(self) -> String {
        match self {
            LspOpError::FromContext(e) => e,
            LspOpError::Uri(e) => format!("{:#}", e),
            LspOpError::Other(e) => format!("{:#}", e),
        }
    }
}

/// Various pieces of context to allow the LSP to interact with starlark parsers, etc.
pub trait LspContext {
    /// Parse a file with the given contents. The filename is used in the diagnostics.
    fn parse_file_with_contents(&self, uri: &LspUri, content: String) -> LspEvalResult;

    /// Resolve a path given in a `load()` statement.
    ///
    /// `path` is the string representation in the `load()` statement. Its meaning is
    ///        implementation defined.
    /// `current_file` is the the file that is including the `load()` statement, and should be used
    ///                if `path` is "relative" in a semantic sense.
    fn resolve_load(
        &self,
        path: &str,
        current_file: &LspUri,
        workspace_root: Option<&Path>,
    ) -> Result<LspUri, String>;

    /// Render the target URI to use as a path in a `load()` statement. If `target` is
    /// in the same package as `current_file`, the result is a relative path.
    ///
    /// `target` is the file that should be loaded by `load()`.
    /// `current_file` is the file that the `load()` statement will be inserted into.
    fn render_as_load(
        &self,
        target: &LspUri,
        current_file: &LspUri,
        workspace_root: Option<&Path>,
    ) -> Result<String, String>;

    /// Resolve a string literal into a URI and a function that specifies a location within that
    /// target file.
    ///
    /// This can be used for things like file paths in string literals, build targets, etc.
    ///
    /// `current_file` is the file that is currently being evaluated
    fn resolve_string_literal(
        &self,
        literal: &str,
        current_file: &LspUri,
        workspace_root: Option<&Path>,
    ) -> Result<Option<StringLiteralResult>, String>;

    /// Get the contents of a starlark program at a given path, if it exists.
    fn get_load_contents(&self, uri: &LspUri) -> Result<Option<String>, String>;

    /// Get the contents of a file at a given URI, and attempt to parse it.
    fn parse_file(&self, uri: &LspUri) -> Result<Option<LspEvalResult>, String> {
        let result = self
            .get_load_contents(uri)?
            .map(|content| self.parse_file_with_contents(uri, content));
        Ok(result)
    }

    /// Get the preloaded environment for a particular file.
    fn get_environment(&self, uri: &LspUri) -> DocModule;

    /// Get the LspUri for a global symbol if possible.
    ///
    /// The current file is provided in case different files have different global symbols
    /// defined.
    fn get_uri_for_global_symbol(
        &self,
        current_file: &LspUri,
        symbol: &str,
    ) -> Result<Option<LspUri>, String>;

    /// Get valid completion options if possible, based on the kind of string
    /// completion expected (e.g. any string literal, versus the path argument in
    /// a load statement).
    fn get_string_completion_options(
        &self,
        document_uri: &LspUri,
        kind: StringCompletionType,
        current_value: &str,
        workspace_root: Option<&Path>,
    ) -> Result<Vec<StringCompletionResult>, String> {
        let _unused = (document_uri, kind, current_value, workspace_root);
        Ok(Vec::new())
    }
}

/// The analysis half of upstream's server: the last valid parse of every open
/// document and the feature functions over it. heph owns the loop, so this
/// carries no connection; the caller routes requests here and sends the result.
pub struct Backend<T: LspContext> {
    pub context: T,
    /// The `AstModule` from the last time that a file was opened / changed and parsed successfully.
    /// Entries are evicted when the file is closed.
    pub(crate) last_valid_parse: RwLock<HashMap<LspUri, Arc<LspModule>>>,
}

/// The logic implementations of stuff
impl<T: LspContext> Backend<T> {
    pub fn new(context: T) -> Self {
        Backend {
            context,
            last_valid_parse: RwLock::default(),
        }
    }

    /// The capabilities upstream's server declares for these settings.
    pub fn server_capabilities(settings: LspServerSettings) -> ServerCapabilities {
        let definition_provider = settings.enable_goto_definition.then_some({
            OneOf::Right(DefinitionOptions {
                work_done_progress_options: WorkDoneProgressOptions {
                    work_done_progress: None,
                },
            })
        });
        ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
            definition_provider,
            completion_provider: Some(CompletionOptions::default()),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        }
    }

    /// The last valid parse of `uri`, if it was ever opened and parsed.
    pub(crate) fn get_ast(&self, uri: &LspUri) -> Option<Arc<LspModule>> {
        let last_valid_parse = self.last_valid_parse.read().unwrap();
        last_valid_parse.get(uri).duped()
    }

    pub(crate) fn get_ast_or_load_from_disk(
        &self,
        uri: &LspUri,
    ) -> Result<Option<Arc<LspModule>>, LspOpError> {
        let module = match self.get_ast(uri) {
            Some(result) => Some(result),
            None => self
                .context
                .parse_file(uri)
                .map_err(LspOpError::FromContext)?
                .and_then(|eval_result| eval_result.ast.map(|ast| Arc::new(LspModule::new(ast)))),
        };
        Ok(module)
    }

    /// Parse `text` as the new contents of `uri`: a successful parse replaces
    /// the last valid one, a failed one keeps it. Returns the diagnostics to
    /// publish and whether this text parsed. Upstream's `did_open` /
    /// `did_change`, minus the notification.
    pub fn validate(&self, uri: &LspUri, text: String) -> (Vec<Diagnostic>, bool) {
        let eval_result = self.context.parse_file_with_contents(uri, text);
        let parsed = eval_result.ast.is_some();
        if let Some(ast) = eval_result.ast {
            let module = Arc::new(LspModule::new(ast));
            let mut last_valid_parse = self.last_valid_parse.write().unwrap();
            last_valid_parse.insert(uri.clone(), module);
        }
        (eval_result.diagnostics, parsed)
    }

    /// Forget `uri`'s parse. Upstream's `did_close`, minus the notification.
    pub fn close(&self, uri: &LspUri) {
        let mut last_valid_parse = self.last_valid_parse.write().unwrap();
        last_valid_parse.remove(uri);
    }

    /// How many documents hold a parse (heph's tests check closing frees it).
    pub fn parsed_documents(&self) -> usize {
        self.last_valid_parse.read().unwrap().len()
    }

    pub(crate) fn resolve_load_path(
        &self,
        path: &str,
        current_uri: &LspUri,
        workspace_root: Option<&Path>,
    ) -> Result<LspUri, LspOpError> {
        match current_uri {
            LspUri::File(_) => self
                .context
                .resolve_load(path, current_uri, workspace_root)
                .map_err(LspOpError::FromContext),
            LspUri::Starlark(_) | LspUri::Other(_) => Err(LspOpError::Other(format!(
                "URI `{}` was expected to be of type file",
                current_uri
            ))),
        }
    }

    /// Simple helper to generate `Some(LocationLink)` objects in `resolve_definition_location`
    fn location_link<R: Into<Range> + Copy>(
        source: ResolvedSpan,
        uri: &LspUri,
        target_range: R,
    ) -> Result<Option<LocationLink>, LspOpError> {
        Ok(Some(LocationLink {
            origin_selection_range: Some(source.into()),
            target_uri: uri.try_into()?,
            target_range: target_range.into(),
            target_selection_range: target_range.into(),
        }))
    }

    /// Find the ultimate places that an identifier is defined.
    ///
    /// Takes a definition location and if necessary loads other files trying
    /// to find where the symbol was defined in a useful way. e.g. pointing to the
    /// symbol in a "load()" statement isn't useful, but going to the file it is
    /// loaded from and pointing at a function definition very much is.
    fn resolve_definition_location(
        &self,
        definition: IdentifierDefinition,
        source: ResolvedSpan,
        member: Option<&str>,
        uri: &LspUri,
        workspace_root: Option<&Path>,
    ) -> Result<Option<LocationLink>, LspOpError> {
        let ret = match definition {
            IdentifierDefinition::Location {
                destination: target,
                ..
            } => Self::location_link(source, uri, target)?,
            IdentifierDefinition::LoadedLocation {
                destination: location,
                path,
                name,
                ..
            } => {
                let load_uri = self.resolve_load_path(&path, uri, workspace_root)?;
                let loaded_location =
                    self.get_ast_or_load_from_disk(&load_uri)?
                        .and_then(|ast| match member {
                            Some(member) => ast.find_exported_symbol_and_member(&name, member),
                            None => ast.find_exported_symbol_span(&name),
                        });
                match loaded_location {
                    None => Self::location_link(source, uri, location)?,
                    Some(loaded_location) => {
                        Self::location_link(source, &load_uri, loaded_location)?
                    }
                }
            }
            IdentifierDefinition::NotFound => None,
            IdentifierDefinition::LoadPath { path, .. } => {
                match self.resolve_load_path(&path, uri, workspace_root) {
                    Ok(load_uri) => Self::location_link(source, &load_uri, Range::default())?,
                    Err(_) => None,
                }
            }
            IdentifierDefinition::StringLiteral { literal, .. } => {
                let Ok(resolved_literal) =
                    self.context
                        .resolve_string_literal(&literal, uri, workspace_root)
                else {
                    return Ok(None);
                };
                match resolved_literal {
                    Some(StringLiteralResult {
                        uri,
                        location_finder: Some(location_finder),
                    }) => {
                        // If there's an error loading the file to parse it, at least
                        // try to get to the file.
                        let result =
                            self.get_ast_or_load_from_disk(&uri)
                                .and_then(|ast| match ast {
                                    Some(module) => location_finder(&module.ast)
                                        .map(|span| {
                                            span.map(|span| module.ast.codemap().resolve_span(span))
                                        })
                                        .map_err(LspOpError::FromContext),
                                    None => Ok(None),
                                });
                        #[allow(clippy::manual_unwrap_or_default)]
                        let result = match result {
                            Ok(result) => result,
                            Err(_e) => {
                                // FIXME(JakobDegen): wtf is happening here?
                                // eprintln!("Error jumping to definition: {e:#}");
                                None
                            }
                        };
                        let target_range = result.unwrap_or_default();
                        Self::location_link(source, &uri, target_range)?
                    }
                    Some(StringLiteralResult {
                        uri,
                        location_finder: None,
                    }) => Self::location_link(source, &uri, Range::default())?,
                    _ => None,
                }
            }
            IdentifierDefinition::Unresolved { name, .. } => {
                match self
                    .context
                    .get_uri_for_global_symbol(uri, &name)
                    .map_err(LspOpError::FromContext)?
                {
                    Some(uri) => {
                        let loaded_location =
                            self.get_ast_or_load_from_disk(&uri)?
                                .and_then(|ast| match member {
                                    Some(member) => {
                                        ast.find_exported_symbol_and_member(&name, member)
                                    }
                                    None => ast.find_exported_symbol_span(&name),
                                });

                        Self::location_link(source, &uri, loaded_location.unwrap_or_default())?
                    }
                    None => None,
                }
            }
        };
        Ok(ret)
    }

    /// Go to the definition of the symbol at the current cursor.
    pub fn find_definition(
        &self,
        params: GotoDefinitionParams,
        initialize_params: &InitializeParams,
    ) -> Result<GotoDefinitionResponse, LspOpError> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .try_into()?;
        let line = params.text_document_position_params.position.line;
        let character = params.text_document_position_params.position.character;
        let workspace_root =
            Self::get_workspace_root(initialize_params.workspace_folders.as_ref(), &uri);

        let location = match self.get_ast(&uri) {
            Some(ast) => {
                let location = ast.find_definition_at_location(line, character);
                let source = location.source().unwrap_or_default();
                match location {
                    Definition::Identifier(definition) => self.resolve_definition_location(
                        definition,
                        source,
                        None,
                        &uri,
                        workspace_root.as_deref(),
                    )?,
                    // In this case we don't pass the name along in the root_definition_location,
                    // so it's simpler to do the lookup here, rather than threading a ton of
                    // information through.
                    Definition::Dotted(DottedDefinition {
                        root_definition_location: IdentifierDefinition::Location { destination, .. },
                        segments,
                        ..
                    }) => {
                        let member_location = ast
                            .find_exported_symbol_and_member(
                                segments.first().expect("at least one segment").as_str(),
                                segments.get(1).expect("at least two segments").as_str(),
                            )
                            .unwrap_or(destination);
                        Self::location_link(source, &uri, member_location)?
                    }
                    Definition::Dotted(definition) => self.resolve_definition_location(
                        definition.root_definition_location,
                        source,
                        Some(
                            definition
                                .segments
                                .last()
                                .expect("to have at least one component")
                                .as_str(),
                        ),
                        &uri,
                        workspace_root.as_deref(),
                    )?,
                }
            }
            None => None,
        };

        let response = match location {
            Some(location) => vec![location],
            None => vec![],
        };
        Ok(GotoDefinitionResponse::Link(response))
    }

    /// Completion items for the current cursor.
    pub fn completion_options(
        &self,
        params: CompletionParams,
        initialize_params: &InitializeParams,
    ) -> Result<CompletionResponse, LspOpError> {
        let uri = params.text_document_position.text_document.uri.try_into()?;
        let line = params.text_document_position.position.line;
        let character = params.text_document_position.position.character;

        let symbols: Option<Vec<_>> = match self.get_ast(&uri) {
            Some(document) => {
                // Figure out what kind of position we are in, to determine the best type of
                // autocomplete.
                let autocomplete_type = document.ast.get_auto_complete_type(line, character);
                let workspace_root =
                    Self::get_workspace_root(initialize_params.workspace_folders.as_ref(), &uri);

                match &autocomplete_type {
                    None | Some(AutocompleteType::None) => None,
                    Some(AutocompleteType::Default) => Some(
                        self.default_completion_options(
                            &uri,
                            &document,
                            line,
                            character,
                            workspace_root.as_deref(),
                        )
                        .collect(),
                    ),
                    Some(AutocompleteType::LoadPath {
                        current_value,
                        current_span,
                    })
                    | Some(AutocompleteType::String {
                        current_value,
                        current_span,
                    }) => Some(self.string_completion_options(
                        &uri,
                        if matches!(&autocomplete_type, Some(AutocompleteType::LoadPath { .. })) {
                            StringCompletionType::LoadPath
                        } else {
                            StringCompletionType::String
                        },
                        current_value,
                        *current_span,
                        workspace_root.as_deref(),
                    )?),
                    Some(AutocompleteType::LoadSymbol {
                        path,
                        current_span,
                        previously_loaded,
                    }) => Some(self.exported_symbol_options(
                        path,
                        *current_span,
                        previously_loaded,
                        &uri,
                        workspace_root.as_deref(),
                    )),
                    Some(AutocompleteType::Parameter {
                        function_name_span,
                        previously_used_named_parameters,
                        ..
                    }) => Some(
                        self.parameter_name_options(
                            function_name_span,
                            &document,
                            &uri,
                            previously_used_named_parameters,
                            workspace_root.as_deref(),
                        )
                        .chain(self.default_completion_options(
                            &uri,
                            &document,
                            line,
                            character,
                            workspace_root.as_deref(),
                        ))
                        .collect(),
                    ),
                    Some(AutocompleteType::Type) => Some(Self::type_completion_options().collect()),
                }
            }
            None => None,
        };

        Ok(CompletionResponse::Array(symbols.unwrap_or_default()))
    }

    /// Using all currently loaded documents, gather a list of known exported
    /// symbols. This list contains both the symbols exported from the loaded
    /// files, as well as symbols loaded in the open files. Symbols that are
    /// loaded from modules that are open are deduplicated.
    pub(crate) fn get_all_exported_symbols<F, S>(
        &self,
        except_from: Option<&LspUri>,
        symbols: &HashMap<String, S>,
        workspace_root: Option<&Path>,
        current_document: &LspUri,
        format_text_edit: F,
    ) -> Vec<CompletionItem>
    where
        F: Fn(&str, &str) -> TextEdit,
    {
        let mut seen = HashSet::new();
        let mut result = Vec::new();

        let all_documents = self.last_valid_parse.read().unwrap();

        for (doc_uri, doc) in all_documents
            .iter()
            .filter(|&(doc_uri, _)| match except_from {
                Some(uri) => doc_uri != uri,
                None => true,
            })
        {
            let Ok(load_path) =
                self.context
                    .render_as_load(doc_uri, current_document, workspace_root)
            else {
                continue;
            };

            for symbol in doc
                .get_exported_symbols()
                .into_iter()
                .filter(|symbol| !symbols.contains_key(&symbol.name))
            {
                seen.insert(format!("{load_path}:{}", symbol.name));

                let text_edits = Some(vec![format_text_edit(&load_path, &symbol.name)]);
                let mut completion_item: CompletionItem = symbol.into();
                completion_item.detail = Some(format!("Load from {load_path}"));
                completion_item.additional_text_edits = text_edits;

                result.push(completion_item)
            }
        }

        for (doc_uri, symbol) in all_documents
            .iter()
            .filter(|&(doc_uri, _)| match except_from {
                Some(uri) => doc_uri != uri,
                None => true,
            })
            .flat_map(|(doc_uri, doc)| {
                doc.get_loaded_symbols()
                    .into_iter()
                    .map(move |symbol| (doc_uri, symbol))
            })
            .filter(|(_, symbol)| !symbols.contains_key(symbol.name))
        {
            let Ok(uri) = self
                .context
                .resolve_load(symbol.loaded_from, doc_uri, workspace_root)
            else {
                continue;
            };
            let Ok(load_path) = self
                .context
                .render_as_load(&uri, current_document, workspace_root)
            else {
                continue;
            };

            if seen.insert(format!("{}:{}", load_path, symbol.name)) {
                result.push(CompletionItem {
                    label: symbol.name.to_owned(),
                    detail: Some(format!("Load from {}", load_path)),
                    kind: Some(CompletionItemKind::CONSTANT),
                    additional_text_edits: Some(vec![format_text_edit(&load_path, symbol.name)]),
                    ..Default::default()
                })
            }
        }

        result
    }

    pub(crate) fn get_global_symbol_completion_items(
        &self,
        current_document: &LspUri,
    ) -> impl Iterator<Item = CompletionItem> + '_ + use<'_, T> {
        self.context
            .get_environment(current_document)
            .members
            .into_iter()
            .map(|(symbol, documentation)| CompletionItem {
                label: symbol.clone(),
                kind: Some(match &documentation {
                    DocItem::Member(DocMember::Function { .. }) => CompletionItemKind::FUNCTION,
                    _ => CompletionItemKind::CONSTANT,
                }),
                detail: documentation.get_doc_summary().map(|str| str.to_owned()),
                documentation: Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: render_doc_item_no_link(&symbol, &documentation),
                })),
                ..Default::default()
            })
    }

    pub(crate) fn get_load_text_edit<P>(
        module: &str,
        symbol: &str,
        ast: &LspModule,
        last_load: Option<ResolvedSpan>,
        existing_load: Option<&(Vec<LoadArgP<P>>, Span)>,
    ) -> TextEdit
    where
        P: AstPayload,
    {
        match existing_load {
            Some((previously_loaded_symbols, load_span)) => {
                // We're already loading a symbol from this module path, construct
                // a text edit that amends the existing load.
                let load_span = ast.ast.codemap().resolve_span(*load_span);
                let mut load_args: Vec<(&str, &str)> = previously_loaded_symbols
                    .iter()
                    .map(|LoadArgP { local, their, .. }| {
                        (local.ident.as_str(), their.node.as_str())
                    })
                    .collect();
                load_args.push((symbol, symbol));
                load_args.sort_by_key(|(_, a)| *a);

                TextEdit::new(
                    load_span.into(),
                    format!(
                        "load(\"{module}\", {})",
                        load_args
                            .into_iter()
                            .map(|(assign, import)| {
                                if assign == import {
                                    format!("\"{import}\"")
                                } else {
                                    format!("{assign} = \"{import}\"")
                                }
                            })
                            .join(", ")
                    ),
                )
            }
            None => {
                // We're not yet loading from this module, construct a text edit that
                // inserts a new load statement after the last one we found.
                TextEdit::new(
                    match last_load {
                        Some(span) => Range::new(
                            Position::new(span.end.line as u32, span.end.column as u32),
                            Position::new(span.end.line as u32, span.end.column as u32),
                        ),
                        None => Range::new(Position::new(0, 0), Position::new(0, 0)),
                    },
                    format!(
                        "{}load(\"{module}\", \"{symbol}\"){}",
                        if last_load.is_some() { "\n" } else { "" },
                        if last_load.is_some() { "" } else { "\n\n" },
                    ),
                )
            }
        }
    }

    /// Get completion items for each language keyword.
    pub(crate) fn get_keyword_completion_items() -> impl Iterator<Item = CompletionItem> {
        [
            // Actual keywords
            "and", "else", "load", "break", "for", "not", "continue", "if", "or", "def", "in",
            "pass", "elif", "return", "lambda",
        ]
        .into_iter()
        .map(|keyword| CompletionItem {
            label: keyword.to_owned(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        })
    }

    /// Get hover information for a given position in a document.
    pub fn hover_info(
        &self,
        params: HoverParams,
        initialize_params: &InitializeParams,
    ) -> Result<Hover, LspOpError> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .try_into()?;
        let line = params.text_document_position_params.position.line;
        let character = params.text_document_position_params.position.character;
        let workspace_root =
            Self::get_workspace_root(initialize_params.workspace_folders.as_ref(), &uri);

        // Return an empty result as a "not found"
        let not_found = Hover {
            contents: HoverContents::Array(vec![]),
            range: None,
        };

        Ok(match self.get_ast(&uri) {
            Some(document) => {
                let location = document.find_definition_at_location(line, character);
                match location {
                    Definition::Identifier(identifier_definition) => self
                        .get_hover_for_identifier_definition(
                            identifier_definition,
                            &document,
                            &uri,
                            workspace_root.as_deref(),
                        )?,
                    Definition::Dotted(DottedDefinition {
                        root_definition_location,
                        ..
                    }) => {
                        // Not something we really support yet, so just provide hover information for
                        // the root definition.
                        self.get_hover_for_identifier_definition(
                            root_definition_location,
                            &document,
                            &uri,
                            workspace_root.as_deref(),
                        )?
                    }
                }
                .unwrap_or(not_found)
            }
            None => not_found,
        })
    }

    fn get_hover_for_identifier_definition(
        &self,
        identifier_definition: IdentifierDefinition,
        document: &LspModule,
        document_uri: &LspUri,
        workspace_root: Option<&Path>,
    ) -> Result<Option<Hover>, LspOpError> {
        Ok(match identifier_definition {
            IdentifierDefinition::Location {
                destination,
                name,
                source,
            } => {
                // TODO: This seems very inefficient. Once the document starts
                // holding the `Scope` including AST nodes, this indirection
                // should be removed.
                find_symbols_at_location(
                    document.ast.codemap(),
                    document.ast.statement(),
                    ResolvedPos {
                        line: destination.begin.line,
                        column: destination.begin.column,
                    },
                )
                .remove(&name)
                .and_then(|symbol| {
                    symbol
                        .doc
                        .map(|docs| Hover {
                            contents: HoverContents::Array(vec![MarkedString::String(
                                render_doc_item_no_link(&symbol.name, &docs),
                            )]),
                            range: Some(source.into()),
                        })
                        .or_else(|| {
                            symbol.param.map(|(starred_name, doc)| Hover {
                                contents: HoverContents::Array(vec![MarkedString::String(
                                    render_doc_param(starred_name, &doc),
                                )]),
                                range: Some(source.into()),
                            })
                        })
                })
            }
            IdentifierDefinition::LoadedLocation {
                path, name, source, ..
            } => {
                // Symbol loaded from another file. Find the file and get the definition
                // from there, hopefully including the docs.
                let load_uri = self.resolve_load_path(&path, document_uri, workspace_root)?;
                self.get_ast_or_load_from_disk(&load_uri)?.and_then(|ast| {
                    ast.find_exported_symbol(&name).and_then(|symbol| {
                        symbol.docs.map(|docs| Hover {
                            contents: HoverContents::Array(vec![MarkedString::String(
                                render_doc_item_no_link(&symbol.name, &docs),
                            )]),
                            range: Some(source.into()),
                        })
                    })
                })
            }
            IdentifierDefinition::StringLiteral { source, literal } => {
                let Ok(resolved_literal) =
                    self.context
                        .resolve_string_literal(&literal, document_uri, workspace_root)
                else {
                    // We might just be hovering a string that's not a file/target/etc,
                    // so just return nothing.
                    return Ok(None);
                };
                match resolved_literal {
                    Some(StringLiteralResult {
                        uri,
                        location_finder: Some(location_finder),
                    }) => {
                        // If there's an error loading the file to parse it, at least
                        // try to get to the file.
                        let module = match self.get_ast_or_load_from_disk(&uri) {
                            Ok(Some(ast)) => ast,
                            _ => {
                                return Ok(None);
                            }
                        };
                        let result =
                            location_finder(&module.ast).map_err(LspOpError::FromContext)?;

                        result.map(|location| Hover {
                            contents: HoverContents::Array(vec![MarkedString::LanguageString(
                                LanguageString {
                                    language: "python".to_owned(),
                                    value: module.ast.codemap().source_span(location).to_owned(),
                                },
                            )]),
                            range: Some(source.into()),
                        })
                    }
                    _ => None,
                }
            }
            IdentifierDefinition::Unresolved { source, name } => {
                // Try to resolve as a global symbol.
                self.context
                    .get_environment(document_uri)
                    .members
                    .into_iter()
                    .find(|symbol| symbol.0 == name)
                    .map(|symbol| Hover {
                        contents: HoverContents::Array(vec![MarkedString::String(
                            render_doc_item_no_link(&symbol.0, &symbol.1),
                        )]),
                        range: Some(source.into()),
                    })
            }
            IdentifierDefinition::LoadPath { .. } | IdentifierDefinition::NotFound => None,
        })
    }

    fn get_workspace_root(
        workspace_roots: Option<&Vec<WorkspaceFolder>>,
        target: &LspUri,
    ) -> Option<PathBuf> {
        match target {
            LspUri::File(target) => workspace_roots.and_then(|roots| {
                roots
                    .iter()
                    .filter_map(|root| uri_to_file_path(&root.uri))
                    .find(|root| target.starts_with(root))
            }),
            _ => None,
        }
    }
}
