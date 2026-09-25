use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{Range, Url};
use qbx_fivem_data::Side;
use qbx_lua_analysis::manifest::Manifest;
use qbx_lua_analysis::summary::FileSummary;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

use crate::types::{FunType, Type};

pub type FileId = u32;
pub type ResourceId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Variable,
    Table,
    Field,
    Class,
    Alias,
    Export,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: SmolStr,
    pub kind: SymbolKind,
    pub ty: Type,
    pub doc: Option<Arc<str>>,
    pub deprecated: bool,
    /// Source text of a short literal value, shown in hovers as `name: type = value`.
    pub literal: Option<SmolStr>,
    pub range: Range,
}

#[derive(Clone, Debug)]
pub struct Member {
    pub owner: SmolStr,
    pub symbol: Symbol,
}

#[derive(Clone, Debug)]
pub struct ClassDef {
    pub name: SmolStr,
    pub parents: Vec<SmolStr>,
    pub fields: Vec<Symbol>,
    pub index: Option<(Type, Type)>,
    pub call: Option<Arc<FunType>>,
    pub doc: Option<Arc<str>>,
    pub range: Range,
}

#[derive(Clone, Debug)]
pub struct AliasDef {
    pub name: SmolStr,
    pub ty: Type,
    pub doc: Option<Arc<str>>,
    pub range: Range,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    NetEvent,
    Handler,
    Callback,
    Trigger,
}

#[derive(Clone, Debug)]
pub struct EventDef {
    pub name: SmolStr,
    pub kind: EventKind,
    /// The manifest side narrowed by the guard around this registration or trigger.
    pub side: Option<Side>,
    pub handler: Option<Arc<FunType>>,
    pub range: Range,
}

#[derive(Clone, Debug, Default)]
pub struct FileIndex {
    pub globals: Vec<Symbol>,
    pub members: Vec<Member>,
    pub classes: Vec<ClassDef>,
    pub aliases: Vec<AliasDef>,
    pub exports: Vec<Symbol>,
    pub events: Vec<EventDef>,
    pub module_return: Option<Type>,
    pub convars: Vec<SmolStr>,
    pub state_keys: Vec<SmolStr>,
    /// The file registers exports under names computed at runtime, so the listed ones are not all.
    pub dynamic_exports: bool,
    pub summary: FileSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOrigin {
    Stub,
    Workspace,
    Library,
}

#[derive(Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub uri: Url,
    pub origin: FileOrigin,
    pub resource: Option<ResourceId>,
    pub side: Option<Side>,
    pub index: FileIndex,
}

#[derive(Debug)]
pub struct ResourceEntry {
    pub name: SmolStr,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    pub files: Vec<FileId>,
    /// Files pulled in through `@resource/file.lua` manifest entries, with the side they load on.
    pub imports: Vec<(FileId, Side)>,
    /// Ships a `.fxap` marker or an encrypted file, so part of its code cannot be read.
    pub escrowed: bool,
}

type Slot = (FileId, u32);

#[derive(Debug, Default)]
pub struct Index {
    files: Vec<Option<FileEntry>>,
    by_path: FxHashMap<PathBuf, FileId>,
    pub resources: Vec<ResourceEntry>,
    globals: FxHashMap<SmolStr, Vec<Slot>>,
    members: FxHashMap<SmolStr, Vec<Slot>>,
    classes: FxHashMap<SmolStr, Vec<Slot>>,
    aliases: FxHashMap<SmolStr, Vec<Slot>>,
}

pub fn normalize_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().replace('/', "\\").to_lowercase())
    } else {
        path.to_path_buf()
    }
}

fn remove_file_slots<'a>(
    map: &mut FxHashMap<SmolStr, Vec<Slot>>,
    keys: impl Iterator<Item = &'a SmolStr>,
    file: FileId,
) {
    for key in keys {
        if let Some(slots) = map.get_mut(key) {
            slots.retain(|(f, _)| *f != file);
            if slots.is_empty() {
                map.remove(key);
            }
        }
    }
}

impl Index {
    /// Start a fresh disk scan without discarding the built-in library.
    pub fn clear_workspace(&mut self) {
        let stubs: Vec<FileEntry> =
            self.files.iter_mut().filter_map(Option::take).filter(|file| file.origin == FileOrigin::Stub).collect();
        *self = Self::default();
        for entry in stubs {
            let id = self.allocate(&entry.path);
            self.set_file(id, entry);
        }
    }

    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.by_path.get(&normalize_path(path)).copied()
    }

    pub fn file(&self, id: FileId) -> Option<&FileEntry> {
        self.files.get(id as usize).and_then(Option::as_ref)
    }

    pub fn files(&self) -> impl Iterator<Item = (FileId, &FileEntry)> {
        self.files.iter().enumerate().filter_map(|(id, f)| f.as_ref().map(|f| (id as FileId, f)))
    }

    pub fn file_count(&self) -> usize {
        self.files.iter().flatten().count()
    }

    pub fn resource(&self, id: ResourceId) -> Option<&ResourceEntry> {
        self.resources.get(id as usize)
    }

    pub fn resource_by_name(&self, name: &str) -> Option<(ResourceId, &ResourceEntry)> {
        self.resources
            .iter()
            .enumerate()
            .find(|(_, r)| r.name.eq_ignore_ascii_case(name))
            .map(|(id, r)| (id as ResourceId, r))
    }

    pub fn resource_of(&self, file: FileId) -> Option<&ResourceEntry> {
        self.file(file)?.resource.and_then(|id| self.resource(id))
    }

    /// Reserves an id for `path`, reusing the existing one when the file is already known.
    pub fn allocate(&mut self, path: &Path) -> FileId {
        let key = normalize_path(path);
        if let Some(id) = self.by_path.get(&key) {
            return *id;
        }
        let id = self.files.len() as FileId;
        self.files.push(None);
        self.by_path.insert(key, id);
        id
    }

    pub fn set_file(&mut self, id: FileId, entry: FileEntry) {
        self.clear_slots(id);
        for (i, symbol) in entry.index.globals.iter().enumerate() {
            self.globals.entry(symbol.name.clone()).or_default().push((id, i as u32));
        }
        for (i, member) in entry.index.members.iter().enumerate() {
            self.members.entry(member.owner.clone()).or_default().push((id, i as u32));
        }
        for (i, class) in entry.index.classes.iter().enumerate() {
            self.classes.entry(class.name.clone()).or_default().push((id, i as u32));
        }
        for (i, alias) in entry.index.aliases.iter().enumerate() {
            self.aliases.entry(alias.name.clone()).or_default().push((id, i as u32));
        }
        if let Some(resource) = entry.resource.and_then(|r| self.resources.get_mut(r as usize)) {
            if !resource.files.contains(&id) {
                resource.files.push(id);
            }
        }
        self.files[id as usize] = Some(entry);
    }

    fn clear_slots(&mut self, id: FileId) {
        let Some(old) = self.files.get_mut(id as usize).and_then(Option::take) else { return };
        remove_file_slots(&mut self.globals, old.index.globals.iter().map(|s| &s.name), id);
        remove_file_slots(&mut self.members, old.index.members.iter().map(|m| &m.owner), id);
        remove_file_slots(&mut self.classes, old.index.classes.iter().map(|c| &c.name), id);
        remove_file_slots(&mut self.aliases, old.index.aliases.iter().map(|a| &a.name), id);
    }

    pub fn remove_file(&mut self, path: &Path) {
        let Some(id) = self.by_path.remove(&normalize_path(path)) else { return };
        self.clear_slots(id);
        for resource in &mut self.resources {
            resource.files.retain(|f| *f != id);
            resource.imports.retain(|(f, _)| *f != id);
        }
        self.files[id as usize] = None;
    }

    /// Whether symbols of `target` are in scope for code in `from`.
    pub fn is_visible(&self, from: FileId, target: FileId) -> bool {
        if from == target {
            return true;
        }
        let (Some(source), Some(other)) = (self.file(from), self.file(target)) else { return false };
        let sides_match = match (source.side, other.side) {
            (Some(a), Some(b)) => b.is_available_on(a),
            _ => true,
        };
        if !sides_match {
            return false;
        }
        if other.origin == FileOrigin::Stub {
            return true;
        }
        match (source.resource, other.resource) {
            (Some(a), Some(b)) if a == b => true,
            (Some(a), _) => self.resource(a).is_some_and(|r| {
                r.imports
                    .iter()
                    .any(|(file, side)| *file == target && source.side.is_none_or(|s| side.is_available_on(s)))
            }),
            (None, None) => true,
            (None, Some(_)) => false,
        }
    }

    /// Whether two files can share globals at all, regardless of the side either one runs on.
    pub fn is_related(&self, a: FileId, b: FileId) -> bool {
        let (Some(first), Some(second)) = (self.file(a), self.file(b)) else { return false };
        match (first.resource, second.resource) {
            (Some(x), Some(y)) if x == y => true,
            (Some(x), Some(y)) => {
                let imports = |from: ResourceId, file: FileId| {
                    self.resource(from).is_some_and(|r| r.imports.iter().any(|(imported, _)| *imported == file))
                };
                imports(x, b) || imports(y, a)
            }
            (None, None) => true,
            _ => false,
        }
    }

    fn visible_first<'a, T>(
        &'a self,
        slots: Option<&'a Vec<Slot>>,
        from: FileId,
        get: impl Fn(&'a FileEntry, u32) -> Option<&'a T>,
    ) -> Vec<(FileId, &'a T)> {
        // Strictly what the runtime would see: a `Config` of some other resource is a different table.
        let Some(slots) = slots else { return Vec::new() };
        let resolve = |(file, i): &Slot| Some((*file, get(self.file(*file)?, *i)?));
        slots.iter().filter(|(f, _)| self.is_visible(from, *f)).filter_map(resolve).collect()
    }

    pub fn globals_named(&self, name: &str, from: FileId) -> Vec<(FileId, &Symbol)> {
        self.visible_first(self.globals.get(name), from, |f, i| f.index.globals.get(i as usize))
    }

    /// Members are looked up per resource rather than per file: libraries such as ox_lib load the
    /// files that extend their table lazily, so importing one file makes all of them reachable.
    pub fn members_of(&self, owner: &str, from: FileId) -> Vec<(FileId, &Symbol)> {
        let Some(slots) = self.members.get(owner) else { return Vec::new() };
        let resolve = |(file, i): &Slot| Some((*file, &self.file(*file)?.index.members.get(*i as usize)?.symbol));
        let reachable = |target: FileId| {
            let sides_match = match (self.file(from).and_then(|f| f.side), self.file(target).and_then(|f| f.side)) {
                (Some(a), Some(b)) => b.is_available_on(a),
                _ => true,
            };
            sides_match
                && (self.is_visible(from, target)
                    || self.is_related(from, target)
                    || self.imports_resource_of(from, target))
        };
        // `%`-owners name one specific table of one file (a local or a module return), so whoever
        // holds a value of that type may see all of it.
        if owner.starts_with('%') {
            return slots.iter().filter_map(resolve).collect();
        }
        // A table the resource fills itself (`Config`, `Shared`) is its own; only tables that come
        // from an imported library (`lib`, `qbx`) are completed from that library's other files.
        let own_resource = self.file(from).and_then(|f| f.resource);
        let is_own = |target: FileId| {
            target == from || (own_resource.is_some() && self.file(target).and_then(|f| f.resource) == own_resource)
        };
        let in_scope: Vec<&Slot> = slots.iter().filter(|(f, _)| self.is_visible(from, *f)).collect();
        if in_scope.iter().any(|(f, _)| is_own(*f)) {
            return in_scope.into_iter().filter_map(resolve).collect();
        }
        let visible: Vec<_> = slots.iter().filter(|(f, _)| reachable(*f)).filter_map(resolve).collect();
        // Classes travel between resources through exports and events, so their members are looked
        // up everywhere; plain tables of unrelated resources are not.
        if !visible.is_empty() || !self.classes.contains_key(owner) {
            return visible;
        }
        slots.iter().filter_map(resolve).collect()
    }

    fn imports_resource_of(&self, from: FileId, target: FileId) -> bool {
        let (Some(resource), Some(target_resource)) =
            (self.resource_of(from), self.file(target).and_then(|f| f.resource))
        else {
            return false;
        };
        resource.imports.iter().any(|(file, _)| self.file(*file).and_then(|f| f.resource) == Some(target_resource))
    }

    pub fn has_members(&self, owner: &str) -> bool {
        self.members.contains_key(owner)
    }

    pub fn class(&self, name: &str) -> Option<(FileId, &ClassDef)> {
        let (file, i) = *self.classes.get(name)?.first()?;
        Some((file, self.file(file)?.index.classes.get(i as usize)?))
    }

    pub fn class_defs(&self, name: &str) -> Vec<(FileId, &ClassDef)> {
        let Some(slots) = self.classes.get(name) else { return Vec::new() };
        slots.iter().filter_map(|(f, i)| Some((*f, self.file(*f)?.index.classes.get(*i as usize)?))).collect()
    }

    pub fn alias(&self, name: &str) -> Option<(FileId, &AliasDef)> {
        let (file, i) = *self.aliases.get(name)?.first()?;
        Some((file, self.file(file)?.index.aliases.get(i as usize)?))
    }

    pub fn class_names(&self) -> impl Iterator<Item = &SmolStr> {
        self.classes.keys().chain(self.aliases.keys())
    }

    /// Every global visible from `from`, for completion.
    pub fn visible_globals(&self, from: FileId) -> impl Iterator<Item = (FileId, &Symbol)> {
        self.files()
            .filter(move |(id, _)| self.is_visible(from, *id))
            .flat_map(|(id, file)| file.index.globals.iter().map(move |s| (id, s)))
    }

    pub fn exports_of(&self, resource: &str) -> Vec<(FileId, &Symbol)> {
        let Some((_, entry)) = self.resource_by_name(resource) else { return Vec::new() };
        entry
            .files
            .iter()
            .filter_map(|id| Some((*id, self.file(*id)?)))
            .flat_map(|(id, file)| file.index.exports.iter().map(move |s| (id, s)))
            .collect()
    }

    pub fn events(&self) -> impl Iterator<Item = (FileId, &EventDef)> {
        self.files().flat_map(|(id, file)| file.index.events.iter().map(move |e| (id, e)))
    }

    /// Resolves a `require` argument the way ox_lib does: dotted or slashed, relative to the
    /// resource root, optionally prefixed with `@resource`.
    pub fn resolve_require(&self, module: &str, from: FileId) -> Option<FileId> {
        let (resource, module) = match module.strip_prefix('@') {
            Some(rest) => {
                let (name, path) = rest.split_once(['/', '.'])?;
                (self.resource_by_name(name)?.1, path)
            }
            None => (self.resource_of(from)?, module),
        };
        let relative = if module.contains('/') { module.to_string() } else { module.replace('.', "/") };
        let relative = relative.trim_end_matches(".lua");
        [format!("{relative}.lua"), format!("{relative}/init.lua")]
            .iter()
            .find_map(|candidate| self.file_id(&resource.root.join(candidate)))
    }
}
