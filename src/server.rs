use std::error::Error;
use std::path::PathBuf;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{self as notif, Notification as _};
use lsp_types::request::{self as req, Request as _};
use lsp_types::*;
use qbx_lua_analysis::project::is_manifest_file;
use qbx_lua_analysis::Level;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::document::Document;
use crate::features::{
    code_action, completion, definition, diagnostics, folding, hover, inlay, references, semantic_tokens, signature,
    symbols,
};
use crate::index::FileOrigin;
use crate::workspace::{uri_to_path, Workspace};

pub type Documents = FxHashMap<Url, Document>;

/// Opening the file shows everything; the workspace overview only needs to say "look here".
const MAX_PROBLEMS_PER_CLOSED_FILE: usize = 100;

type AnyResult<T> = Result<T, Box<dyn Error + Sync + Send>>;

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct DiagnosticSettings {
    pub enable: Option<bool>,
    pub workspace: Option<bool>,
    pub rules: FxHashMap<String, String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct ToggleSettings {
    pub enable: Option<bool>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub library: Vec<String>,
    pub diagnostics: DiagnosticSettings,
    pub inlay_hints: ToggleSettings,
    pub semantic_tokens: ToggleSettings,
}

impl Settings {
    fn rule_overrides(&self) -> Vec<(String, Level)> {
        self.diagnostics
            .rules
            .iter()
            .filter(|(code, _)| qbx_lua_analysis::rules::find(code).is_some())
            .filter_map(|(code, level)| {
                let level = match level.as_str() {
                    "off" => Level::Off,
                    "hint" => Level::Hint,
                    "info" => Level::Info,
                    "warning" | "warn" => Level::Warning,
                    "error" => Level::Error,
                    _ => return None,
                };
                Some((code.clone(), level))
            })
            .collect()
    }
}

pub struct Server {
    connection: Connection,
    ws: Workspace,
    docs: Documents,
    settings: Settings,
    snippet_support: bool,
    watched_files_registration: bool,
    dirty: FxHashSet<Url>,
    diagnostics_pending: bool,
    /// Closed files that currently have diagnostics published, so they can be cleared again.
    workspace_reported: FxHashSet<Url>,
    workspace_stale: bool,
    /// Resources to re-lint after a save or close, which is much cheaper than the whole workspace.
    stale_resources: FxHashSet<Option<crate::index::ResourceId>>,
    next_request_id: i32,
}

pub fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::INCREMENTAL),
            save: Some(TextDocumentSyncSaveOptions::Supported(true)),
            ..TextDocumentSyncOptions::default()
        })),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(true),
            trigger_characters: Some(vec![".".into(), ":".into(), "'".into(), "\"".into(), "@".into(), "{".into()]),
            ..CompletionOptions::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".into(), ",".into()]),
            retrigger_characters: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(false),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            },
        )),
        ..ServerCapabilities::default()
    }
}

pub fn run() -> AnyResult<()> {
    let (connection, io_threads) = Connection::stdio();
    run_connection(connection)?;
    io_threads.join()?;
    Ok(())
}

pub fn run_connection(connection: Connection) -> AnyResult<()> {
    let (id, params) = connection.initialize_start()?;
    let params: InitializeParams = serde_json::from_value(params)?;
    let result = json!({
        "capabilities": capabilities(),
        "serverInfo": { "name": "qbx-lua-ls", "version": env!("CARGO_PKG_VERSION") },
    });
    connection.initialize_finish(id, result)?;

    let mut server = Server::new(connection, params);
    server.start();
    server.main_loop()
}

#[allow(deprecated)]
fn workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let folders = params.workspace_folders.iter().flatten().filter_map(|f| uri_to_path(&f.uri));
    let mut roots: Vec<PathBuf> = folders.collect();
    if roots.is_empty() {
        roots.extend(params.root_uri.as_ref().and_then(uri_to_path));
    }
    roots
}

impl Server {
    pub fn new(connection: Connection, params: InitializeParams) -> Self {
        let settings: Settings =
            params.initialization_options.clone().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let mut ws = Workspace::default();
        ws.roots = workspace_roots(&params);
        ws.library = settings.library.iter().map(PathBuf::from).collect();
        let snippet_support = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|text| text.completion.as_ref())
            .and_then(|completion| completion.completion_item.as_ref())
            .and_then(|item| item.snippet_support)
            .unwrap_or(false);
        let watched_files_registration = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|watched| watched.dynamic_registration)
            .unwrap_or(false);
        Self {
            connection,
            ws,
            docs: Documents::default(),
            settings,
            snippet_support,
            watched_files_registration,
            dirty: FxHashSet::default(),
            diagnostics_pending: false,
            workspace_reported: FxHashSet::default(),
            workspace_stale: true,
            stale_resources: FxHashSet::default(),
            next_request_id: 0,
        }
    }

    fn start(&mut self) {
        self.ws.load_stubs();
        let stats = self.ws.scan();
        self.log(format!(
            "indexed {} files in {} resources in {} ms ({} natives available)",
            stats.files,
            stats.resources,
            stats.millis,
            qbx_fivem_data::native_count()
        ));
        self.register_watchers();
    }

    fn log(&self, message: String) {
        self.notify::<notif::LogMessage>(LogMessageParams { typ: MessageType::INFO, message });
    }

    fn notify<N: notif::Notification>(&self, params: N::Params) {
        let _ = self.connection.sender.send(Message::Notification(Notification::new(N::METHOD.to_string(), params)));
    }

    fn register_watchers(&mut self) {
        if !self.watched_files_registration {
            return;
        }
        let watchers = ["**/*.lua", "**/qbxlint.toml", "**/.qbxlint.toml", "**/locales/*.json", "**/*.cfg"]
            .iter()
            .map(|glob| FileSystemWatcher { glob_pattern: GlobPattern::String(glob.to_string()), kind: None })
            .collect();
        let registration = Registration {
            id: "qbx-watch-lua".into(),
            method: notif::DidChangeWatchedFiles::METHOD.into(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers }).ok(),
        };
        self.next_request_id += 1;
        let request = Request::new(
            RequestId::from(self.next_request_id),
            req::RegisterCapability::METHOD.to_string(),
            RegistrationParams { registrations: vec![registration] },
        );
        let _ = self.connection.sender.send(Message::Request(request));
    }

    fn main_loop(&mut self) -> AnyResult<()> {
        while let Ok(message) = self.connection.receiver.recv() {
            match message {
                Message::Request(request) => {
                    if self.connection.handle_shutdown(&request)? {
                        return Ok(());
                    }
                    let id = request.id.clone();
                    let response = self.isolated(|server| {
                        server.flush_index();
                        server.handle_request(request)
                    });
                    let response = response.unwrap_or_else(|| {
                        Response::new_err(id, ErrorCode::InternalError as i32, "internal error".to_string())
                    });
                    self.connection.sender.send(Message::Response(response))?;
                }
                Message::Notification(notification) => {
                    self.isolated(|server| server.handle_notification(notification));
                }
                Message::Response(_) => {}
            }
            if self.connection.receiver.is_empty() {
                self.isolated(Self::publish_dirty);
                self.isolated(Self::publish_workspace);
            }
        }
        Ok(())
    }

    /// A bug in one request must not take the editor's language server down with it.
    fn isolated<T>(&mut self, work: impl FnOnce(&mut Self) -> T) -> Option<T> {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(self)));
        if outcome.is_err() {
            self.dirty.clear();
            self.diagnostics_pending = false;
            self.log("recovered from an internal error; please report it with the file that triggered it".to_string());
        }
        outcome.ok()
    }

    /// Open documents are reparsed on every edit but only reindexed once something needs the index.
    fn flush_index(&mut self) {
        self.diagnostics_pending |= !self.dirty.is_empty();
        for uri in std::mem::take(&mut self.dirty) {
            if let Some(doc) = self.docs.get_mut(&uri) {
                // A full scan reallocates file IDs, including the reserved slots for manifests.
                doc.file = self.ws.index.allocate(&doc.path);
                let is_source = !qbx_lua_analysis::project::is_not_source(doc.text.as_bytes());
                if !doc.is_manifest() && is_source {
                    doc.file =
                        self.ws.index_parsed(&doc.path, FileOrigin::Workspace, &doc.text, &doc.chunk, &doc.resolution);
                }
            }
        }
    }

    fn publish_dirty(&mut self) {
        self.flush_index();
        if !std::mem::take(&mut self.diagnostics_pending) {
            return;
        }
        let crossrefs = self.ws.crossrefs();
        let uris: Vec<Url> = self.docs.keys().cloned().collect();
        for uri in uris {
            self.publish(&uri, &crossrefs);
        }
    }

    /// Lints the files nobody has open, so the Problems panel covers the whole workspace. Each file
    /// is parsed, checked and dropped again; only the diagnostics leave this function.
    fn publish_workspace(&mut self) {
        let everything = std::mem::take(&mut self.workspace_stale);
        let scope = std::mem::take(&mut self.stale_resources);
        if !everything && scope.is_empty() {
            return;
        }
        let in_scope = |resource: Option<crate::index::ResourceId>| everything || scope.contains(&resource);
        let settings = &self.settings.diagnostics;
        let enabled = settings.enable.unwrap_or(true) && settings.workspace.unwrap_or(true);
        let mut targets: Vec<(Url, PathBuf, Option<crate::index::FileId>)> = Vec::new();
        if enabled {
            let in_workspace = |path: &std::path::Path| self.ws.roots.iter().any(|root| path.starts_with(root));
            let files =
                self.ws.index.files().filter(|(_, f)| f.origin == FileOrigin::Workspace && in_scope(f.resource));
            for (id, file) in files {
                targets.push((file.uri.clone(), file.path.clone(), Some(id)));
            }
            for (id, resource) in self.ws.index.resources.iter().enumerate() {
                if in_workspace(&resource.manifest_path) && in_scope(Some(id as crate::index::ResourceId)) {
                    let uri = crate::workspace::path_to_uri(&resource.manifest_path);
                    targets.push((uri, resource.manifest_path.clone(), None));
                }
            }
        }

        let overrides = self.settings.rule_overrides();
        let checked: FxHashSet<Url> = targets.iter().map(|(uri, ..)| uri.clone()).collect();
        let mut reported: FxHashSet<Url> = if everything {
            FxHashSet::default()
        } else {
            self.workspace_reported.iter().filter(|uri| !checked.contains(*uri)).cloned().collect()
        };
        let crossrefs = self.ws.crossrefs();
        let mut locale_usage: FxHashMap<crate::index::ResourceId, Vec<qbx_lua_analysis::locale::LocaleUsage>> =
            FxHashMap::default();
        for (uri, path, file) in targets {
            let resource = file.and_then(|id| self.ws.index.file(id)).and_then(|f| f.resource);
            if let Some(open) = self.docs.get(&uri) {
                if let Some(resource) = resource {
                    locale_usage.entry(resource).or_default().push(qbx_lua_analysis::locale::locale_usage(&open.chunk));
                }
                continue;
            }
            let Ok(text) = qbx_lua_analysis::project::read_source(&path) else { continue };
            let mut doc = Document::new(uri.clone(), path, 0, text);
            doc.file = file.unwrap_or_else(|| self.ws.index.allocate(&doc.path));
            if let Some(resource) = resource {
                locale_usage.entry(resource).or_default().push(qbx_lua_analysis::locale::locale_usage(&doc.chunk));
            }
            let mut found = diagnostics::diagnostics(&self.ws, &doc, &overrides, &crossrefs);
            found.retain(|d| d.severity != Some(DiagnosticSeverity::HINT));
            found.sort_by_key(|d| d.severity.map_or(4, |s| if s == DiagnosticSeverity::ERROR { 0 } else { 1 }));
            found.truncate(MAX_PROBLEMS_PER_CLOSED_FILE);
            if !found.is_empty() {
                reported.insert(uri.clone());
            }
            if !found.is_empty() || self.workspace_reported.contains(&uri) {
                self.notify::<notif::PublishDiagnostics>(PublishDiagnosticsParams {
                    uri,
                    diagnostics: found,
                    version: None,
                });
            }
        }
        for (resource, usages) in locale_usage {
            let Some(entry) = self.ws.index.resource(resource) else { continue };
            // Encrypted scripts may use any key, so "unused" cannot be decided for such a resource.
            if entry.escrowed {
                continue;
            }
            let Some(locale) = qbx_lua_analysis::locale::LocaleFile::load(&entry.root) else { continue };
            let mut config = self.ws.lint_config.for_file(&locale.path);
            overrides.iter().for_each(|(code, level)| config.set(code, *level));
            let Some(severity) = config.severity(qbx_lua_analysis::rules::UNUSED_LOCALE_KEY) else { continue };
            let lines = qbx_lua_syntax::LineIndex::new(&locale.source);
            let found: Vec<Diagnostic> = qbx_lua_analysis::lint::unused_locale_keys_from(&locale, usages.into_iter())
                .into_iter()
                .map(|d| Diagnostic {
                    range: crate::indexer::span_to_range(&locale.source, &lines, d.span),
                    severity: Some(match severity {
                        qbx_lua_analysis::Severity::Error => DiagnosticSeverity::ERROR,
                        qbx_lua_analysis::Severity::Warning => DiagnosticSeverity::WARNING,
                        qbx_lua_analysis::Severity::Info => DiagnosticSeverity::INFORMATION,
                        qbx_lua_analysis::Severity::Hint => DiagnosticSeverity::HINT,
                    }),
                    code: Some(NumberOrString::String(d.code.to_string())),
                    source: Some(diagnostics::SOURCE.to_string()),
                    message: d.message,
                    tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                    ..Diagnostic::default()
                })
                .collect();
            let uri = crate::workspace::path_to_uri(&locale.path);
            reported.remove(&uri);
            if !found.is_empty() {
                reported.insert(uri.clone());
            }
            if !found.is_empty() || self.workspace_reported.contains(&uri) {
                self.notify::<notif::PublishDiagnostics>(PublishDiagnosticsParams {
                    uri,
                    diagnostics: found,
                    version: None,
                });
            }
        }
        for uri in self.workspace_reported.difference(&reported).filter(|uri| !self.docs.contains_key(*uri)) {
            let cleared = PublishDiagnosticsParams { uri: uri.clone(), diagnostics: Vec::new(), version: None };
            self.notify::<notif::PublishDiagnostics>(cleared);
        }
        self.workspace_reported = reported;
    }

    fn mark_resource_stale(&mut self, uri: &Url) {
        let Some(path) = uri_to_path(uri) else { return };
        let resource = match self.ws.index.file_id(&path).and_then(|id| self.ws.index.file(id)) {
            Some(file) => file.resource,
            None => self.ws.index.resources.iter().position(|r| r.manifest_path == path).map(|id| id as u32),
        };
        self.stale_resources.insert(resource);
    }

    fn publish(&self, uri: &Url, crossrefs: &qbx_lua_analysis::crossref::CrossRefs) {
        let Some(doc) = self.docs.get(uri) else { return };
        let diagnostics = if self.settings.diagnostics.enable.unwrap_or(true) {
            diagnostics::diagnostics(&self.ws, doc, &self.settings.rule_overrides(), crossrefs)
        } else {
            Vec::new()
        };
        self.notify::<notif::PublishDiagnostics>(PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics,
            version: Some(doc.version),
        });
    }

    fn handle_notification(&mut self, notification: Notification) {
        let Notification { method, params } = notification;
        match method.as_str() {
            notif::DidOpenTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(params) else { return };
                let item = params.text_document;
                let Some(path) = uri_to_path(&item.uri) else { return };
                let mut doc = Document::new(item.uri.clone(), path, item.version, item.text);
                if doc.is_manifest() {
                    self.ws.side_and_resource(&doc.path);
                    doc.file = self.ws.index.allocate(&doc.path);
                }
                self.dirty.insert(item.uri.clone());
                self.docs.insert(item.uri, doc);
            }
            notif::DidChangeTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidChangeTextDocumentParams>(params) else { return };
                let uri = params.text_document.uri;
                if let Some(doc) = self.docs.get_mut(&uri) {
                    doc.apply_changes(params.text_document.version, params.content_changes);
                    self.dirty.insert(uri);
                }
            }
            notif::DidSaveTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidSaveTextDocumentParams>(params) else { return };
                let uri = params.text_document.uri;
                if let Some(path) = uri_to_path(&uri).filter(|p| is_manifest_file(p)) {
                    self.ws.reload_manifest(&path);
                }
                self.mark_resource_stale(&uri);
                self.dirty.insert(uri);
            }
            notif::DidCloseTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidCloseTextDocumentParams>(params) else { return };
                let uri = params.text_document.uri;
                if let Some(doc) = self.docs.remove(&uri) {
                    if !doc.is_manifest() {
                        self.ws.index_path(&doc.path, FileOrigin::Workspace, None);
                    }
                }
                self.dirty.remove(&uri);
                self.mark_resource_stale(&uri);
                self.workspace_reported.insert(uri);
            }
            notif::DidChangeWatchedFiles::METHOD => {
                let Ok(params) = serde_json::from_value::<DidChangeWatchedFilesParams>(params) else { return };
                self.watched_files_changed(params.changes);
            }
            notif::DidChangeConfiguration::METHOD => {
                let Ok(params) = serde_json::from_value::<DidChangeConfigurationParams>(params) else { return };
                let section = params.settings.get("qbxLua").cloned().unwrap_or(params.settings);
                if let Ok(settings) = serde_json::from_value::<Settings>(section) {
                    self.settings = settings;
                    self.workspace_stale = true;
                    self.dirty.extend(self.docs.keys().cloned());
                }
            }
            _ => {}
        }
    }

    fn watched_files_changed(&mut self, changes: Vec<FileEvent>) {
        let mut manifests_changed = false;
        let mut manifests_deleted = false;
        for change in changes {
            let Some(path) = uri_to_path(&change.uri) else { continue };
            if path.extension().is_some_and(|e| e == "cfg") {
                qbx_lua_analysis::startup::clear_cache();
            } else if path.extension().is_some_and(|e| e == "toml") {
                if let Some(root) = self.ws.roots.first() {
                    self.ws.lint_config = qbx_lua_analysis::Config::discover(root).ok().flatten().unwrap_or_default();
                }
            } else if is_manifest_file(&path) {
                // A manifest appearing or vanishing changes which resources count as installed.
                qbx_lua_analysis::startup::clear_cache();
                manifests_changed = true;
                if change.typ == FileChangeType::DELETED {
                    manifests_deleted = true;
                } else {
                    self.ws.reload_manifest(&path);
                }
            } else if change.typ == FileChangeType::DELETED {
                self.ws.index.remove_file(&path);
            } else if path.extension().is_some_and(|e| e == "lua") && !self.docs.contains_key(&change.uri) {
                self.ws.index_path(&path, FileOrigin::Workspace, None);
            }
        }
        if manifests_changed {
            if manifests_deleted {
                self.ws.scan();
                self.dirty.extend(self.docs.keys().cloned());
                self.flush_index();
            }
            self.ws.link_imports();
        }
        self.workspace_stale = true;
        self.dirty.extend(self.docs.keys().cloned());
    }

    fn handle_request(&mut self, request: Request) -> Response {
        let id = request.id.clone();
        match self.dispatch(request) {
            Ok(value) => Response { id, result: Some(value), error: None },
            Err(message) => Response::new_err(id, ErrorCode::InvalidParams as i32, message),
        }
    }

    fn doc(&self, uri: &Url) -> Result<&Document, String> {
        self.docs.get(uri).ok_or_else(|| format!("document is not open: {uri}"))
    }

    fn dispatch(&mut self, request: Request) -> Result<Value, String> {
        fn params<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
            serde_json::from_value(value).map_err(|e| e.to_string())
        }
        fn reply<T: serde::Serialize>(value: T) -> Result<Value, String> {
            serde_json::to_value(value).map_err(|e| e.to_string())
        }

        let Request { method, params: raw, .. } = request;
        match method.as_str() {
            req::Completion::METHOD => {
                let p: CompletionParams = params(raw)?;
                let doc = self.doc(&p.text_document_position.text_document.uri)?;
                reply(completion::completion(&self.ws, doc, p.text_document_position.position, self.snippet_support))
            }
            req::ResolveCompletionItem::METHOD => reply(completion::resolve(params(raw)?)),
            req::HoverRequest::METHOD => {
                let p: HoverParams = params(raw)?;
                let doc = self.doc(&p.text_document_position_params.text_document.uri)?;
                reply(hover::hover(&self.ws, doc, p.text_document_position_params.position))
            }
            req::SignatureHelpRequest::METHOD => {
                let p: SignatureHelpParams = params(raw)?;
                let doc = self.doc(&p.text_document_position_params.text_document.uri)?;
                reply(signature::signature_help(&self.ws, doc, p.text_document_position_params.position))
            }
            req::GotoDefinition::METHOD => {
                let p: GotoDefinitionParams = params(raw)?;
                let doc = self.doc(&p.text_document_position_params.text_document.uri)?;
                reply(definition::definition(&self.ws, doc, p.text_document_position_params.position))
            }
            req::References::METHOD => {
                let p: ReferenceParams = params(raw)?;
                let doc = self.doc(&p.text_document_position.text_document.uri)?;
                reply(references::references(
                    &self.ws,
                    &self.docs,
                    doc,
                    p.text_document_position.position,
                    p.context.include_declaration,
                ))
            }
            req::DocumentHighlightRequest::METHOD => {
                let p: DocumentHighlightParams = params(raw)?;
                let doc = self.doc(&p.text_document_position_params.text_document.uri)?;
                reply(references::highlights(&self.ws, doc, p.text_document_position_params.position))
            }
            req::PrepareRenameRequest::METHOD => {
                let p: TextDocumentPositionParams = params(raw)?;
                reply(references::prepare_rename(&self.ws, self.doc(&p.text_document.uri)?, p.position))
            }
            req::Rename::METHOD => {
                let p: RenameParams = params(raw)?;
                let doc = self.doc(&p.text_document_position.text_document.uri)?;
                reply(references::rename(&self.ws, &self.docs, doc, p.text_document_position.position, &p.new_name))
            }
            req::DocumentSymbolRequest::METHOD => {
                let p: DocumentSymbolParams = params(raw)?;
                reply(DocumentSymbolResponse::Nested(symbols::document_symbols(self.doc(&p.text_document.uri)?)))
            }
            req::WorkspaceSymbolRequest::METHOD => {
                let p: WorkspaceSymbolParams = params(raw)?;
                reply(symbols::workspace_symbols(&self.ws, &p.query))
            }
            req::CodeActionRequest::METHOD => {
                let p: CodeActionParams = params(raw)?;
                reply(code_action::code_actions(self.doc(&p.text_document.uri)?, &p.context.diagnostics))
            }
            req::FoldingRangeRequest::METHOD => {
                let p: FoldingRangeParams = params(raw)?;
                reply(folding::folding_ranges(self.doc(&p.text_document.uri)?))
            }
            req::InlayHintRequest::METHOD => {
                let p: InlayHintParams = params(raw)?;
                if self.settings.inlay_hints.enable == Some(false) {
                    return reply(Vec::<InlayHint>::new());
                }
                reply(inlay::inlay_hints(&self.ws, self.doc(&p.text_document.uri)?, p.range))
            }
            req::SemanticTokensFullRequest::METHOD => {
                let p: SemanticTokensParams = params(raw)?;
                if self.settings.semantic_tokens.enable == Some(false) {
                    return reply(SemanticTokens::default());
                }
                reply(semantic_tokens::semantic_tokens(&self.ws, self.doc(&p.text_document.uri)?))
            }
            req::Formatting::METHOD => {
                let p: DocumentFormattingParams = params(raw)?;
                let doc = self.doc(&p.text_document.uri)?;
                let mut options = self.ws.lint_config.format.clone();
                // Without a qbxlint.toml the editor's own indentation settings decide.
                if self.ws.lint_config.root.as_os_str().is_empty() {
                    options.indent_width = p.options.tab_size.max(1) as usize;
                    options.use_tabs = !p.options.insert_spaces;
                }
                match qbx_lua_fmt::format(&doc.text, &options) {
                    Ok(text) if text == doc.text => reply(Vec::<TextEdit>::new()),
                    Ok(text) => {
                        let whole = Range::new(Position::new(0, 0), doc.position(doc.text.len() as u32));
                        reply(vec![TextEdit::new(whole, text)])
                    }
                    Err(error) => {
                        self.notify::<notif::ShowMessage>(ShowMessageParams {
                            typ: MessageType::WARNING,
                            message: format!("Qbox Lua could not format this file: {error}"),
                        });
                        Ok(Value::Null)
                    }
                }
            }
            "qbx/status" => Ok(json!({
                "files": self.ws.index.file_count(),
                "resources": self.ws.index.resources.len(),
                "openDocuments": self.docs.len(),
                "natives": qbx_fivem_data::native_count(),
            })),
            "qbx/reindex" => {
                qbx_lua_analysis::startup::clear_cache();
                let stats = self.ws.scan();
                self.dirty.extend(self.docs.keys().cloned());
                // Restore unsaved text before any request or closed-file diagnostic can see the
                // rebuilt index. A second pass settles types shared between open documents.
                self.flush_index();
                self.ws.link_imports();
                self.dirty.extend(self.docs.keys().cloned());
                self.flush_index();
                self.stale_resources.clear();
                self.workspace_stale = true;
                Ok(json!({ "files": stats.files, "resources": stats.resources, "millis": stats.millis as u64 }))
            }
            "qbx/fileInfo" => {
                let p: TextDocumentIdentifier = params(raw)?;
                let path = uri_to_path(&p.uri).ok_or("not a file uri")?;
                let entry = self.ws.index.file_id(&path).and_then(|id| self.ws.index.file(id));
                let resource = entry.and_then(|f| f.resource).and_then(|id| self.ws.index.resource(id));
                let side = match (entry.and_then(|f| f.side), resource, is_manifest_file(&path)) {
                    (_, _, true) => "manifest",
                    (Some(side), _, _) => side.label(),
                    (None, Some(_), _) => "module",
                    (None, None, _) => "standalone",
                };
                Ok(json!({ "side": side, "resource": resource.map(|r| r.name.to_string()) }))
            }
            "qbx/snippets" => {
                let doc =
                    serde_json::from_value::<TextDocumentIdentifier>(raw).ok().and_then(|p| self.docs.get(&p.uri));
                let snippets: Vec<Value> = completion::all_snippets(&self.ws, doc)
                    .into_iter()
                    .map(|s| {
                        let preview = completion::snippet_preview(&s.body);
                        json!({ "label": s.label, "description": s.description, "body": s.body, "preview": preview })
                    })
                    .collect();
                Ok(json!(snippets))
            }
            other => Err(format!("unsupported request: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_index_edits_once_and_idle_time_still_publishes_them() {
        let (connection, client) = Connection::memory();
        let mut server = Server::new(connection, InitializeParams::default());
        let uri = Url::from_file_path(std::env::temp_dir().join("qbx-lua-ls-dirty.lua")).unwrap();
        let text_document = TextDocumentItem::new(uri, "lua".into(), 1, "Value = 1".into());
        server.handle_notification(Notification::new(
            notif::DidOpenTextDocument::METHOD.into(),
            DidOpenTextDocumentParams { text_document },
        ));
        server.flush_index();
        assert!(server.dirty.is_empty());
        server.publish_dirty();
        let published = client
            .receiver
            .try_iter()
            .filter(|m| matches!(m, Message::Notification(n) if n.method == notif::PublishDiagnostics::METHOD))
            .count();
        assert_eq!(published, 1);
    }
}
