// Copyright 2013-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Rustdoc's HTML Rendering module
//!
//! This modules contains the bulk of the logic necessary for rendering a
//! rustdoc `clean::Crate` instance to a set of static HTML pages. This
//! rendering process is largely driven by the `format!` syntax extension to
//! perform all I/O into files and streams.
//!
//! The rendering process is largely driven by the `Context` and `Cache`
//! structures. The cache is pre-populated by crawling the crate in question,
//! and then it is shared among the various rendering threads. The cache is meant
//! to be a fairly large structure not implementing `Clone` (because it's shared
//! among threads). The context, however, should be a lightweight structure. This
//! is cloned per-thread and contains information about what is currently being
//! rendered.
//!
//! In order to speed up rendering (mostly because of markdown rendering), the
//! rendering process has been parallelized. This parallelization is only
//! exposed through the `crate` method on the context, and then also from the
//! fact that the shared cache is stored in TLS (and must be accessed as such).
//!
//! In addition to rendering the crate itself, this module is also responsible
//! for creating the corresponding search index and source file renderings.
//! These threads are not parallelized (they haven't been a bottleneck yet), and
//! both occur before the crate is rendered.
pub use self::ExternalLocation::*;

use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::default::Default;
use std::error;
use std::fmt::{self, Display, Formatter, Write as FmtWrite};
use std::fs::{self, File, OpenOptions};
use std::io::prelude::*;
use std::io::{self, BufWriter, BufReader};
use std::iter::repeat;
use std::mem;
use std::path::{PathBuf, Path, Component};
use std::str;
use std::sync::Arc;

use externalfiles::ExternalHtml;

use serialize::json::{ToJson, Json, as_json};
use syntax::{abi, ast};
use syntax::codemap::FileName;
use rustc::hir::def_id::{CrateNum, CRATE_DEF_INDEX, DefId};
use rustc::middle::privacy::AccessLevels;
use rustc::middle::stability;
use rustc::hir;
use rustc::util::nodemap::{FxHashMap, FxHashSet};
use rustc_data_structures::flock;

use clean::{self, AttributesExt, GetDefId, SelfTy, Mutability, Span};
use doctree;
use fold::DocFolder;
use html::escape::Escape;
use html::format::{ConstnessSpace};
use html::format::{TyParamBounds, WhereClause, href, AbiSpace};
use html::format::{VisSpace, Method, UnsafetySpace, MutableSpace};
use html::format::fmt_impl_for_trait_page;
use html::item_type::ItemType;
use html::markdown::{self, Markdown, MarkdownHtml, MarkdownSummaryLine, RenderType};
use html::{highlight, layout};

use html_diff;

/// A pair of name and its optional document.
pub type NameDoc = (String, Option<String>);

/// Major driving force in all rustdoc rendering. This contains information
/// about where in the tree-like hierarchy rendering is occurring and controls
/// how the current page is being rendered.
///
/// It is intended that this context is a lightweight object which can be fairly
/// easily cloned because it is cloned per work-job (about once per item in the
/// rustdoc tree).
#[derive(Clone)]
pub struct Context {
    /// Current hierarchy of components leading down to what's currently being
    /// rendered
    pub current: Vec<String>,
    /// The current destination folder of where HTML artifacts should be placed.
    /// This changes as the context descends into the module hierarchy.
    pub dst: PathBuf,
    /// A flag, which when `true`, will render pages which redirect to the
    /// real location of an item. This is used to allow external links to
    /// publicly reused items to redirect to the right location.
    pub render_redirect_pages: bool,
    pub shared: Arc<SharedContext>,
    pub render_type: RenderType,
}

pub struct SharedContext {
    /// The path to the crate root source minus the file name.
    /// Used for simplifying paths to the highlighted source code files.
    pub src_root: PathBuf,
    /// This describes the layout of each page, and is not modified after
    /// creation of the context (contains info like the favicon and added html).
    pub layout: layout::Layout,
    /// This flag indicates whether [src] links should be generated or not. If
    /// the source files are present in the html rendering, then this will be
    /// `true`.
    pub include_sources: bool,
    /// The local file sources we've emitted and their respective url-paths.
    pub local_sources: FxHashMap<PathBuf, String>,
    /// All the passes that were run on this crate.
    pub passes: FxHashSet<String>,
    /// The base-URL of the issue tracker for when an item has been tagged with
    /// an issue number.
    pub issue_tracker_base_url: Option<String>,
    /// The given user css file which allow to customize the generated
    /// documentation theme.
    pub css_file_extension: Option<PathBuf>,
    /// Warnings for the user if rendering would differ using different markdown
    /// parsers.
    pub markdown_warnings: RefCell<Vec<(Span, String, Vec<html_diff::Difference>)>>,
    /// The directories that have already been created in this doc run. Used to reduce the number
    /// of spurious `create_dir_all` calls.
    pub created_dirs: RefCell<FxHashSet<PathBuf>>,
    /// This flag indicates whether listings of modules (in the side bar and documentation itself)
    /// should be ordered alphabetically or in order of appearance (in the source code).
    pub sort_modules_alphabetically: bool,
}

impl SharedContext {
    fn ensure_dir(&self, dst: &Path) -> io::Result<()> {
        let mut dirs = self.created_dirs.borrow_mut();
        if !dirs.contains(dst) {
            fs::create_dir_all(dst)?;
            dirs.insert(dst.to_path_buf());
        }

        Ok(())
    }
}

impl SharedContext {
    /// Returns whether the `collapse-docs` pass was run on this crate.
    pub fn was_collapsed(&self) -> bool {
        self.passes.contains("collapse-docs")
    }

    /// Based on whether the `collapse-docs` pass was run, return either the `doc_value` or the
    /// `collapsed_doc_value` of the given item.
    pub fn maybe_collapsed_doc_value<'a>(&self, item: &'a clean::Item) -> Option<Cow<'a, str>> {
        if self.was_collapsed() {
            item.collapsed_doc_value().map(|s| s.into())
        } else {
            item.doc_value().map(|s| s.into())
        }
    }
}

/// Indicates where an external crate can be found.
pub enum ExternalLocation {
    /// Remote URL root of the external crate
    Remote(String),
    /// This external crate can be found in the local doc/ folder
    Local,
    /// The external crate could not be found.
    Unknown,
}

/// Metadata about an implementor of a trait.
pub struct Implementor {
    pub def_id: DefId,
    pub stability: Option<clean::Stability>,
    pub impl_: clean::Impl,
}

/// Metadata about implementations for a type.
#[derive(Clone)]
pub struct Impl {
    pub impl_item: clean::Item,
}

impl Impl {
    fn inner_impl(&self) -> &clean::Impl {
        match self.impl_item.inner {
            clean::ImplItem(ref impl_) => impl_,
            _ => panic!("non-impl item found in impl")
        }
    }

    fn trait_did(&self) -> Option<DefId> {
        self.inner_impl().trait_.def_id()
    }
}

#[derive(Debug)]
pub struct Error {
    file: PathBuf,
    error: io::Error,
}

impl error::Error for Error {
    fn description(&self) -> &str {
        self.error.description()
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "\"{}\": {}", self.file.display(), self.error)
    }
}

impl Error {
    pub fn new(e: io::Error, file: &Path) -> Error {
        Error {
            file: file.to_path_buf(),
            error: e,
        }
    }
}

macro_rules! try_err {
    ($e:expr, $file:expr) => ({
        match $e {
            Ok(e) => e,
            Err(e) => return Err(Error::new(e, $file)),
        }
    })
}

/// This cache is used to store information about the `clean::Crate` being
/// rendered in order to provide more useful documentation. This contains
/// information like all implementors of a trait, all traits a type implements,
/// documentation for all known traits, etc.
///
/// This structure purposefully does not implement `Clone` because it's intended
/// to be a fairly large and expensive structure to clone. Instead this adheres
/// to `Send` so it may be stored in a `Arc` instance and shared among the various
/// rendering threads.
#[derive(Default)]
pub struct Cache {
    /// Mapping of typaram ids to the name of the type parameter. This is used
    /// when pretty-printing a type (so pretty printing doesn't have to
    /// painfully maintain a context like this)
    pub typarams: FxHashMap<DefId, String>,

    /// Maps a type id to all known implementations for that type. This is only
    /// recognized for intra-crate `ResolvedPath` types, and is used to print
    /// out extra documentation on the page of an enum/struct.
    ///
    /// The values of the map are a list of implementations and documentation
    /// found on that implementation.
    pub impls: FxHashMap<DefId, Vec<Impl>>,

    /// Maintains a mapping of local crate node ids to the fully qualified name
    /// and "short type description" of that node. This is used when generating
    /// URLs when a type is being linked to. External paths are not located in
    /// this map because the `External` type itself has all the information
    /// necessary.
    pub paths: FxHashMap<DefId, (Vec<String>, ItemType)>,

    /// Similar to `paths`, but only holds external paths. This is only used for
    /// generating explicit hyperlinks to other crates.
    pub external_paths: FxHashMap<DefId, (Vec<String>, ItemType)>,

    /// This map contains information about all known traits of this crate.
    /// Implementations of a crate should inherit the documentation of the
    /// parent trait if no extra documentation is specified, and default methods
    /// should show up in documentation about trait implementations.
    pub traits: FxHashMap<DefId, clean::Trait>,

    /// When rendering traits, it's often useful to be able to list all
    /// implementors of the trait, and this mapping is exactly, that: a mapping
    /// of trait ids to the list of known implementors of the trait
    pub implementors: FxHashMap<DefId, Vec<Implementor>>,

    /// Cache of where external crate documentation can be found.
    pub extern_locations: FxHashMap<CrateNum, (String, PathBuf, ExternalLocation)>,

    /// Cache of where documentation for primitives can be found.
    pub primitive_locations: FxHashMap<clean::PrimitiveType, DefId>,

    // Note that external items for which `doc(hidden)` applies to are shown as
    // non-reachable while local items aren't. This is because we're reusing
    // the access levels from crateanalysis.
    pub access_levels: Arc<AccessLevels<DefId>>,

    /// The version of the crate being documented, if given fron the `--crate-version` flag.
    pub crate_version: Option<String>,

    // Private fields only used when initially crawling a crate to build a cache

    stack: Vec<String>,
    parent_stack: Vec<DefId>,
    parent_is_trait_impl: bool,
    search_index: Vec<IndexItem>,
    stripped_mod: bool,
    deref_trait_did: Option<DefId>,
    deref_mut_trait_did: Option<DefId>,
    owned_box_did: Option<DefId>,
    masked_crates: FxHashSet<CrateNum>,

    // In rare case where a structure is defined in one module but implemented
    // in another, if the implementing module is parsed before defining module,
    // then the fully qualified name of the structure isn't presented in `paths`
    // yet when its implementation methods are being indexed. Caches such methods
    // and their parent id here and indexes them at the end of crate parsing.
    orphan_impl_items: Vec<(DefId, clean::Item)>,
}

/// Temporary storage for data obtained during `RustdocVisitor::clean()`.
/// Later on moved into `CACHE_KEY`.
#[derive(Default)]
pub struct RenderInfo {
    pub inlined: FxHashSet<DefId>,
    pub external_paths: ::core::ExternalPaths,
    pub external_typarams: FxHashMap<DefId, String>,
    pub deref_trait_did: Option<DefId>,
    pub deref_mut_trait_did: Option<DefId>,
    pub owned_box_did: Option<DefId>,
}

/// Helper struct to render all source code to HTML pages
struct SourceCollector<'a> {
    scx: &'a mut SharedContext,

    /// Root destination to place all HTML output into
    dst: PathBuf,
}

/// Wrapper struct to render the source code of a file. This will do things like
/// adding line numbers to the left-hand side.
struct Source<'a>(&'a str);

// Helper structs for rendering items/sidebars and carrying along contextual
// information

#[derive(Copy, Clone)]
struct Item<'a> {
    cx: &'a Context,
    item: &'a clean::Item,
}

struct Sidebar<'a> { cx: &'a Context, item: &'a clean::Item, }

/// Struct representing one entry in the JS search index. These are all emitted
/// by hand to a large JS file at the end of cache-creation.
struct IndexItem {
    ty: ItemType,
    name: String,
    path: String,
    desc: String,
    parent: Option<DefId>,
    parent_idx: Option<usize>,
    search_type: Option<IndexItemFunctionType>,
}

impl ToJson for IndexItem {
    fn to_json(&self) -> Json {
        assert_eq!(self.parent.is_some(), self.parent_idx.is_some());

        let mut data = Vec::with_capacity(6);
        data.push((self.ty as usize).to_json());
        data.push(self.name.to_json());
        data.push(self.path.to_json());
        data.push(self.desc.to_json());
        data.push(self.parent_idx.to_json());
        data.push(self.search_type.to_json());

        Json::Array(data)
    }
}

/// A type used for the search index.
struct Type {
    name: Option<String>,
    generics: Option<Vec<String>>,
}

impl ToJson for Type {
    fn to_json(&self) -> Json {
        match self.name {
            Some(ref name) => {
                let mut data = BTreeMap::new();
                data.insert("name".to_owned(), name.to_json());
                if let Some(ref generics) = self.generics {
                    data.insert("generics".to_owned(), generics.to_json());
                }
                Json::Object(data)
            },
            None => Json::Null
        }
    }
}

/// Full type of functions/methods in the search index.
struct IndexItemFunctionType {
    inputs: Vec<Type>,
    output: Option<Type>
}

impl ToJson for IndexItemFunctionType {
    fn to_json(&self) -> Json {
        // If we couldn't figure out a type, just write `null`.
        if self.inputs.iter().chain(self.output.iter()).any(|ref i| i.name.is_none()) {
            Json::Null
        } else {
            let mut data = BTreeMap::new();
            data.insert("inputs".to_owned(), self.inputs.to_json());
            data.insert("output".to_owned(), self.output.to_json());
            Json::Object(data)
        }
    }
}

// TLS keys used to carry information around during rendering.

thread_local!(static CACHE_KEY: RefCell<Arc<Cache>> = Default::default());
thread_local!(pub static CURRENT_LOCATION_KEY: RefCell<Vec<String>> =
                    RefCell::new(Vec::new()));
thread_local!(pub static USED_ID_MAP: RefCell<FxHashMap<String, usize>> =
                    RefCell::new(init_ids()));

pub fn render_text<F: FnMut(RenderType) -> String>(mut render: F) -> (String, String) {
    // Save the state of USED_ID_MAP so it only gets updated once even
    // though we're rendering twice.
    let orig_used_id_map = USED_ID_MAP.with(|map| map.borrow().clone());
    let hoedown_output = render(RenderType::Hoedown);
    USED_ID_MAP.with(|map| *map.borrow_mut() = orig_used_id_map);
    let pulldown_output = render(RenderType::Pulldown);
    (hoedown_output, pulldown_output)
}

fn init_ids() -> FxHashMap<String, usize> {
    [
     "main",
     "search",
     "help",
     "TOC",
     "render-detail",
     "associated-types",
     "associated-const",
     "required-methods",
     "provided-methods",
     "implementors",
     "implementors-list",
     "methods",
     "deref-methods",
     "implementations",
    ].into_iter().map(|id| (String::from(*id), 1)).collect()
}

/// This method resets the local table of used ID attributes. This is typically
/// used at the beginning of rendering an entire HTML page to reset from the
/// previous state (if any).
pub fn reset_ids(embedded: bool) {
    USED_ID_MAP.with(|s| {
        *s.borrow_mut() = if embedded {
            init_ids()
        } else {
            FxHashMap()
        };
    });
}

pub fn derive_id(candidate: String) -> String {
    USED_ID_MAP.with(|map| {
        let id = match map.borrow_mut().get_mut(&candidate) {
            None => candidate,
            Some(a) => {
                let id = format!("{}-{}", candidate, *a);
                *a += 1;
                id
            }
        };

        map.borrow_mut().insert(id.clone(), 1);
        id
    })
}

/// Generates the documentation for `crate` into the directory `dst`
pub fn run(mut krate: clean::Crate,
           external_html: &ExternalHtml,
           playground_url: Option<String>,
           dst: PathBuf,
           passes: FxHashSet<String>,
           css_file_extension: Option<PathBuf>,
           renderinfo: RenderInfo,
           render_type: RenderType,
           sort_modules_alphabetically: bool) -> Result<(), Error> {
    let src_root = match krate.src {
        FileName::Real(ref p) => match p.parent() {
            Some(p) => p.to_path_buf(),
            None => PathBuf::new(),
        },
        _ => PathBuf::new(),
    };
    let mut scx = SharedContext {
        src_root,
        passes,
        include_sources: true,
        local_sources: FxHashMap(),
        issue_tracker_base_url: None,
        layout: layout::Layout {
            logo: "".to_string(),
            favicon: "".to_string(),
            external_html: external_html.clone(),
            krate: krate.name.clone(),
        },
        css_file_extension: css_file_extension.clone(),
        markdown_warnings: RefCell::new(vec![]),
        created_dirs: RefCell::new(FxHashSet()),
        sort_modules_alphabetically,
    };

    // If user passed in `--playground-url` arg, we fill in crate name here
    if let Some(url) = playground_url {
        markdown::PLAYGROUND.with(|slot| {
            *slot.borrow_mut() = Some((Some(krate.name.clone()), url));
        });
    }

    // Crawl the crate attributes looking for attributes which control how we're
    // going to emit HTML
    if let Some(attrs) = krate.module.as_ref().map(|m| &m.attrs) {
        for attr in attrs.lists("doc") {
            let name = attr.name().map(|s| s.as_str());
            match (name.as_ref().map(|s| &s[..]), attr.value_str()) {
                (Some("html_favicon_url"), Some(s)) => {
                    scx.layout.favicon = s.to_string();
                }
                (Some("html_logo_url"), Some(s)) => {
                    scx.layout.logo = s.to_string();
                }
                (Some("html_playground_url"), Some(s)) => {
                    markdown::PLAYGROUND.with(|slot| {
                        let name = krate.name.clone();
                        *slot.borrow_mut() = Some((Some(name), s.to_string()));
                    });
                }
                (Some("issue_tracker_base_url"), Some(s)) => {
                    scx.issue_tracker_base_url = Some(s.to_string());
                }
                (Some("html_no_source"), None) if attr.is_word() => {
                    scx.include_sources = false;
                }
                _ => {}
            }
        }
    }
    try_err!(fs::create_dir_all(&dst), &dst);
    krate = render_sources(&dst, &mut scx, krate)?;
    let cx = Context {
        current: Vec::new(),
        dst,
        render_redirect_pages: false,
        shared: Arc::new(scx),
        render_type,
    };

    // Crawl the crate to build various caches used for the output
    let RenderInfo {
        inlined: _,
        external_paths,
        external_typarams,
        deref_trait_did,
        deref_mut_trait_did,
        owned_box_did,
    } = renderinfo;

    let external_paths = external_paths.into_iter()
        .map(|(k, (v, t))| (k, (v, ItemType::from(t))))
        .collect();

    let mut cache = Cache {
        impls: FxHashMap(),
        external_paths,
        paths: FxHashMap(),
        implementors: FxHashMap(),
        stack: Vec::new(),
        parent_stack: Vec::new(),
        search_index: Vec::new(),
        parent_is_trait_impl: false,
        extern_locations: FxHashMap(),
        primitive_locations: FxHashMap(),
        stripped_mod: false,
        access_levels: krate.access_levels.clone(),
        crate_version: krate.version.take(),
        orphan_impl_items: Vec::new(),
        traits: mem::replace(&mut krate.external_traits, FxHashMap()),
        deref_trait_did,
        deref_mut_trait_did,
        owned_box_did,
        masked_crates: mem::replace(&mut krate.masked_crates, FxHashSet()),
        typarams: external_typarams,
    };

    // Cache where all our extern crates are located
    for &(n, ref e) in &krate.externs {
        let src_root = match e.src {
            FileName::Real(ref p) => match p.parent() {
                Some(p) => p.to_path_buf(),
                None => PathBuf::new(),
            },
            _ => PathBuf::new(),
        };
        cache.extern_locations.insert(n, (e.name.clone(), src_root,
                                          extern_location(e, &cx.dst)));

        let did = DefId { krate: n, index: CRATE_DEF_INDEX };
        cache.external_paths.insert(did, (vec![e.name.to_string()], ItemType::Module));
    }

    // Cache where all known primitives have their documentation located.
    //
    // Favor linking to as local extern as possible, so iterate all crates in
    // reverse topological order.
    for &(_, ref e) in krate.externs.iter().rev() {
        for &(def_id, prim, _) in &e.primitives {
            cache.primitive_locations.insert(prim, def_id);
        }
    }
    for &(def_id, prim, _) in &krate.primitives {
        cache.primitive_locations.insert(prim, def_id);
    }

    cache.stack.push(krate.name.clone());
    krate = cache.fold_crate(krate);

    // Build our search index
    let index = build_index(&krate, &mut cache);

    // Freeze the cache now that the index has been built. Put an Arc into TLS
    // for future parallelization opportunities
    let cache = Arc::new(cache);
    CACHE_KEY.with(|v| *v.borrow_mut() = cache.clone());
    CURRENT_LOCATION_KEY.with(|s| s.borrow_mut().clear());

    write_shared(&cx, &krate, &*cache, index)?;

    let scx = cx.shared.clone();

    // And finally render the whole crate's documentation
    let result = cx.krate(krate);

    let markdown_warnings = scx.markdown_warnings.borrow();
    if !markdown_warnings.is_empty() {
        let mut intro_msg = false;
        for &(ref span, ref text, ref diffs) in &*markdown_warnings {
            for d in diffs {
                render_difference(d, &mut intro_msg, span, text);
            }
        }
    }

    result
}

// A short, single-line view of `s`.
fn concise_str(mut s: &str) -> String {
    if s.contains('\n') {
        s = s.lines().next().expect("Impossible! We just found a newline");
    }
    if s.len() > 70 {
        let mut lo = 50;
        let mut hi = s.len() - 20;
        while !s.is_char_boundary(lo) {
            lo -= 1;
        }
        while !s.is_char_boundary(hi) {
            hi += 1;
        }
        return format!("{} ... {}", &s[..lo], &s[hi..]);
    }
    s.to_owned()
}

// Returns short versions of s1 and s2, starting from where the strings differ.
fn concise_compared_strs(s1: &str, s2: &str) -> (String, String) {
    let s1 = s1.trim();
    let s2 = s2.trim();
    if !s1.contains('\n') && !s2.contains('\n') && s1.len() <= 70 && s2.len() <= 70 {
        return (s1.to_owned(), s2.to_owned());
    }

    let mut start_byte = 0;
    for (c1, c2) in s1.chars().zip(s2.chars()) {
        if c1 != c2 {
            break;
        }

        start_byte += c1.len_utf8();
    }

    if start_byte == 0 {
        return (concise_str(s1), concise_str(s2));
    }

    let s1 = &s1[start_byte..];
    let s2 = &s2[start_byte..];
    (format!("...{}", concise_str(s1)), format!("...{}", concise_str(s2)))
}

fn print_message(msg: &str, intro_msg: &mut bool, span: &Span, text: &str) {
    if !*intro_msg {
        println!("WARNING: documentation for this crate may be rendered \
                  differently using the new Pulldown renderer.");
        println!("    See https://github.com/rust-lang/rust/issues/44229 for details.");
        *intro_msg = true;
    }
    println!("WARNING: rendering difference in `{}`", concise_str(text));
    println!("   --> {}:{}:{}", span.filename, span.loline, span.locol);
    println!("{}", msg);
}

pub fn render_difference(diff: &html_diff::Difference,
                         intro_msg: &mut bool,
                         span: &Span,
                         text: &str) {
    match *diff {
        html_diff::Difference::NodeType { ref elem, ref opposite_elem } => {
            print_message(&format!("    {} Types differ: expected: `{}`, found: `{}`",
                                   elem.path, elem.element_name, opposite_elem.element_name),
                          intro_msg, span, text);
        }
        html_diff::Difference::NodeName { ref elem, ref opposite_elem } => {
            print_message(&format!("    {} Tags differ: expected: `{}`, found: `{}`",
                                   elem.path, elem.element_name, opposite_elem.element_name),
                          intro_msg, span, text);
        }
        html_diff::Difference::NodeAttributes { ref elem,
                                                ref elem_attributes,
                                                ref opposite_elem_attributes,
                                                .. } => {
            print_message(&format!("    {} Attributes differ in `{}`: expected: `{:?}`, \
                                    found: `{:?}`",
                                   elem.path, elem.element_name, elem_attributes,
                                   opposite_elem_attributes),
                          intro_msg, span, text);
        }
        html_diff::Difference::NodeText { ref elem, ref elem_text, ref opposite_elem_text, .. } => {
            if elem_text.split("\n")
                        .zip(opposite_elem_text.split("\n"))
                        .any(|(a, b)| a.trim() != b.trim()) {
                let (s1, s2) = concise_compared_strs(elem_text, opposite_elem_text);
                print_message(&format!("    {} Text differs:\n        expected: `{}`\n        \
                                        found:    `{}`",
                                       elem.path, s1, s2),
                              intro_msg, span, text);
            }
        }
        html_diff::Difference::NotPresent { ref elem, ref opposite_elem } => {
            if let Some(ref elem) = *elem {
                print_message(&format!("    {} One element is missing: expected: `{}`",
                                       elem.path, elem.element_name),
                              intro_msg, span, text);
            } else if let Some(ref elem) = *opposite_elem {
                if elem.element_name.is_empty() {
                    print_message(&format!("    {} One element is missing: expected: `{}`",
                                           elem.path, concise_str(&elem.element_content)),
                                  intro_msg, span, text);
                } else {
                    print_message(&format!("    {} Unexpected element `{}`: found: `{}`",
                                           elem.path, elem.element_name,
                                           concise_str(&elem.element_content)),
                                  intro_msg, span, text);
                }
            }
        }
    }
}

/// Build the search index from the collected metadata
fn build_index(krate: &clean::Crate, cache: &mut Cache) -> String {
    let mut nodeid_to_pathid = FxHashMap();
    let mut crate_items = Vec::with_capacity(cache.search_index.len());
    let mut crate_paths = Vec::<Json>::new();

    let Cache { ref mut search_index,
                ref orphan_impl_items,
                ref mut paths, .. } = *cache;

    // Attach all orphan items to the type's definition if the type
    // has since been learned.
    for &(did, ref item) in orphan_impl_items {
        if let Some(&(ref fqp, _)) = paths.get(&did) {
            search_index.push(IndexItem {
                ty: item.type_(),
                name: item.name.clone().unwrap(),
                path: fqp[..fqp.len() - 1].join("::"),
                desc: plain_summary_line(item.doc_value()),
                parent: Some(did),
                parent_idx: None,
                search_type: get_index_search_type(&item),
            });
        }
    }

    // Reduce `NodeId` in paths into smaller sequential numbers,
    // and prune the paths that do not appear in the index.
    let mut lastpath = String::new();
    let mut lastpathid = 0usize;

    for item in search_index {
        item.parent_idx = item.parent.map(|nodeid| {
            if nodeid_to_pathid.contains_key(&nodeid) {
                *nodeid_to_pathid.get(&nodeid).unwrap()
            } else {
                let pathid = lastpathid;
                nodeid_to_pathid.insert(nodeid, pathid);
                lastpathid += 1;

                let &(ref fqp, short) = paths.get(&nodeid).unwrap();
                crate_paths.push(((short as usize), fqp.last().unwrap().clone()).to_json());
                pathid
            }
        });

        // Omit the parent path if it is same to that of the prior item.
        if lastpath == item.path {
            item.path.clear();
        } else {
            lastpath = item.path.clone();
        }
        crate_items.push(item.to_json());
    }

    let crate_doc = krate.module.as_ref().map(|module| {
        plain_summary_line(module.doc_value())
    }).unwrap_or(String::new());

    let mut crate_data = BTreeMap::new();
    crate_data.insert("doc".to_owned(), Json::String(crate_doc));
    crate_data.insert("items".to_owned(), Json::Array(crate_items));
    crate_data.insert("paths".to_owned(), Json::Array(crate_paths));

    // Collect the index into a string
    format!("searchIndex[{}] = {};",
            as_json(&krate.name),
            Json::Object(crate_data))
}

fn write_shared(cx: &Context,
                krate: &clean::Crate,
                cache: &Cache,
                search_index: String) -> Result<(), Error> {
    // Write out the shared files. Note that these are shared among all rustdoc
    // docs placed in the output directory, so this needs to be a synchronized
    // operation with respect to all other rustdocs running around.
    let _lock = flock::Lock::panicking_new(&cx.dst.join(".lock"), true, true, true);

    // Add all the static files. These may already exist, but we just
    // overwrite them anyway to make sure that they're fresh and up-to-date.

    write(cx.dst.join("main.js"),
          include_bytes!("static/main.js"))?;
    write(cx.dst.join("rustdoc.css"),
          include_bytes!("static/rustdoc.css"))?;
    write(cx.dst.join("main.css"),
          include_bytes!("static/styles/main.css"))?;
    if let Some(ref css) = cx.shared.css_file_extension {
        let mut content = String::new();
        let css = css.as_path();
        let mut f = try_err!(File::open(css), css);

        try_err!(f.read_to_string(&mut content), css);
        let css = cx.dst.join("theme.css");
        let css = css.as_path();
        let mut f = try_err!(File::create(css), css);
        try_err!(write!(f, "{}", &content), css);
    }
    write(cx.dst.join("normalize.css"),
          include_bytes!("static/normalize.css"))?;
    write(cx.dst.join("FiraSans-Regular.woff"),
          include_bytes!("static/FiraSans-Regular.woff"))?;
    write(cx.dst.join("FiraSans-Medium.woff"),
          include_bytes!("static/FiraSans-Medium.woff"))?;
    write(cx.dst.join("FiraSans-LICENSE.txt"),
          include_bytes!("static/FiraSans-LICENSE.txt"))?;
    write(cx.dst.join("Heuristica-Italic.woff"),
          include_bytes!("static/Heuristica-Italic.woff"))?;
    write(cx.dst.join("Heuristica-LICENSE.txt"),
          include_bytes!("static/Heuristica-LICENSE.txt"))?;
    write(cx.dst.join("SourceSerifPro-Regular.woff"),
          include_bytes!("static/SourceSerifPro-Regular.woff"))?;
    write(cx.dst.join("SourceSerifPro-Bold.woff"),
          include_bytes!("static/SourceSerifPro-Bold.woff"))?;
    write(cx.dst.join("SourceSerifPro-LICENSE.txt"),
          include_bytes!("static/SourceSerifPro-LICENSE.txt"))?;
    write(cx.dst.join("SourceCodePro-Regular.woff"),
          include_bytes!("static/SourceCodePro-Regular.woff"))?;
    write(cx.dst.join("SourceCodePro-Semibold.woff"),
          include_bytes!("static/SourceCodePro-Semibold.woff"))?;
    write(cx.dst.join("SourceCodePro-LICENSE.txt"),
          include_bytes!("static/SourceCodePro-LICENSE.txt"))?;
    write(cx.dst.join("LICENSE-MIT.txt"),
          include_bytes!("static/LICENSE-MIT.txt"))?;
    write(cx.dst.join("LICENSE-APACHE.txt"),
          include_bytes!("static/LICENSE-APACHE.txt"))?;
    write(cx.dst.join("COPYRIGHT.txt"),
          include_bytes!("static/COPYRIGHT.txt"))?;

    fn collect(path: &Path, krate: &str,
               key: &str) -> io::Result<Vec<String>> {
        let mut ret = Vec::new();
        if path.exists() {
            for line in BufReader::new(File::open(path)?).lines() {
                let line = line?;
                if !line.starts_with(key) {
                    continue;
                }
                if line.starts_with(&format!(r#"{}["{}"]"#, key, krate)) {
                    continue;
                }
                ret.push(line.to_string());
            }
        }
        Ok(ret)
    }

    // Update the search index
    let dst = cx.dst.join("search-index.js");
    let mut all_indexes = try_err!(collect(&dst, &krate.name, "searchIndex"), &dst);
    all_indexes.push(search_index);
    // Sort the indexes by crate so the file will be generated identically even
    // with rustdoc running in parallel.
    all_indexes.sort();
    let mut w = try_err!(File::create(&dst), &dst);
    try_err!(writeln!(&mut w, "var searchIndex = {{}};"), &dst);
    for index in &all_indexes {
        try_err!(writeln!(&mut w, "{}", *index), &dst);
    }
    try_err!(writeln!(&mut w, "initSearch(searchIndex);"), &dst);

    // Update the list of all implementors for traits
    let dst = cx.dst.join("implementors");
    for (&did, imps) in &cache.implementors {
        // Private modules can leak through to this phase of rustdoc, which
        // could contain implementations for otherwise private types. In some
        // rare cases we could find an implementation for an item which wasn't
        // indexed, so we just skip this step in that case.
        //
        // FIXME: this is a vague explanation for why this can't be a `get`, in
        //        theory it should be...
        let &(ref remote_path, remote_item_type) = match cache.paths.get(&did) {
            Some(p) => p,
            None => match cache.external_paths.get(&did) {
                Some(p) => p,
                None => continue,
            }
        };

        let mut have_impls = false;
        let mut implementors = format!(r#"implementors["{}"] = ["#, krate.name);
        for imp in imps {
            // If the trait and implementation are in the same crate, then
            // there's no need to emit information about it (there's inlining
            // going on). If they're in different crates then the crate defining
            // the trait will be interested in our implementation.
            if imp.def_id.krate == did.krate { continue }
            // If the implementation is from another crate then that crate
            // should add it.
            if !imp.def_id.is_local() { continue }
            have_impls = true;
            write!(implementors, "{},", as_json(&imp.impl_.to_string())).unwrap();
        }
        implementors.push_str("];");

        // Only create a js file if we have impls to add to it. If the trait is
        // documented locally though we always create the file to avoid dead
        // links.
        if !have_impls && !cache.paths.contains_key(&did) {
            continue;
        }

        let mut mydst = dst.clone();
        for part in &remote_path[..remote_path.len() - 1] {
            mydst.push(part);
        }
        try_err!(fs::create_dir_all(&mydst), &mydst);
        mydst.push(&format!("{}.{}.js",
                            remote_item_type.css_class(),
                            remote_path[remote_path.len() - 1]));

        let mut all_implementors = try_err!(collect(&mydst, &krate.name, "implementors"), &mydst);
        all_implementors.push(implementors);
        // Sort the implementors by crate so the file will be generated
        // identically even with rustdoc running in parallel.
        all_implementors.sort();

        let mut f = try_err!(File::create(&mydst), &mydst);
        try_err!(writeln!(&mut f, "(function() {{var implementors = {{}};"), &mydst);
        for implementor in &all_implementors {
            try_err!(writeln!(&mut f, "{}", *implementor), &mydst);
        }
        try_err!(writeln!(&mut f, "{}", r"
            if (window.register_implementors) {
                window.register_implementors(implementors);
            } else {
                window.pending_implementors = implementors;
            }
        "), &mydst);
        try_err!(writeln!(&mut f, r"}})()"), &mydst);
    }
    Ok(())
}

fn render_sources(dst: &Path, scx: &mut SharedContext,
                  krate: clean::Crate) -> Result<clean::Crate, Error> {
    info!("emitting source files");
    let dst = dst.join("src").join(&krate.name);
    try_err!(fs::create_dir_all(&dst), &dst);
    let mut folder = SourceCollector {
        dst,
        scx,
    };
    Ok(folder.fold_crate(krate))
}

/// Writes the entire contents of a string to a destination, not attempting to
/// catch any errors.
fn write(dst: PathBuf, contents: &[u8]) -> Result<(), Error> {
    Ok(try_err!(try_err!(File::create(&dst), &dst).write_all(contents), &dst))
}

/// Takes a path to a source file and cleans the path to it. This canonicalizes
/// things like ".." to components which preserve the "top down" hierarchy of a
/// static HTML tree. Each component in the cleaned path will be passed as an
/// argument to `f`. The very last component of the path (ie the file name) will
/// be passed to `f` if `keep_filename` is true, and ignored otherwise.
// FIXME (#9639): The closure should deal with &[u8] instead of &str
// FIXME (#9639): This is too conservative, rejecting non-UTF-8 paths
fn clean_srcpath<F>(src_root: &Path, p: &Path, keep_filename: bool, mut f: F) where
    F: FnMut(&str),
{
    // make it relative, if possible
    let p = p.strip_prefix(src_root).unwrap_or(p);

    let mut iter = p.components().peekable();

    while let Some(c) = iter.next() {
        if !keep_filename && iter.peek().is_none() {
            break;
        }

        match c {
            Component::ParentDir => f("up"),
            Component::Normal(c) => f(c.to_str().unwrap()),
            _ => continue,
        }
    }
}

/// Attempts to find where an external crate is located, given that we're
/// rendering in to the specified source destination.
fn extern_location(e: &clean::ExternalCrate, dst: &Path) -> ExternalLocation {
    // See if there's documentation generated into the local directory
    let local_location = dst.join(&e.name);
    if local_location.is_dir() {
        return Local;
    }

    // Failing that, see if there's an attribute specifying where to find this
    // external crate
    e.attrs.lists("doc")
     .filter(|a| a.check_name("html_root_url"))
     .filter_map(|a| a.value_str())
     .map(|url| {
        let mut url = url.to_string();
        if !url.ends_with("/") {
            url.push('/')
        }
        Remote(url)
    }).next().unwrap_or(Unknown) // Well, at least we tried.
}

impl<'a> DocFolder for SourceCollector<'a> {
    fn fold_item(&mut self, item: clean::Item) -> Option<clean::Item> {
        // If we're including source files, and we haven't seen this file yet,
        // then we need to render it out to the filesystem.
        if self.scx.include_sources
            // skip all invalid or macro spans
            && item.source.filename.is_real()
            // skip non-local items
            && item.def_id.is_local() {

            // If it turns out that we couldn't read this file, then we probably
            // can't read any of the files (generating html output from json or
            // something like that), so just don't include sources for the
            // entire crate. The other option is maintaining this mapping on a
            // per-file basis, but that's probably not worth it...
            self.scx
                .include_sources = match self.emit_source(&item.source.filename) {
                Ok(()) => true,
                Err(e) => {
                    println!("warning: source code was requested to be rendered, \
                              but processing `{}` had an error: {}",
                             item.source.filename, e);
                    println!("         skipping rendering of source code");
                    false
                }
            };
        }
        self.fold_item_recur(item)
    }
}

impl<'a> SourceCollector<'a> {
    /// Renders the given filename into its corresponding HTML source file.
    fn emit_source(&mut self, filename: &FileName) -> io::Result<()> {
        let p = match *filename {
            FileName::Real(ref file) => file,
            _ => return Ok(()),
        };
        if self.scx.local_sources.contains_key(&**p) {
            // We've already emitted this source
            return Ok(());
        }

        let mut contents = Vec::new();
        File::open(&p).and_then(|mut f| f.read_to_end(&mut contents))?;

        let contents = str::from_utf8(&contents).unwrap();

        // Remove the utf-8 BOM if any
        let contents = if contents.starts_with("\u{feff}") {
            &contents[3..]
        } else {
            contents
        };

        // Create the intermediate directories
        let mut cur = self.dst.clone();
        let mut root_path = String::from("../../");
        let mut href = String::new();
        clean_srcpath(&self.scx.src_root, &p, false, |component| {
            cur.push(component);
            fs::create_dir_all(&cur).unwrap();
            root_path.push_str("../");
            href.push_str(component);
            href.push('/');
        });
        let mut fname = p.file_name().expect("source has no filename")
                         .to_os_string();
        fname.push(".html");
        cur.push(&fname);
        href.push_str(&fname.to_string_lossy());

        let mut w = BufWriter::new(File::create(&cur)?);
        let title = format!("{} -- source", cur.file_name().unwrap()
                                               .to_string_lossy());
        let desc = format!("Source to the Rust file `{}`.", filename);
        let page = layout::Page {
            title: &title,
            css_class: "source",
            root_path: &root_path,
            description: &desc,
            keywords: BASIC_KEYWORDS,
        };
        layout::render(&mut w, &self.scx.layout,
                       &page, &(""), &Source(contents),
                       self.scx.css_file_extension.is_some())?;
        w.flush()?;
        self.scx.local_sources.insert(p.clone(), href);
        Ok(())
    }
}

impl DocFolder for Cache {
    fn fold_item(&mut self, item: clean::Item) -> Option<clean::Item> {
        // If this is a stripped module,
        // we don't want it or its children in the search index.
        let orig_stripped_mod = match item.inner {
            clean::StrippedItem(box clean::ModuleItem(..)) => {
                mem::replace(&mut self.stripped_mod, true)
            }
            _ => self.stripped_mod,
        };

        // Register any generics to their corresponding string. This is used
        // when pretty-printing types.
        if let Some(generics) = item.inner.generics() {
            self.generics(generics);
        }

        // Propagate a trait method's documentation to all implementors of the
        // trait.
        if let clean::TraitItem(ref t) = item.inner {
            self.traits.entry(item.def_id).or_insert_with(|| t.clone());
        }

        // Collect all the implementors of traits.
        if let clean::ImplItem(ref i) = item.inner {
            if !self.masked_crates.contains(&item.def_id.krate) {
                if let Some(did) = i.trait_.def_id() {
                    if i.for_.def_id().map_or(true, |d| !self.masked_crates.contains(&d.krate)) {
                        self.implementors.entry(did).or_insert(vec![]).push(Implementor {
                            def_id: item.def_id,
                            stability: item.stability.clone(),
                            impl_: i.clone(),
                        });
                    }
                }
            }
        }

        // Index this method for searching later on.
        if let Some(ref s) = item.name {
            let (parent, is_inherent_impl_item) = match item.inner {
                clean::StrippedItem(..) => ((None, None), false),
                clean::AssociatedConstItem(..) |
                clean::TypedefItem(_, true) if self.parent_is_trait_impl => {
                    // skip associated items in trait impls
                    ((None, None), false)
                }
                clean::AssociatedTypeItem(..) |
                clean::TyMethodItem(..) |
                clean::StructFieldItem(..) |
                clean::VariantItem(..) => {
                    ((Some(*self.parent_stack.last().unwrap()),
                      Some(&self.stack[..self.stack.len() - 1])),
                     false)
                }
                clean::MethodItem(..) | clean::AssociatedConstItem(..) => {
                    if self.parent_stack.is_empty() {
                        ((None, None), false)
                    } else {
                        let last = self.parent_stack.last().unwrap();
                        let did = *last;
                        let path = match self.paths.get(&did) {
                            // The current stack not necessarily has correlation
                            // for where the type was defined. On the other
                            // hand, `paths` always has the right
                            // information if present.
                            Some(&(ref fqp, ItemType::Trait)) |
                            Some(&(ref fqp, ItemType::Struct)) |
                            Some(&(ref fqp, ItemType::Union)) |
                            Some(&(ref fqp, ItemType::Enum)) =>
                                Some(&fqp[..fqp.len() - 1]),
                            Some(..) => Some(&*self.stack),
                            None => None
                        };
                        ((Some(*last), path), true)
                    }
                }
                _ => ((None, Some(&*self.stack)), false)
            };

            match parent {
                (parent, Some(path)) if is_inherent_impl_item || (!self.stripped_mod) => {
                    debug_assert!(!item.is_stripped());

                    // A crate has a module at its root, containing all items,
                    // which should not be indexed. The crate-item itself is
                    // inserted later on when serializing the search-index.
                    if item.def_id.index != CRATE_DEF_INDEX {
                        self.search_index.push(IndexItem {
                            ty: item.type_(),
                            name: s.to_string(),
                            path: path.join("::").to_string(),
                            desc: plain_summary_line(item.doc_value()),
                            parent,
                            parent_idx: None,
                            search_type: get_index_search_type(&item),
                        });
                    }
                }
                (Some(parent), None) if is_inherent_impl_item => {
                    // We have a parent, but we don't know where they're
                    // defined yet. Wait for later to index this item.
                    self.orphan_impl_items.push((parent, item.clone()));
                }
                _ => {}
            }
        }

        // Keep track of the fully qualified path for this item.
        let pushed = match item.name {
            Some(ref n) if !n.is_empty() => {
                self.stack.push(n.to_string());
                true
            }
            _ => false,
        };

        match item.inner {
            clean::StructItem(..) | clean::EnumItem(..) |
            clean::TypedefItem(..) | clean::TraitItem(..) |
            clean::FunctionItem(..) | clean::ModuleItem(..) |
            clean::ForeignFunctionItem(..) | clean::ForeignStaticItem(..) |
            clean::ConstantItem(..) | clean::StaticItem(..) |
            clean::UnionItem(..) | clean::ForeignTypeItem
            if !self.stripped_mod => {
                // Reexported items mean that the same id can show up twice
                // in the rustdoc ast that we're looking at. We know,
                // however, that a reexported item doesn't show up in the
                // `public_items` map, so we can skip inserting into the
                // paths map if there was already an entry present and we're
                // not a public item.
                if
                    !self.paths.contains_key(&item.def_id) ||
                    self.access_levels.is_public(item.def_id)
                {
                    self.paths.insert(item.def_id,
                                      (self.stack.clone(), item.type_()));
                }
            }
            // Link variants to their parent enum because pages aren't emitted
            // for each variant.
            clean::VariantItem(..) if !self.stripped_mod => {
                let mut stack = self.stack.clone();
                stack.pop();
                self.paths.insert(item.def_id, (stack, ItemType::Enum));
            }

            clean::PrimitiveItem(..) if item.visibility.is_some() => {
                self.paths.insert(item.def_id, (self.stack.clone(),
                                                item.type_()));
            }

            _ => {}
        }

        // Maintain the parent stack
        let orig_parent_is_trait_impl = self.parent_is_trait_impl;
        let parent_pushed = match item.inner {
            clean::TraitItem(..) | clean::EnumItem(..) | clean::ForeignTypeItem |
            clean::StructItem(..) | clean::UnionItem(..) => {
                self.parent_stack.push(item.def_id);
                self.parent_is_trait_impl = false;
                true
            }
            clean::ImplItem(ref i) => {
                self.parent_is_trait_impl = i.trait_.is_some();
                match i.for_ {
                    clean::ResolvedPath{ did, .. } => {
                        self.parent_stack.push(did);
                        true
                    }
                    ref t => {
                        let prim_did = t.primitive_type().and_then(|t| {
                            self.primitive_locations.get(&t).cloned()
                        });
                        match prim_did {
                            Some(did) => {
                                self.parent_stack.push(did);
                                true
                            }
                            None => false,
                        }
                    }
                }
            }
            _ => false
        };

        // Once we've recursively found all the generics, hoard off all the
        // implementations elsewhere.
        let ret = self.fold_item_recur(item).and_then(|item| {
            if let clean::Item { inner: clean::ImplItem(_), .. } = item {
                // Figure out the id of this impl. This may map to a
                // primitive rather than always to a struct/enum.
                // Note: matching twice to restrict the lifetime of the `i` borrow.
                let mut dids = FxHashSet();
                if let clean::Item { inner: clean::ImplItem(ref i), .. } = item {
                    let masked_trait = i.trait_.def_id().map_or(false,
                        |d| self.masked_crates.contains(&d.krate));
                    if !masked_trait {
                        match i.for_ {
                            clean::ResolvedPath { did, .. } |
                            clean::BorrowedRef {
                                type_: box clean::ResolvedPath { did, .. }, ..
                            } => {
                                dids.insert(did);
                            }
                            ref t => {
                                let did = t.primitive_type().and_then(|t| {
                                    self.primitive_locations.get(&t).cloned()
                                });

                                if let Some(did) = did {
                                    dids.insert(did);
                                }
                            }
                        }
                    }

                    if let Some(generics) = i.trait_.as_ref().and_then(|t| t.generics()) {
                        for bound in generics {
                            if let Some(did) = bound.def_id() {
                                dids.insert(did);
                            }
                        }
                    }
                } else {
                    unreachable!()
                };
                for did in dids {
                    self.impls.entry(did).or_insert(vec![]).push(Impl {
                        impl_item: item.clone(),
                    });
                }
                None
            } else {
                Some(item)
            }
        });

        if pushed { self.stack.pop().unwrap(); }
        if parent_pushed { self.parent_stack.pop().unwrap(); }
        self.stripped_mod = orig_stripped_mod;
        self.parent_is_trait_impl = orig_parent_is_trait_impl;
        ret
    }
}

impl<'a> Cache {
    fn generics(&mut self, generics: &clean::Generics) {
        for typ in &generics.type_params {
            self.typarams.insert(typ.did, typ.name.clone());
        }
    }
}

impl Context {
    /// String representation of how to get back to the root path of the 'doc/'
    /// folder in terms of a relative URL.
    fn root_path(&self) -> String {
        repeat("../").take(self.current.len()).collect::<String>()
    }

    /// Recurse in the directory structure and change the "root path" to make
    /// sure it always points to the top (relatively).
    fn recurse<T, F>(&mut self, s: String, f: F) -> T where
        F: FnOnce(&mut Context) -> T,
    {
        if s.is_empty() {
            panic!("Unexpected empty destination: {:?}", self.current);
        }
        let prev = self.dst.clone();
        self.dst.push(&s);
        self.current.push(s);

        info!("Recursing into {}", self.dst.display());

        let ret = f(self);

        info!("Recursed; leaving {}", self.dst.display());

        // Go back to where we were at
        self.dst = prev;
        self.current.pop().unwrap();

        ret
    }

    /// Main method for rendering a crate.
    ///
    /// This currently isn't parallelized, but it'd be pretty easy to add
    /// parallelization to this function.
    fn krate(self, mut krate: clean::Crate) -> Result<(), Error> {
        let mut item = match krate.module.take() {
            Some(i) => i,
            None => return Ok(()),
        };
        item.name = Some(krate.name);

        // Render the crate documentation
        let mut work = vec![(self, item)];

        while let Some((mut cx, item)) = work.pop() {
            cx.item(item, |cx, item| {
                work.push((cx.clone(), item))
            })?
        }
        Ok(())
    }

    fn render_item(&self,
                   writer: &mut io::Write,
                   it: &clean::Item,
                   pushname: bool)
                   -> io::Result<()> {
        // A little unfortunate that this is done like this, but it sure
        // does make formatting *a lot* nicer.
        CURRENT_LOCATION_KEY.with(|slot| {
            *slot.borrow_mut() = self.current.clone();
        });

        let mut title = if it.is_primitive() {
            // No need to include the namespace for primitive types
            String::new()
        } else {
            self.current.join("::")
        };
        if pushname {
            if !title.is_empty() {
                title.push_str("::");
            }
            title.push_str(it.name.as_ref().unwrap());
        }
        title.push_str(" - Rust");
        let tyname = it.type_().css_class();
        let desc = if it.is_crate() {
            format!("API documentation for the Rust `{}` crate.",
                    self.shared.layout.krate)
        } else {
            format!("API documentation for the Rust `{}` {} in crate `{}`.",
                    it.name.as_ref().unwrap(), tyname, self.shared.layout.krate)
        };
        let keywords = make_item_keywords(it);
        let page = layout::Page {
            css_class: tyname,
            root_path: &self.root_path(),
            title: &title,
            description: &desc,
            keywords: &keywords,
        };

        reset_ids(true);

        if !self.render_redirect_pages {
            layout::render(writer, &self.shared.layout, &page,
                           &Sidebar{ cx: self, item: it },
                           &Item{ cx: self, item: it },
                           self.shared.css_file_extension.is_some())?;
        } else {
            let mut url = self.root_path();
            if let Some(&(ref names, ty)) = cache().paths.get(&it.def_id) {
                for name in &names[..names.len() - 1] {
                    url.push_str(name);
                    url.push_str("/");
                }
                url.push_str(&item_path(ty, names.last().unwrap()));
                layout::redirect(writer, &url)?;
            }
        }
        Ok(())
    }

    /// Non-parallelized version of rendering an item. This will take the input
    /// item, render its contents, and then invoke the specified closure with
    /// all sub-items which need to be rendered.
    ///
    /// The rendering driver uses this closure to queue up more work.
    fn item<F>(&mut self, item: clean::Item, mut f: F) -> Result<(), Error> where
        F: FnMut(&mut Context, clean::Item),
    {
        // Stripped modules survive the rustdoc passes (i.e. `strip-private`)
        // if they contain impls for public types. These modules can also
        // contain items such as publicly reexported structures.
        //
        // External crates will provide links to these structures, so
        // these modules are recursed into, but not rendered normally
        // (a flag on the context).
        if !self.render_redirect_pages {
            self.render_redirect_pages = item.is_stripped();
        }

        if item.is_mod() {
            // modules are special because they add a namespace. We also need to
            // recurse into the items of the module as well.
            let name = item.name.as_ref().unwrap().to_string();
            let mut item = Some(item);
            self.recurse(name, |this| {
                let item = item.take().unwrap();

                let mut buf = Vec::new();
                this.render_item(&mut buf, &item, false).unwrap();
                // buf will be empty if the module is stripped and there is no redirect for it
                if !buf.is_empty() {
                    try_err!(this.shared.ensure_dir(&this.dst), &this.dst);
                    let joint_dst = this.dst.join("index.html");
                    let mut dst = try_err!(File::create(&joint_dst), &joint_dst);
                    try_err!(dst.write_all(&buf), &joint_dst);
                }

                let m = match item.inner {
                    clean::StrippedItem(box clean::ModuleItem(m)) |
                    clean::ModuleItem(m) => m,
                    _ => unreachable!()
                };

                // Render sidebar-items.js used throughout this module.
                if !this.render_redirect_pages {
                    let items = this.build_sidebar_items(&m);
                    let js_dst = this.dst.join("sidebar-items.js");
                    let mut js_out = BufWriter::new(try_err!(File::create(&js_dst), &js_dst));
                    try_err!(write!(&mut js_out, "initSidebarItems({});",
                                    as_json(&items)), &js_dst);
                }

                for item in m.items {
                    f(this,item);
                }

                Ok(())
            })?;
        } else if item.name.is_some() {
            let mut buf = Vec::new();
            self.render_item(&mut buf, &item, true).unwrap();
            // buf will be empty if the item is stripped and there is no redirect for it
            if !buf.is_empty() {
                let name = item.name.as_ref().unwrap();
                let item_type = item.type_();
                let file_name = &item_path(item_type, name);
                try_err!(self.shared.ensure_dir(&self.dst), &self.dst);
                let joint_dst = self.dst.join(file_name);
                let mut dst = try_err!(File::create(&joint_dst), &joint_dst);
                try_err!(dst.write_all(&buf), &joint_dst);

                // Redirect from a sane URL using the namespace to Rustdoc's
                // URL for the page.
                let redir_name = format!("{}.{}.html", name, item_type.name_space());
                let redir_dst = self.dst.join(redir_name);
                if let Ok(redirect_out) = OpenOptions::new().create_new(true)
                                                                .write(true)
                                                                .open(&redir_dst) {
                    let mut redirect_out = BufWriter::new(redirect_out);
                    try_err!(layout::redirect(&mut redirect_out, file_name), &redir_dst);
                }

                // If the item is a macro, redirect from the old macro URL (with !)
                // to the new one (without).
                // FIXME(#35705) remove this redirect.
                if item_type == ItemType::Macro {
                    let redir_name = format!("{}.{}!.html", item_type, name);
                    let redir_dst = self.dst.join(redir_name);
                    let redirect_out = try_err!(File::create(&redir_dst), &redir_dst);
                    let mut redirect_out = BufWriter::new(redirect_out);
                    try_err!(layout::redirect(&mut redirect_out, file_name), &redir_dst);
                }
            }
        }
        Ok(())
    }

    fn build_sidebar_items(&self, m: &clean::Module) -> BTreeMap<String, Vec<NameDoc>> {
        // BTreeMap instead of HashMap to get a sorted output
        let mut map = BTreeMap::new();
        for item in &m.items {
            if item.is_stripped() { continue }

            let short = item.type_().css_class();
            let myname = match item.name {
                None => continue,
                Some(ref s) => s.to_string(),
            };
            let short = short.to_string();
            map.entry(short).or_insert(vec![])
                .push((myname, Some(plain_summary_line(item.doc_value()))));
        }

        if self.shared.sort_modules_alphabetically {
            for (_, items) in &mut map {
                items.sort();
            }
        }
        map
    }
}

impl<'a> Item<'a> {
    /// Generate a url appropriate for an `href` attribute back to the source of
    /// this item.
    ///
    /// The url generated, when clicked, will redirect the browser back to the
    /// original source code.
    ///
    /// If `None` is returned, then a source link couldn't be generated. This
    /// may happen, for example, with externally inlined items where the source
    /// of their crate documentation isn't known.
    fn src_href(&self) -> Option<String> {
        let mut root = self.cx.root_path();

        let cache = cache();
        let mut path = String::new();

        // We can safely ignore macros from other libraries
        let file = match self.item.source.filename {
            FileName::Real(ref path) => path,
            _ => return None,
        };

        let (krate, path) = if self.item.def_id.is_local() {
            if let Some(path) = self.cx.shared.local_sources.get(file) {
                (&self.cx.shared.layout.krate, path)
            } else {
                return None;
            }
        } else {
            let (krate, src_root) = match cache.extern_locations.get(&self.item.def_id.krate) {
                Some(&(ref name, ref src, Local)) => (name, src),
                Some(&(ref name, ref src, Remote(ref s))) => {
                    root = s.to_string();
                    (name, src)
                }
                Some(&(_, _, Unknown)) | None => return None,
            };

            clean_srcpath(&src_root, file, false, |component| {
                path.push_str(component);
                path.push('/');
            });
            let mut fname = file.file_name().expect("source has no filename")
                                .to_os_string();
            fname.push(".html");
            path.push_str(&fname.to_string_lossy());
            (krate, &path)
        };

        let lines = if self.item.source.loline == self.item.source.hiline {
            format!("{}", self.item.source.loline)
        } else {
            format!("{}-{}", self.item.source.loline, self.item.source.hiline)
        };
        Some(format!("{root}src/{krate}/{path}#{lines}",
                     root = Escape(&root),
                     krate = krate,
                     path = path,
                     lines = lines))
    }
}

impl<'a> fmt::Display for Item<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        debug_assert!(!self.item.is_stripped());
        // Write the breadcrumb trail header for the top
        write!(fmt, "\n<h1 class='fqn'><span class='in-band'>")?;
        match self.item.inner {
            clean::ModuleItem(ref m) => if m.is_crate {
                    write!(fmt, "Crate ")?;
                } else {
                    write!(fmt, "Module ")?;
                },
            clean::FunctionItem(..) | clean::ForeignFunctionItem(..) => write!(fmt, "Function ")?,
            clean::TraitItem(..) => write!(fmt, "Trait ")?,
            clean::StructItem(..) => write!(fmt, "Struct ")?,
            clean::UnionItem(..) => write!(fmt, "Union ")?,
            clean::EnumItem(..) => write!(fmt, "Enum ")?,
            clean::TypedefItem(..) => write!(fmt, "Type Definition ")?,
            clean::MacroItem(..) => write!(fmt, "Macro ")?,
            clean::PrimitiveItem(..) => write!(fmt, "Primitive Type ")?,
            clean::StaticItem(..) | clean::ForeignStaticItem(..) => write!(fmt, "Static ")?,
            clean::ConstantItem(..) => write!(fmt, "Constant ")?,
            clean::ForeignTypeItem => write!(fmt, "Foreign Type ")?,
            _ => {
                // We don't generate pages for any other type.
                unreachable!();
            }
        }
        if !self.item.is_primitive() {
            let cur = &self.cx.current;
            let amt = if self.item.is_mod() { cur.len() - 1 } else { cur.len() };
            for (i, component) in cur.iter().enumerate().take(amt) {
                write!(fmt, "<a href='{}index.html'>{}</a>::<wbr>",
                       repeat("../").take(cur.len() - i - 1)
                                    .collect::<String>(),
                       component)?;
            }
        }
        write!(fmt, "<a class=\"{}\" href=''>{}</a>",
               self.item.type_(), self.item.name.as_ref().unwrap())?;

        write!(fmt, "</span>")?; // in-band
        write!(fmt, "<span class='out-of-band'>")?;
        if let Some(version) = self.item.stable_since() {
            write!(fmt, "<span class='since' title='Stable since Rust version {0}'>{0}</span>",
                   version)?;
        }
        write!(fmt,
               r##"<span id='render-detail'>
                   <a id="toggle-all-docs" href="javascript:void(0)" title="collapse all docs">
                       [<span class='inner'>&#x2212;</span>]
                   </a>
               </span>"##)?;

        // Write `src` tag
        //
        // When this item is part of a `pub use` in a downstream crate, the
        // [src] link in the downstream documentation will actually come back to
        // this page, and this link will be auto-clicked. The `id` attribute is
        // used to find the link to auto-click.
        if self.cx.shared.include_sources && !self.item.is_primitive() {
            if let Some(l) = self.src_href() {
                write!(fmt, "<a class='srclink' href='{}' title='{}'>[src]</a>",
                       l, "goto source code")?;
            }
        }

        write!(fmt, "</span>")?; // out-of-band

        write!(fmt, "</h1>\n")?;

        match self.item.inner {
            clean::ModuleItem(ref m) => {
                item_module(fmt, self.cx, self.item, &m.items)
            }
            clean::FunctionItem(ref f) | clean::ForeignFunctionItem(ref f) =>
                item_function(fmt, self.cx, self.item, f),
            clean::TraitItem(ref t) => item_trait(fmt, self.cx, self.item, t),
            clean::StructItem(ref s) => item_struct(fmt, self.cx, self.item, s),
            clean::UnionItem(ref s) => item_union(fmt, self.cx, self.item, s),
            clean::EnumItem(ref e) => item_enum(fmt, self.cx, self.item, e),
            clean::TypedefItem(ref t, _) => item_typedef(fmt, self.cx, self.item, t),
            clean::MacroItem(ref m) => item_macro(fmt, self.cx, self.item, m),
            clean::PrimitiveItem(ref p) => item_primitive(fmt, self.cx, self.item, p),
            clean::StaticItem(ref i) | clean::ForeignStaticItem(ref i) =>
                item_static(fmt, self.cx, self.item, i),
            clean::ConstantItem(ref c) => item_constant(fmt, self.cx, self.item, c),
            clean::ForeignTypeItem => item_foreign_type(fmt, self.cx, self.item),
            _ => {
                // We don't generate pages for any other type.
                unreachable!();
            }
        }
    }
}

fn item_path(ty: ItemType, name: &str) -> String {
    match ty {
        ItemType::Module => format!("{}/index.html", name),
        _ => format!("{}.{}.html", ty.css_class(), name),
    }
}

fn full_path(cx: &Context, item: &clean::Item) -> String {
    let mut s = cx.current.join("::");
    s.push_str("::");
    s.push_str(item.name.as_ref().unwrap());
    s
}

fn shorter<'a>(s: Option<&'a str>) -> String {
    match s {
        Some(s) => s.lines()
            .skip_while(|s| s.chars().all(|c| c.is_whitespace()))
            .take_while(|line|{
            (*line).chars().any(|chr|{
                !chr.is_whitespace()
            })
        }).collect::<Vec<_>>().join("\n"),
        None => "".to_string()
    }
}

#[inline]
fn plain_summary_line(s: Option<&str>) -> String {
    let line = shorter(s).replace("\n", " ");
    markdown::plain_summary_line(&line[..])
}

fn document(w: &mut fmt::Formatter, cx: &Context, item: &clean::Item) -> fmt::Result {
    if let Some(ref name) = item.name {
        info!("Documenting {}", name);
    }
    document_stability(w, cx, item)?;
    let prefix = render_assoc_const_value(item);
    document_full(w, item, cx, &prefix)?;
    Ok(())
}

/// Render md_text as markdown. Warns the user if there are difference in
/// rendering between Pulldown and Hoedown.
fn render_markdown(w: &mut fmt::Formatter,
                   md_text: &str,
                   span: Span,
                   render_type: RenderType,
                   prefix: &str,
                   scx: &SharedContext)
                   -> fmt::Result {
    let (hoedown_output, pulldown_output) = render_text(|ty| format!("{}", Markdown(md_text, ty)));
    let mut differences = html_diff::get_differences(&pulldown_output, &hoedown_output);
    differences.retain(|s| {
        match *s {
            html_diff::Difference::NodeText { ref elem_text,
                                              ref opposite_elem_text,
                                              .. }
                if elem_text.split_whitespace().eq(opposite_elem_text.split_whitespace()) => {
                    false
            }
            _ => true,
        }
    });

    if !differences.is_empty() {
        scx.markdown_warnings.borrow_mut().push((span, md_text.to_owned(), differences));
    }

    write!(w, "<div class='docblock'>{}{}</div>",
           prefix,
           if render_type == RenderType::Pulldown { pulldown_output } else { hoedown_output })
}

fn document_short(w: &mut fmt::Formatter, item: &clean::Item, link: AssocItemLink,
                  cx: &Context, prefix: &str) -> fmt::Result {
    if let Some(s) = item.doc_value() {
        let markdown = if s.contains('\n') {
            format!("{} [Read more]({})",
                    &plain_summary_line(Some(s)), naive_assoc_href(item, link))
        } else {
            format!("{}", &plain_summary_line(Some(s)))
        };
        render_markdown(w, &markdown, item.source.clone(), cx.render_type, prefix, &cx.shared)?;
    } else if !prefix.is_empty() {
        write!(w, "<div class='docblock'>{}</div>", prefix)?;
    }
    Ok(())
}

fn render_assoc_const_value(item: &clean::Item) -> String {
    match item.inner {
        clean::AssociatedConstItem(ref ty, Some(ref default)) => {
            highlight::render_with_highlighting(
                &format!("{}: {:#} = {}", item.name.as_ref().unwrap(), ty, default),
                None,
                None,
                None,
                None,
            )
        }
        _ => String::new(),
    }
}

fn document_full(w: &mut fmt::Formatter, item: &clean::Item,
                 cx: &Context, prefix: &str) -> fmt::Result {
    if let Some(s) = cx.shared.maybe_collapsed_doc_value(item) {
        debug!("Doc block: =====\n{}\n=====", s);
        render_markdown(w, &*s, item.source.clone(), cx.render_type, prefix, &cx.shared)?;
    } else if !prefix.is_empty() {
        write!(w, "<div class='docblock'>{}</div>", prefix)?;
    }
    Ok(())
}

fn document_stability(w: &mut fmt::Formatter, cx: &Context, item: &clean::Item) -> fmt::Result {
    let stabilities = short_stability(item, cx, true);
    if !stabilities.is_empty() {
        write!(w, "<div class='stability'>")?;
        for stability in stabilities {
            write!(w, "{}", stability)?;
        }
        write!(w, "</div>")?;
    }
    Ok(())
}

fn name_key(name: &str) -> (&str, u64, usize) {
    // find number at end
    let split = name.bytes().rposition(|b| b < b'0' || b'9' < b).map_or(0, |s| s + 1);

    // count leading zeroes
    let after_zeroes =
        name[split..].bytes().position(|b| b != b'0').map_or(name.len(), |extra| split + extra);

    // sort leading zeroes last
    let num_zeroes = after_zeroes - split;

    match name[split..].parse() {
        Ok(n) => (&name[..split], n, num_zeroes),
        Err(_) => (name, 0, num_zeroes),
    }
}

fn item_module(w: &mut fmt::Formatter, cx: &Context,
               item: &clean::Item, items: &[clean::Item]) -> fmt::Result {
    document(w, cx, item)?;

    let mut indices = (0..items.len()).filter(|i| {
        if let clean::AutoImplItem(..) = items[*i].inner {
            return false;
        }
        !items[*i].is_stripped()
    }).collect::<Vec<usize>>();

    // the order of item types in the listing
    fn reorder(ty: ItemType) -> u8 {
        match ty {
            ItemType::ExternCrate     => 0,
            ItemType::Import          => 1,
            ItemType::Primitive       => 2,
            ItemType::Module          => 3,
            ItemType::Macro           => 4,
            ItemType::Struct          => 5,
            ItemType::Enum            => 6,
            ItemType::Constant        => 7,
            ItemType::Static          => 8,
            ItemType::Trait           => 9,
            ItemType::Function        => 10,
            ItemType::Typedef         => 12,
            ItemType::Union           => 13,
            _                         => 14 + ty as u8,
        }
    }

    fn cmp(i1: &clean::Item, i2: &clean::Item, idx1: usize, idx2: usize) -> Ordering {
        let ty1 = i1.type_();
        let ty2 = i2.type_();
        if ty1 != ty2 {
            return (reorder(ty1), idx1).cmp(&(reorder(ty2), idx2))
        }
        let s1 = i1.stability.as_ref().map(|s| s.level);
        let s2 = i2.stability.as_ref().map(|s| s.level);
        match (s1, s2) {
            (Some(stability::Unstable), Some(stability::Stable)) => return Ordering::Greater,
            (Some(stability::Stable), Some(stability::Unstable)) => return Ordering::Less,
            _ => {}
        }
        let lhs = i1.name.as_ref().map_or("", |s| &**s);
        let rhs = i2.name.as_ref().map_or("", |s| &**s);
        name_key(lhs).cmp(&name_key(rhs))
    }

    if cx.shared.sort_modules_alphabetically {
        indices.sort_by(|&i1, &i2| cmp(&items[i1], &items[i2], i1, i2));
    }
    // This call is to remove reexport duplicates in cases such as:
    //
    // ```
    // pub mod foo {
    //     pub mod bar {
    //         pub trait Double { fn foo(); }
    //     }
    // }
    //
    // pub use foo::bar::*;
    // pub use foo::*;
    // ```
    //
    // `Double` will appear twice in the generated docs.
    //
    // FIXME: This code is quite ugly and could be improved. Small issue: DefId
    // can be identical even if the elements are different (mostly in imports).
    // So in case this is an import, we keep everything by adding a "unique id"
    // (which is the position in the vector).
    indices.dedup_by_key(|i| (items[*i].def_id,
                              if items[*i].name.as_ref().is_some() {
                                  Some(full_path(cx, &items[*i]).clone())
                              } else {
                                  None
                              },
                              items[*i].type_(),
                              if items[*i].is_import() {
                                  *i
                              } else {
                                  0
                              }));

    debug!("{:?}", indices);
    let mut curty = None;
    for &idx in &indices {
        let myitem = &items[idx];
        if myitem.is_stripped() {
            continue;
        }

        let myty = Some(myitem.type_());
        if curty == Some(ItemType::ExternCrate) && myty == Some(ItemType::Import) {
            // Put `extern crate` and `use` re-exports in the same section.
            curty = myty;
        } else if myty != curty {
            if curty.is_some() {
                write!(w, "</table>")?;
            }
            curty = myty;
            let (short, name) = match myty.unwrap() {
                ItemType::ExternCrate |
                ItemType::Import          => ("reexports", "Reexports"),
                ItemType::Module          => ("modules", "Modules"),
                ItemType::Struct          => ("structs", "Structs"),
                ItemType::Union           => ("unions", "Unions"),
                ItemType::Enum            => ("enums", "Enums"),
                ItemType::Function        => ("functions", "Functions"),
                ItemType::Typedef         => ("types", "Type Definitions"),
                ItemType::Static          => ("statics", "Statics"),
                ItemType::Constant        => ("constants", "Constants"),
                ItemType::Trait           => ("traits", "Traits"),
                ItemType::Impl            => ("impls", "Implementations"),
                ItemType::TyMethod        => ("tymethods", "Type Methods"),
                ItemType::Method          => ("methods", "Methods"),
                ItemType::StructField     => ("fields", "Struct Fields"),
                ItemType::Variant         => ("variants", "Variants"),
                ItemType::Macro           => ("macros", "Macros"),
                ItemType::Primitive       => ("primitives", "Primitive Types"),
                ItemType::AssociatedType  => ("associated-types", "Associated Types"),
                ItemType::AssociatedConst => ("associated-consts", "Associated Constants"),
                ItemType::ForeignType     => ("foreign-types", "Foreign Types"),
            };
            write!(w, "<h2 id='{id}' class='section-header'>\
                       <a href=\"#{id}\">{name}</a></h2>\n<table>",
                   id = derive_id(short.to_owned()), name = name)?;
        }

        match myitem.inner {
            clean::ExternCrateItem(ref name, ref src) => {
                use html::format::HRef;

                match *src {
                    Some(ref src) => {
                        write!(w, "<tr><td><code>{}extern crate {} as {};",
                               VisSpace(&myitem.visibility),
                               HRef::new(myitem.def_id, src),
                               name)?
                    }
                    None => {
                        write!(w, "<tr><td><code>{}extern crate {};",
                               VisSpace(&myitem.visibility),
                               HRef::new(myitem.def_id, name))?
                    }
                }
                write!(w, "</code></td></tr>")?;
            }

            clean::ImportItem(ref import) => {
                write!(w, "<tr><td><code>{}{}</code></td></tr>",
                       VisSpace(&myitem.visibility), *import)?;
            }

            _ => {
                if myitem.name.is_none() { continue }

                let stabilities = short_stability(myitem, cx, false);

                let stab_docs = if !stabilities.is_empty() {
                    stabilities.iter()
                               .map(|s| format!("[{}]", s))
                               .collect::<Vec<_>>()
                               .as_slice()
                               .join(" ")
                } else {
                    String::new()
                };

                let unsafety_flag = match myitem.inner {
                    clean::FunctionItem(ref func) | clean::ForeignFunctionItem(ref func)
                    if func.unsafety == hir::Unsafety::Unsafe => {
                        "<a title='unsafe function' href='#'><sup>⚠</sup></a>"
                    }
                    _ => "",
                };

                let doc_value = myitem.doc_value().unwrap_or("");
                write!(w, "
                       <tr class='{stab} module-item'>
                           <td><a class=\"{class}\" href=\"{href}\"
                                  title='{title_type} {title}'>{name}</a>{unsafety_flag}</td>
                           <td class='docblock-short'>
                               {stab_docs} {docs}
                           </td>
                       </tr>",
                       name = *myitem.name.as_ref().unwrap(),
                       stab_docs = stab_docs,
                       docs = if cx.render_type == RenderType::Hoedown {
                           format!("{}",
                                   shorter(Some(&Markdown(doc_value,
                                                          RenderType::Hoedown).to_string())))
                       } else {
                           format!("{}", MarkdownSummaryLine(doc_value))
                       },
                       class = myitem.type_(),
                       stab = myitem.stability_class().unwrap_or("".to_string()),
                       unsafety_flag = unsafety_flag,
                       href = item_path(myitem.type_(), myitem.name.as_ref().unwrap()),
                       title_type = myitem.type_(),
                       title = full_path(cx, myitem))?;
            }
        }
    }

    if curty.is_some() {
        write!(w, "</table>")?;
    }
    Ok(())
}

fn short_stability(item: &clean::Item, cx: &Context, show_reason: bool) -> Vec<String> {
    let mut stability = vec![];

    if let Some(stab) = item.stability.as_ref() {
        let deprecated_reason = if show_reason && !stab.deprecated_reason.is_empty() {
            format!(": {}", stab.deprecated_reason)
        } else {
            String::new()
        };
        if !stab.deprecated_since.is_empty() {
            let since = if show_reason {
                format!(" since {}", Escape(&stab.deprecated_since))
            } else {
                String::new()
            };
            let text = format!("Deprecated{}{}",
                               since,
                               MarkdownHtml(&deprecated_reason, cx.render_type));
            stability.push(format!("<div class='stab deprecated'>{}</div>", text))
        };

        if stab.level == stability::Unstable {
            if show_reason {
                let unstable_extra = match (!stab.feature.is_empty(),
                                            &cx.shared.issue_tracker_base_url,
                                            stab.issue) {
                    (true, &Some(ref tracker_url), Some(issue_no)) if issue_no > 0 =>
                        format!(" (<code>{} </code><a href=\"{}{}\">#{}</a>)",
                                Escape(&stab.feature), tracker_url, issue_no, issue_no),
                    (false, &Some(ref tracker_url), Some(issue_no)) if issue_no > 0 =>
                        format!(" (<a href=\"{}{}\">#{}</a>)", Escape(&tracker_url), issue_no,
                                issue_no),
                    (true, ..) =>
                        format!(" (<code>{}</code>)", Escape(&stab.feature)),
                    _ => String::new(),
                };
                if stab.unstable_reason.is_empty() {
                    stability.push(format!("<div class='stab unstable'>\
                                            <span class=microscope>🔬</span> \
                                            This is a nightly-only experimental API. {}\
                                            </div>",
                                           unstable_extra));
                } else {
                    let text = format!("<summary><span class=microscope>🔬</span> \
                                        This is a nightly-only experimental API. {}\
                                        </summary>{}",
                                       unstable_extra,
                                       MarkdownHtml(&stab.unstable_reason, cx.render_type));
                    stability.push(format!("<div class='stab unstable'><details>{}</details></div>",
                                   text));
                }
            } else {
                stability.push(format!("<div class='stab unstable'>Experimental</div>"))
            }
        };
    } else if let Some(depr) = item.deprecation.as_ref() {
        let note = if show_reason && !depr.note.is_empty() {
            format!(": {}", depr.note)
        } else {
            String::new()
        };
        let since = if show_reason && !depr.since.is_empty() {
            format!(" since {}", Escape(&depr.since))
        } else {
            String::new()
        };

        let text = format!("Deprecated{}{}", since, MarkdownHtml(&note, cx.render_type));
        stability.push(format!("<div class='stab deprecated'>{}</div>", text))
    }

    if let Some(ref cfg) = item.attrs.cfg {
        stability.push(format!("<div class='stab portability'>{}</div>", if show_reason {
            cfg.render_long_html()
        } else {
            cfg.render_short_html()
        }));
    }

    stability
}

struct Initializer<'a>(&'a str);

impl<'a> fmt::Display for Initializer<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let Initializer(s) = *self;
        if s.is_empty() { return Ok(()); }
        write!(f, "<code> = </code>")?;
        write!(f, "<code>{}</code>", Escape(s))
    }
}

fn item_constant(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
                 c: &clean::Constant) -> fmt::Result {
    write!(w, "<pre class='rust const'>")?;
    render_attributes(w, it)?;
    write!(w, "{vis}const \
               {name}: {typ}{init}</pre>",
           vis = VisSpace(&it.visibility),
           name = it.name.as_ref().unwrap(),
           typ = c.type_,
           init = Initializer(&c.expr))?;
    document(w, cx, it)
}

fn item_static(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
               s: &clean::Static) -> fmt::Result {
    write!(w, "<pre class='rust static'>")?;
    render_attributes(w, it)?;
    write!(w, "{vis}static {mutability}\
               {name}: {typ}{init}</pre>",
           vis = VisSpace(&it.visibility),
           mutability = MutableSpace(s.mutability),
           name = it.name.as_ref().unwrap(),
           typ = s.type_,
           init = Initializer(&s.expr))?;
    document(w, cx, it)
}

fn item_function(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
                 f: &clean::Function) -> fmt::Result {
    let name_len = format!("{}{}{}{:#}fn {}{:#}",
                           VisSpace(&it.visibility),
                           ConstnessSpace(f.constness),
                           UnsafetySpace(f.unsafety),
                           AbiSpace(f.abi),
                           it.name.as_ref().unwrap(),
                           f.generics).len();
    write!(w, "{}<pre class='rust fn'>", render_spotlight_traits(it)?)?;
    render_attributes(w, it)?;
    write!(w,
           "{vis}{constness}{unsafety}{abi}fn {name}{generics}{decl}{where_clause}</pre>",
           vis = VisSpace(&it.visibility),
           constness = ConstnessSpace(f.constness),
           unsafety = UnsafetySpace(f.unsafety),
           abi = AbiSpace(f.abi),
           name = it.name.as_ref().unwrap(),
           generics = f.generics,
           where_clause = WhereClause { gens: &f.generics, indent: 0, end_newline: true },
           decl = Method {
              decl: &f.decl,
              name_len,
              indent: 0,
           })?;
    document(w, cx, it)
}

fn implementor2item<'a>(cache: &'a Cache, imp : &Implementor) -> Option<&'a clean::Item> {
    if let Some(t_did) = imp.impl_.for_.def_id() {
        if let Some(impl_item) = cache.impls.get(&t_did).and_then(|i| i.iter()
            .find(|i| i.impl_item.def_id == imp.def_id))
        {
            let i = &impl_item.impl_item;
            return Some(i);
        }
    }
    None
}

fn item_trait(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
              t: &clean::Trait) -> fmt::Result {
    let mut bounds = String::new();
    let mut bounds_plain = String::new();
    if !t.bounds.is_empty() {
        if !bounds.is_empty() {
            bounds.push(' ');
            bounds_plain.push(' ');
        }
        bounds.push_str(": ");
        bounds_plain.push_str(": ");
        for (i, p) in t.bounds.iter().enumerate() {
            if i > 0 {
                bounds.push_str(" + ");
                bounds_plain.push_str(" + ");
            }
            bounds.push_str(&format!("{}", *p));
            bounds_plain.push_str(&format!("{:#}", *p));
        }
    }

    // Output the trait definition
    write!(w, "<pre class='rust trait'>")?;
    render_attributes(w, it)?;
    write!(w, "{}{}trait {}{}{}",
           VisSpace(&it.visibility),
           UnsafetySpace(t.unsafety),
           it.name.as_ref().unwrap(),
           t.generics,
           bounds)?;

    if !t.generics.where_predicates.is_empty() {
        write!(w, "{}", WhereClause { gens: &t.generics, indent: 0, end_newline: true })?;
    } else {
        write!(w, " ")?;
    }

    let types = t.items.iter().filter(|m| m.is_associated_type()).collect::<Vec<_>>();
    let consts = t.items.iter().filter(|m| m.is_associated_const()).collect::<Vec<_>>();
    let required = t.items.iter().filter(|m| m.is_ty_method()).collect::<Vec<_>>();
    let provided = t.items.iter().filter(|m| m.is_method()).collect::<Vec<_>>();

    if t.items.is_empty() {
        write!(w, "{{ }}")?;
    } else {
        // FIXME: we should be using a derived_id for the Anchors here
        write!(w, "{{\n")?;
        for t in &types {
            write!(w, "    ")?;
            render_assoc_item(w, t, AssocItemLink::Anchor(None), ItemType::Trait)?;
            write!(w, ";\n")?;
        }
        if !types.is_empty() && !consts.is_empty() {
            w.write_str("\n")?;
        }
        for t in &consts {
            write!(w, "    ")?;
            render_assoc_item(w, t, AssocItemLink::Anchor(None), ItemType::Trait)?;
            write!(w, ";\n")?;
        }
        if !consts.is_empty() && !required.is_empty() {
            w.write_str("\n")?;
        }
        for (pos, m) in required.iter().enumerate() {
            write!(w, "    ")?;
            render_assoc_item(w, m, AssocItemLink::Anchor(None), ItemType::Trait)?;
            write!(w, ";\n")?;

            if pos < required.len() - 1 {
               write!(w, "<div class='item-spacer'></div>")?;
            }
        }
        if !required.is_empty() && !provided.is_empty() {
            w.write_str("\n")?;
        }
        for (pos, m) in provided.iter().enumerate() {
            write!(w, "    ")?;
            render_assoc_item(w, m, AssocItemLink::Anchor(None), ItemType::Trait)?;
            match m.inner {
                clean::MethodItem(ref inner) if !inner.generics.where_predicates.is_empty() => {
                    write!(w, ",\n    {{ ... }}\n")?;
                },
                _ => {
                    write!(w, " {{ ... }}\n")?;
                },
            }
            if pos < provided.len() - 1 {
               write!(w, "<div class='item-spacer'></div>")?;
            }
        }
        write!(w, "}}")?;
    }
    write!(w, "</pre>")?;

    // Trait documentation
    document(w, cx, it)?;

    fn trait_item(w: &mut fmt::Formatter, cx: &Context, m: &clean::Item, t: &clean::Item)
                  -> fmt::Result {
        let name = m.name.as_ref().unwrap();
        let item_type = m.type_();
        let id = derive_id(format!("{}.{}", item_type, name));
        let ns_id = derive_id(format!("{}.{}", name, item_type.name_space()));
        write!(w, "{extra}<h3 id='{id}' class='method'>\
                   <span id='{ns_id}' class='invisible'><code>",
               extra = render_spotlight_traits(m)?,
               id = id,
               ns_id = ns_id)?;
        render_assoc_item(w, m, AssocItemLink::Anchor(Some(&id)), ItemType::Impl)?;
        write!(w, "</code>")?;
        render_stability_since(w, m, t)?;
        write!(w, "</span></h3>")?;
        document(w, cx, m)?;
        Ok(())
    }

    if !types.is_empty() {
        write!(w, "
            <h2 id='associated-types' class='small-section-header'>
              Associated Types<a href='#associated-types' class='anchor'></a>
            </h2>
            <div class='methods'>
        ")?;
        for t in &types {
            trait_item(w, cx, *t, it)?;
        }
        write!(w, "</div>")?;
    }

    if !consts.is_empty() {
        write!(w, "
            <h2 id='associated-const' class='small-section-header'>
              Associated Constants<a href='#associated-const' class='anchor'></a>
            </h2>
            <div class='methods'>
        ")?;
        for t in &consts {
            trait_item(w, cx, *t, it)?;
        }
        write!(w, "</div>")?;
    }

    // Output the documentation for each function individually
    if !required.is_empty() {
        write!(w, "
            <h2 id='required-methods' class='small-section-header'>
              Required Methods<a href='#required-methods' class='anchor'></a>
            </h2>
            <div class='methods'>
        ")?;
        for m in &required {
            trait_item(w, cx, *m, it)?;
        }
        write!(w, "</div>")?;
    }
    if !provided.is_empty() {
        write!(w, "
            <h2 id='provided-methods' class='small-section-header'>
              Provided Methods<a href='#provided-methods' class='anchor'></a>
            </h2>
            <div class='methods'>
        ")?;
        for m in &provided {
            trait_item(w, cx, *m, it)?;
        }
        write!(w, "</div>")?;
    }

    // If there are methods directly on this trait object, render them here.
    render_assoc_items(w, cx, it, it.def_id, AssocItemRender::All)?;

    let cache = cache();
    let impl_header = "
        <h2 id='implementors' class='small-section-header'>
          Implementors<a href='#implementors' class='anchor'></a>
        </h2>
        <ul class='item-list' id='implementors-list'>
    ";
    if let Some(implementors) = cache.implementors.get(&it.def_id) {
        // The DefId is for the first Type found with that name. The bool is
        // if any Types with the same name but different DefId have been found.
        let mut implementor_dups: FxHashMap<&str, (DefId, bool)> = FxHashMap();
        for implementor in implementors {
            match implementor.impl_.for_ {
                clean::ResolvedPath { ref path, did, is_generic: false, .. } |
                clean::BorrowedRef {
                    type_: box clean::ResolvedPath { ref path, did, is_generic: false, .. },
                    ..
                } => {
                    let &mut (prev_did, ref mut has_duplicates) =
                        implementor_dups.entry(path.last_name()).or_insert((did, false));
                    if prev_did != did {
                        *has_duplicates = true;
                    }
                }
                _ => {}
            }
        }

        let (local, foreign) = implementors.iter()
            .partition::<Vec<_>, _>(|i| i.impl_.for_.def_id()
                                         .map_or(true, |d| cache.paths.contains_key(&d)));

        if !foreign.is_empty() {
            write!(w, "
                <h2 id='foreign-impls' class='small-section-header'>
                  Implementations on Foreign Types<a href='#foreign-impls' class='anchor'></a>
                </h2>
            ")?;

            for implementor in foreign {
                if let Some(i) = implementor2item(&cache, implementor) {
                    let impl_ = Impl { impl_item: i.clone() };
                    let assoc_link = AssocItemLink::GotoSource(
                        i.def_id, &implementor.impl_.provided_trait_methods
                    );
                    render_impl(w, cx, &impl_, assoc_link,
                                RenderMode::Normal, i.stable_since(), false)?;
                }
            }
        }

        write!(w, "{}", impl_header)?;

        for implementor in local {
            write!(w, "<li>")?;
            if let Some(item) = implementor2item(&cache, implementor) {
                if let Some(l) = (Item { cx, item }).src_href() {
                    write!(w, "<div class='out-of-band'>")?;
                    write!(w, "<a class='srclink' href='{}' title='{}'>[src]</a>",
                                l, "goto source code")?;
                    write!(w, "</div>")?;
                }
            }
            write!(w, "<code>")?;
            // If there's already another implementor that has the same abbridged name, use the
            // full path, for example in `std::iter::ExactSizeIterator`
            let use_absolute = match implementor.impl_.for_ {
                clean::ResolvedPath { ref path, is_generic: false, .. } |
                clean::BorrowedRef {
                    type_: box clean::ResolvedPath { ref path, is_generic: false, .. },
                    ..
                } => implementor_dups[path.last_name()].1,
                _ => false,
            };
            fmt_impl_for_trait_page(&implementor.impl_, w, use_absolute)?;
            for it in &implementor.impl_.items {
                if let clean::TypedefItem(ref tydef, _) = it.inner {
                    write!(w, "<span class=\"where fmt-newline\">  ")?;
                    assoc_type(w, it, &vec![], Some(&tydef.type_), AssocItemLink::Anchor(None))?;
                    write!(w, ";</span>")?;
                }
            }
            writeln!(w, "</code></li>")?;
        }
    } else {
        // even without any implementations to write in, we still want the heading and list, so the
        // implementors javascript file pulled in below has somewhere to write the impls into
        write!(w, "{}", impl_header)?;
    }
    write!(w, "</ul>")?;
    write!(w, r#"<script type="text/javascript" async
                         src="{root_path}/implementors/{path}/{ty}.{name}.js">
                 </script>"#,
           root_path = vec![".."; cx.current.len()].join("/"),
           path = if it.def_id.is_local() {
               cx.current.join("/")
           } else {
               let (ref path, _) = cache.external_paths[&it.def_id];
               path[..path.len() - 1].join("/")
           },
           ty = it.type_().css_class(),
           name = *it.name.as_ref().unwrap())?;
    Ok(())
}

fn naive_assoc_href(it: &clean::Item, link: AssocItemLink) -> String {
    use html::item_type::ItemType::*;

    let name = it.name.as_ref().unwrap();
    let ty = match it.type_() {
        Typedef | AssociatedType => AssociatedType,
        s@_ => s,
    };

    let anchor = format!("#{}.{}", ty, name);
    match link {
        AssocItemLink::Anchor(Some(ref id)) => format!("#{}", id),
        AssocItemLink::Anchor(None) => anchor,
        AssocItemLink::GotoSource(did, _) => {
            href(did).map(|p| format!("{}{}", p.0, anchor)).unwrap_or(anchor)
        }
    }
}

fn assoc_const(w: &mut fmt::Formatter,
               it: &clean::Item,
               ty: &clean::Type,
               _default: Option<&String>,
               link: AssocItemLink) -> fmt::Result {
    write!(w, "{}const <a href='{}' class=\"constant\"><b>{}</b></a>: {}",
           VisSpace(&it.visibility),
           naive_assoc_href(it, link),
           it.name.as_ref().unwrap(),
           ty)?;
    Ok(())
}

fn assoc_type<W: fmt::Write>(w: &mut W, it: &clean::Item,
                             bounds: &Vec<clean::TyParamBound>,
                             default: Option<&clean::Type>,
                             link: AssocItemLink) -> fmt::Result {
    write!(w, "type <a href='{}' class=\"type\">{}</a>",
           naive_assoc_href(it, link),
           it.name.as_ref().unwrap())?;
    if !bounds.is_empty() {
        write!(w, ": {}", TyParamBounds(bounds))?
    }
    if let Some(default) = default {
        write!(w, " = {}", default)?;
    }
    Ok(())
}

fn render_stability_since_raw<'a>(w: &mut fmt::Formatter,
                                  ver: Option<&'a str>,
                                  containing_ver: Option<&'a str>) -> fmt::Result {
    if let Some(v) = ver {
        if containing_ver != ver && v.len() > 0 {
            write!(w, "<div class='since' title='Stable since Rust version {0}'>{0}</div>",
                   v)?
        }
    }
    Ok(())
}

fn render_stability_since(w: &mut fmt::Formatter,
                          item: &clean::Item,
                          containing_item: &clean::Item) -> fmt::Result {
    render_stability_since_raw(w, item.stable_since(), containing_item.stable_since())
}

fn render_assoc_item(w: &mut fmt::Formatter,
                     item: &clean::Item,
                     link: AssocItemLink,
                     parent: ItemType) -> fmt::Result {
    fn method(w: &mut fmt::Formatter,
              meth: &clean::Item,
              unsafety: hir::Unsafety,
              constness: hir::Constness,
              abi: abi::Abi,
              g: &clean::Generics,
              d: &clean::FnDecl,
              link: AssocItemLink,
              parent: ItemType)
              -> fmt::Result {
        let name = meth.name.as_ref().unwrap();
        let anchor = format!("#{}.{}", meth.type_(), name);
        let href = match link {
            AssocItemLink::Anchor(Some(ref id)) => format!("#{}", id),
            AssocItemLink::Anchor(None) => anchor,
            AssocItemLink::GotoSource(did, provided_methods) => {
                // We're creating a link from an impl-item to the corresponding
                // trait-item and need to map the anchored type accordingly.
                let ty = if provided_methods.contains(name) {
                    ItemType::Method
                } else {
                    ItemType::TyMethod
                };

                href(did).map(|p| format!("{}#{}.{}", p.0, ty, name)).unwrap_or(anchor)
            }
        };
        let mut head_len = format!("{}{}{}{:#}fn {}{:#}",
                                   VisSpace(&meth.visibility),
                                   ConstnessSpace(constness),
                                   UnsafetySpace(unsafety),
                                   AbiSpace(abi),
                                   name,
                                   *g).len();
        let (indent, end_newline) = if parent == ItemType::Trait {
            head_len += 4;
            (4, false)
        } else {
            (0, true)
        };
        write!(w, "{}{}{}{}fn <a href='{href}' class='fnname'>{name}</a>\
                   {generics}{decl}{where_clause}",
               VisSpace(&meth.visibility),
               ConstnessSpace(constness),
               UnsafetySpace(unsafety),
               AbiSpace(abi),
               href = href,
               name = name,
               generics = *g,
               decl = Method {
                   decl: d,
                   name_len: head_len,
                   indent,
               },
               where_clause = WhereClause {
                   gens: g,
                   indent,
                   end_newline,
               })
    }
    match item.inner {
        clean::StrippedItem(..) => Ok(()),
        clean::TyMethodItem(ref m) => {
            method(w, item, m.unsafety, hir::Constness::NotConst,
                   m.abi, &m.generics, &m.decl, link, parent)
        }
        clean::MethodItem(ref m) => {
            method(w, item, m.unsafety, m.constness,
                   m.abi, &m.generics, &m.decl, link, parent)
        }
        clean::AssociatedConstItem(ref ty, ref default) => {
            assoc_const(w, item, ty, default.as_ref(), link)
        }
        clean::AssociatedTypeItem(ref bounds, ref default) => {
            assoc_type(w, item, bounds, default.as_ref(), link)
        }
        _ => panic!("render_assoc_item called on non-associated-item")
    }
}

fn item_struct(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
               s: &clean::Struct) -> fmt::Result {
    write!(w, "<pre class='rust struct'>")?;
    render_attributes(w, it)?;
    render_struct(w,
                  it,
                  Some(&s.generics),
                  s.struct_type,
                  &s.fields,
                  "",
                  true)?;
    write!(w, "</pre>")?;

    document(w, cx, it)?;
    let mut fields = s.fields.iter().filter_map(|f| {
        match f.inner {
            clean::StructFieldItem(ref ty) => Some((f, ty)),
            _ => None,
        }
    }).peekable();
    if let doctree::Plain = s.struct_type {
        if fields.peek().is_some() {
            write!(w, "<h2 id='fields' class='fields small-section-header'>
                       Fields<a href='#fields' class='anchor'></a></h2>")?;
            for (field, ty) in fields {
                let id = derive_id(format!("{}.{}",
                                           ItemType::StructField,
                                           field.name.as_ref().unwrap()));
                let ns_id = derive_id(format!("{}.{}",
                                              field.name.as_ref().unwrap(),
                                              ItemType::StructField.name_space()));
                write!(w, "<span id=\"{id}\" class=\"{item_type} small-section-header\">
                           <a href=\"#{id}\" class=\"anchor field\"></a>
                           <span id=\"{ns_id}\" class='invisible'>
                           <code>{name}: {ty}</code>
                           </span></span>",
                       item_type = ItemType::StructField,
                       id = id,
                       ns_id = ns_id,
                       name = field.name.as_ref().unwrap(),
                       ty = ty)?;
                if let Some(stability_class) = field.stability_class() {
                    write!(w, "<span class='stab {stab}'></span>",
                        stab = stability_class)?;
                }
                document(w, cx, field)?;
            }
        }
    }
    render_assoc_items(w, cx, it, it.def_id, AssocItemRender::All)
}

fn item_union(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
               s: &clean::Union) -> fmt::Result {
    write!(w, "<pre class='rust union'>")?;
    render_attributes(w, it)?;
    render_union(w,
                 it,
                 Some(&s.generics),
                 &s.fields,
                 "",
                 true)?;
    write!(w, "</pre>")?;

    document(w, cx, it)?;
    let mut fields = s.fields.iter().filter_map(|f| {
        match f.inner {
            clean::StructFieldItem(ref ty) => Some((f, ty)),
            _ => None,
        }
    }).peekable();
    if fields.peek().is_some() {
        write!(w, "<h2 id='fields' class='fields small-section-header'>
                   Fields<a href='#fields' class='anchor'></a></h2>")?;
        for (field, ty) in fields {
            write!(w, "<span id='{shortty}.{name}' class=\"{shortty}\"><code>{name}: {ty}</code>
                       </span>",
                   shortty = ItemType::StructField,
                   name = field.name.as_ref().unwrap(),
                   ty = ty)?;
            if let Some(stability_class) = field.stability_class() {
                write!(w, "<span class='stab {stab}'></span>",
                    stab = stability_class)?;
            }
            document(w, cx, field)?;
        }
    }
    render_assoc_items(w, cx, it, it.def_id, AssocItemRender::All)
}

fn item_enum(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
             e: &clean::Enum) -> fmt::Result {
    write!(w, "<pre class='rust enum'>")?;
    render_attributes(w, it)?;
    write!(w, "{}enum {}{}{}",
           VisSpace(&it.visibility),
           it.name.as_ref().unwrap(),
           e.generics,
           WhereClause { gens: &e.generics, indent: 0, end_newline: true })?;
    if e.variants.is_empty() && !e.variants_stripped {
        write!(w, " {{}}")?;
    } else {
        write!(w, " {{\n")?;
        for v in &e.variants {
            write!(w, "    ")?;
            let name = v.name.as_ref().unwrap();
            match v.inner {
                clean::VariantItem(ref var) => {
                    match var.kind {
                        clean::VariantKind::CLike => write!(w, "{}", name)?,
                        clean::VariantKind::Tuple(ref tys) => {
                            write!(w, "{}(", name)?;
                            for (i, ty) in tys.iter().enumerate() {
                                if i > 0 {
                                    write!(w, ",&nbsp;")?
                                }
                                write!(w, "{}", *ty)?;
                            }
                            write!(w, ")")?;
                        }
                        clean::VariantKind::Struct(ref s) => {
                            render_struct(w,
                                          v,
                                          None,
                                          s.struct_type,
                                          &s.fields,
                                          "    ",
                                          false)?;
                        }
                    }
                }
                _ => unreachable!()
            }
            write!(w, ",\n")?;
        }

        if e.variants_stripped {
            write!(w, "    // some variants omitted\n")?;
        }
        write!(w, "}}")?;
    }
    write!(w, "</pre>")?;

    document(w, cx, it)?;
    if !e.variants.is_empty() {
        write!(w, "<h2 id='variants' class='variants small-section-header'>
                   Variants<a href='#variants' class='anchor'></a></h2>\n")?;
        for variant in &e.variants {
            let id = derive_id(format!("{}.{}",
                                       ItemType::Variant,
                                       variant.name.as_ref().unwrap()));
            let ns_id = derive_id(format!("{}.{}",
                                          variant.name.as_ref().unwrap(),
                                          ItemType::Variant.name_space()));
            write!(w, "<span id=\"{id}\" class=\"variant small-section-header\">\
                       <a href=\"#{id}\" class=\"anchor field\"></a>\
                       <span id='{ns_id}' class='invisible'><code>{name}",
                   id = id,
                   ns_id = ns_id,
                   name = variant.name.as_ref().unwrap())?;
            if let clean::VariantItem(ref var) = variant.inner {
                if let clean::VariantKind::Tuple(ref tys) = var.kind {
                    write!(w, "(")?;
                    for (i, ty) in tys.iter().enumerate() {
                        if i > 0 {
                            write!(w, ",&nbsp;")?;
                        }
                        write!(w, "{}", *ty)?;
                    }
                    write!(w, ")")?;
                }
            }
            write!(w, "</code></span></span>")?;
            document(w, cx, variant)?;

            use clean::{Variant, VariantKind};
            if let clean::VariantItem(Variant {
                kind: VariantKind::Struct(ref s)
            }) = variant.inner {
                let variant_id = derive_id(format!("{}.{}.fields",
                                                   ItemType::Variant,
                                                   variant.name.as_ref().unwrap()));
                write!(w, "<span class='docblock autohide sub-variant' id='{id}'>",
                       id = variant_id)?;
                write!(w, "<h3 class='fields'>Fields of <code>{name}</code></h3>\n
                           <table>", name = variant.name.as_ref().unwrap())?;
                for field in &s.fields {
                    use clean::StructFieldItem;
                    if let StructFieldItem(ref ty) = field.inner {
                        let id = derive_id(format!("variant.{}.field.{}",
                                                   variant.name.as_ref().unwrap(),
                                                   field.name.as_ref().unwrap()));
                        let ns_id = derive_id(format!("{}.{}.{}.{}",
                                                      variant.name.as_ref().unwrap(),
                                                      ItemType::Variant.name_space(),
                                                      field.name.as_ref().unwrap(),
                                                      ItemType::StructField.name_space()));
                        write!(w, "<tr><td \
                                   id='{id}'>\
                                   <span id='{ns_id}' class='invisible'>\
                                   <code>{f}:&nbsp;{t}</code></span></td><td>",
                               id = id,
                               ns_id = ns_id,
                               f = field.name.as_ref().unwrap(),
                               t = *ty)?;
                        document(w, cx, field)?;
                        write!(w, "</td></tr>")?;
                    }
                }
                write!(w, "</table></span>")?;
            }
            render_stability_since(w, variant, it)?;
        }
    }
    render_assoc_items(w, cx, it, it.def_id, AssocItemRender::All)?;
    Ok(())
}

fn render_attribute(attr: &ast::MetaItem) -> Option<String> {
    let name = attr.name();

    if attr.is_word() {
        Some(format!("{}", name))
    } else if let Some(v) = attr.value_str() {
        Some(format!("{} = {:?}", name, v.as_str()))
    } else if let Some(values) = attr.meta_item_list() {
        let display: Vec<_> = values.iter().filter_map(|attr| {
            attr.meta_item().and_then(|mi| render_attribute(mi))
        }).collect();

        if display.len() > 0 {
            Some(format!("{}({})", name, display.join(", ")))
        } else {
            None
        }
    } else {
        None
    }
}

const ATTRIBUTE_WHITELIST: &'static [&'static str] = &[
    "export_name",
    "lang",
    "link_section",
    "must_use",
    "no_mangle",
    "repr",
    "unsafe_destructor_blind_to_params"
];

fn render_attributes(w: &mut fmt::Formatter, it: &clean::Item) -> fmt::Result {
    let mut attrs = String::new();

    for attr in &it.attrs.other_attrs {
        let name = attr.name().unwrap();
        if !ATTRIBUTE_WHITELIST.contains(&&*name.as_str()) {
            continue;
        }
        if let Some(s) = render_attribute(&attr.meta().unwrap()) {
            attrs.push_str(&format!("#[{}]\n", s));
        }
    }
    if attrs.len() > 0 {
        write!(w, "<div class=\"docblock attributes\">{}</div>", &attrs)?;
    }
    Ok(())
}

fn render_struct(w: &mut fmt::Formatter, it: &clean::Item,
                 g: Option<&clean::Generics>,
                 ty: doctree::StructType,
                 fields: &[clean::Item],
                 tab: &str,
                 structhead: bool) -> fmt::Result {
    write!(w, "{}{}{}",
           VisSpace(&it.visibility),
           if structhead {"struct "} else {""},
           it.name.as_ref().unwrap())?;
    if let Some(g) = g {
        write!(w, "{}", g)?
    }
    match ty {
        doctree::Plain => {
            if let Some(g) = g {
                write!(w, "{}", WhereClause { gens: g, indent: 0, end_newline: true })?
            }
            let mut has_visible_fields = false;
            write!(w, " {{")?;
            for field in fields {
                if let clean::StructFieldItem(ref ty) = field.inner {
                    write!(w, "\n{}    {}{}: {},",
                           tab,
                           VisSpace(&field.visibility),
                           field.name.as_ref().unwrap(),
                           *ty)?;
                    has_visible_fields = true;
                }
            }

            if has_visible_fields {
                if it.has_stripped_fields().unwrap() {
                    write!(w, "\n{}    // some fields omitted", tab)?;
                }
                write!(w, "\n{}", tab)?;
            } else if it.has_stripped_fields().unwrap() {
                // If there are no visible fields we can just display
                // `{ /* fields omitted */ }` to save space.
                write!(w, " /* fields omitted */ ")?;
            }
            write!(w, "}}")?;
        }
        doctree::Tuple => {
            write!(w, "(")?;
            for (i, field) in fields.iter().enumerate() {
                if i > 0 {
                    write!(w, ", ")?;
                }
                match field.inner {
                    clean::StrippedItem(box clean::StructFieldItem(..)) => {
                        write!(w, "_")?
                    }
                    clean::StructFieldItem(ref ty) => {
                        write!(w, "{}{}", VisSpace(&field.visibility), *ty)?
                    }
                    _ => unreachable!()
                }
            }
            write!(w, ")")?;
            if let Some(g) = g {
                write!(w, "{}", WhereClause { gens: g, indent: 0, end_newline: false })?
            }
            write!(w, ";")?;
        }
        doctree::Unit => {
            // Needed for PhantomData.
            if let Some(g) = g {
                write!(w, "{}", WhereClause { gens: g, indent: 0, end_newline: false })?
            }
            write!(w, ";")?;
        }
    }
    Ok(())
}

fn render_union(w: &mut fmt::Formatter, it: &clean::Item,
                g: Option<&clean::Generics>,
                fields: &[clean::Item],
                tab: &str,
                structhead: bool) -> fmt::Result {
    write!(w, "{}{}{}",
           VisSpace(&it.visibility),
           if structhead {"union "} else {""},
           it.name.as_ref().unwrap())?;
    if let Some(g) = g {
        write!(w, "{}", g)?;
        write!(w, "{}", WhereClause { gens: g, indent: 0, end_newline: true })?;
    }

    write!(w, " {{\n{}", tab)?;
    for field in fields {
        if let clean::StructFieldItem(ref ty) = field.inner {
            write!(w, "    {}{}: {},\n{}",
                   VisSpace(&field.visibility),
                   field.name.as_ref().unwrap(),
                   *ty,
                   tab)?;
        }
    }

    if it.has_stripped_fields().unwrap() {
        write!(w, "    // some fields omitted\n{}", tab)?;
    }
    write!(w, "}}")?;
    Ok(())
}

#[derive(Copy, Clone)]
enum AssocItemLink<'a> {
    Anchor(Option<&'a str>),
    GotoSource(DefId, &'a FxHashSet<String>),
}

impl<'a> AssocItemLink<'a> {
    fn anchor(&self, id: &'a String) -> Self {
        match *self {
            AssocItemLink::Anchor(_) => { AssocItemLink::Anchor(Some(&id)) },
            ref other => *other,
        }
    }
}

enum AssocItemRender<'a> {
    All,
    DerefFor { trait_: &'a clean::Type, type_: &'a clean::Type, deref_mut_: bool }
}

#[derive(Copy, Clone, PartialEq)]
enum RenderMode {
    Normal,
    ForDeref { mut_: bool },
}

fn render_assoc_items(w: &mut fmt::Formatter,
                      cx: &Context,
                      containing_item: &clean::Item,
                      it: DefId,
                      what: AssocItemRender) -> fmt::Result {
    let c = cache();
    let v = match c.impls.get(&it) {
        Some(v) => v,
        None => return Ok(()),
    };
    let (non_trait, traits): (Vec<_>, _) = v.iter().partition(|i| {
        i.inner_impl().trait_.is_none()
    });
    if !non_trait.is_empty() {
        let render_mode = match what {
            AssocItemRender::All => {
                write!(w, "
                    <h2 id='methods' class='small-section-header'>
                      Methods<a href='#methods' class='anchor'></a>
                    </h2>
                ")?;
                RenderMode::Normal
            }
            AssocItemRender::DerefFor { trait_, type_, deref_mut_ } => {
                write!(w, "
                    <h2 id='deref-methods' class='small-section-header'>
                      Methods from {}&lt;Target = {}&gt;<a href='#deref-methods' class='anchor'></a>
                    </h2>
                ", trait_, type_)?;
                RenderMode::ForDeref { mut_: deref_mut_ }
            }
        };
        for i in &non_trait {
            render_impl(w, cx, i, AssocItemLink::Anchor(None), render_mode,
                        containing_item.stable_since(), true)?;
        }
    }
    if let AssocItemRender::DerefFor { .. } = what {
        return Ok(());
    }
    if !traits.is_empty() {
        let deref_impl = traits.iter().find(|t| {
            t.inner_impl().trait_.def_id() == c.deref_trait_did
        });
        if let Some(impl_) = deref_impl {
            let has_deref_mut = traits.iter().find(|t| {
                t.inner_impl().trait_.def_id() == c.deref_mut_trait_did
            }).is_some();
            render_deref_methods(w, cx, impl_, containing_item, has_deref_mut)?;
        }
        write!(w, "
            <h2 id='implementations' class='small-section-header'>
              Trait Implementations<a href='#implementations' class='anchor'></a>
            </h2>
        ")?;
        for i in &traits {
            let did = i.trait_did().unwrap();
            let assoc_link = AssocItemLink::GotoSource(did, &i.inner_impl().provided_trait_methods);
            render_impl(w, cx, i, assoc_link,
                        RenderMode::Normal, containing_item.stable_since(), true)?;
        }
    }
    Ok(())
}

fn render_deref_methods(w: &mut fmt::Formatter, cx: &Context, impl_: &Impl,
                        container_item: &clean::Item, deref_mut: bool) -> fmt::Result {
    let deref_type = impl_.inner_impl().trait_.as_ref().unwrap();
    let target = impl_.inner_impl().items.iter().filter_map(|item| {
        match item.inner {
            clean::TypedefItem(ref t, true) => Some(&t.type_),
            _ => None,
        }
    }).next().expect("Expected associated type binding");
    let what = AssocItemRender::DerefFor { trait_: deref_type, type_: target,
                                           deref_mut_: deref_mut };
    if let Some(did) = target.def_id() {
        render_assoc_items(w, cx, container_item, did, what)
    } else {
        if let Some(prim) = target.primitive_type() {
            if let Some(&did) = cache().primitive_locations.get(&prim) {
                render_assoc_items(w, cx, container_item, did, what)?;
            }
        }
        Ok(())
    }
}

fn should_render_item(item: &clean::Item, deref_mut_: bool) -> bool {
    let self_type_opt = match item.inner {
        clean::MethodItem(ref method) => method.decl.self_type(),
        clean::TyMethodItem(ref method) => method.decl.self_type(),
        _ => None
    };

    if let Some(self_ty) = self_type_opt {
        let (by_mut_ref, by_box, by_value) = match self_ty {
            SelfTy::SelfBorrowed(_, mutability) |
            SelfTy::SelfExplicit(clean::BorrowedRef { mutability, .. }) => {
                (mutability == Mutability::Mutable, false, false)
            },
            SelfTy::SelfExplicit(clean::ResolvedPath { did, .. }) => {
                (false, Some(did) == cache().owned_box_did, false)
            },
            SelfTy::SelfValue => (false, false, true),
            _ => (false, false, false),
        };

        (deref_mut_ || !by_mut_ref) && !by_box && !by_value
    } else {
        false
    }
}

fn render_spotlight_traits(item: &clean::Item) -> Result<String, fmt::Error> {
    let mut out = String::new();

    match item.inner {
        clean::FunctionItem(clean::Function { ref decl, .. }) |
        clean::TyMethodItem(clean::TyMethod { ref decl, .. }) |
        clean::MethodItem(clean::Method { ref decl, .. }) |
        clean::ForeignFunctionItem(clean::Function { ref decl, .. }) => {
            out = spotlight_decl(decl)?;
        }
        _ => {}
    }

    Ok(out)
}

fn spotlight_decl(decl: &clean::FnDecl) -> Result<String, fmt::Error> {
    let mut out = String::new();
    let mut trait_ = String::new();

    if let Some(did) = decl.output.def_id() {
        let c = cache();
        if let Some(impls) = c.impls.get(&did) {
            for i in impls {
                let impl_ = i.inner_impl();
                if impl_.trait_.def_id().and_then(|d| c.traits.get(&d))
                                        .map_or(false, |t| t.is_spotlight) {
                    if out.is_empty() {
                        out.push_str(
                            &format!("<h3 class=\"important\">Important traits for {}</h3>\
                                      <code class=\"content\">",
                                     impl_.for_));
                        trait_.push_str(&format!("{}", impl_.for_));
                    }

                    //use the "where" class here to make it small
                    out.push_str(&format!("<span class=\"where fmt-newline\">{}</span>", impl_));
                    let t_did = impl_.trait_.def_id().unwrap();
                    for it in &impl_.items {
                        if let clean::TypedefItem(ref tydef, _) = it.inner {
                            out.push_str("<span class=\"where fmt-newline\">    ");
                            assoc_type(&mut out, it, &vec![],
                                       Some(&tydef.type_),
                                       AssocItemLink::GotoSource(t_did, &FxHashSet()))?;
                            out.push_str(";</span>");
                        }
                    }
                }
            }
        }
    }

    if !out.is_empty() {
        out.insert_str(0, &format!("<div class=\"important-traits\"><div class='tooltip'>ⓘ\
                                    <span class='tooltiptext'>Important traits for {}</span></div>\
                                    <div class=\"content hidden\">",
                                   trait_));
        out.push_str("</code></div></div>");
    }

    Ok(out)
}

fn render_impl(w: &mut fmt::Formatter, cx: &Context, i: &Impl, link: AssocItemLink,
               render_mode: RenderMode, outer_version: Option<&str>,
               show_def_docs: bool) -> fmt::Result {
    if render_mode == RenderMode::Normal {
        let id = derive_id(match i.inner_impl().trait_ {
            Some(ref t) => format!("impl-{}", small_url_encode(&format!("{:#}", t))),
            None => "impl".to_string(),
        });
        write!(w, "<h3 id='{}' class='impl'><span class='in-band'><code>{}</code>",
               id, i.inner_impl())?;
        write!(w, "<a href='#{}' class='anchor'></a>", id)?;
        write!(w, "</span><span class='out-of-band'>")?;
        let since = i.impl_item.stability.as_ref().map(|s| &s.since[..]);
        if let Some(l) = (Item { item: &i.impl_item, cx: cx }).src_href() {
            write!(w, "<div class='ghost'></div>")?;
            render_stability_since_raw(w, since, outer_version)?;
            write!(w, "<a class='srclink' href='{}' title='{}'>[src]</a>",
                   l, "goto source code")?;
        } else {
            render_stability_since_raw(w, since, outer_version)?;
        }
        write!(w, "</span>")?;
        write!(w, "</h3>\n")?;
        if let Some(ref dox) = cx.shared.maybe_collapsed_doc_value(&i.impl_item) {
            write!(w, "<div class='docblock'>{}</div>", Markdown(&*dox, cx.render_type))?;
        }
    }

    fn doc_impl_item(w: &mut fmt::Formatter, cx: &Context, item: &clean::Item,
                     link: AssocItemLink, render_mode: RenderMode,
                     is_default_item: bool, outer_version: Option<&str>,
                     trait_: Option<&clean::Trait>, show_def_docs: bool) -> fmt::Result {
        let item_type = item.type_();
        let name = item.name.as_ref().unwrap();

        let render_method_item: bool = match render_mode {
            RenderMode::Normal => true,
            RenderMode::ForDeref { mut_: deref_mut_ } => should_render_item(&item, deref_mut_),
        };

        match item.inner {
            clean::MethodItem(clean::Method { ref decl, .. }) |
            clean::TyMethodItem(clean::TyMethod{ ref decl, .. }) => {
                // Only render when the method is not static or we allow static methods
                if render_method_item {
                    let id = derive_id(format!("{}.{}", item_type, name));
                    let ns_id = derive_id(format!("{}.{}", name, item_type.name_space()));
                    write!(w, "<h4 id='{}' class=\"{}\">", id, item_type)?;
                    write!(w, "{}", spotlight_decl(decl)?)?;
                    write!(w, "<span id='{}' class='invisible'>", ns_id)?;
                    write!(w, "<code>")?;
                    render_assoc_item(w, item, link.anchor(&id), ItemType::Impl)?;
                    write!(w, "</code>")?;
                    if let Some(l) = (Item { cx, item }).src_href() {
                        write!(w, "</span><span class='out-of-band'>")?;
                        write!(w, "<div class='ghost'></div>")?;
                        render_stability_since_raw(w, item.stable_since(), outer_version)?;
                        write!(w, "<a class='srclink' href='{}' title='{}'>[src]</a>",
                               l, "goto source code")?;
                    } else {
                        render_stability_since_raw(w, item.stable_since(), outer_version)?;
                    }
                    write!(w, "</span></h4>\n")?;
                }
            }
            clean::TypedefItem(ref tydef, _) => {
                let id = derive_id(format!("{}.{}", ItemType::AssociatedType, name));
                let ns_id = derive_id(format!("{}.{}", name, item_type.name_space()));
                write!(w, "<h4 id='{}' class=\"{}\">", id, item_type)?;
                write!(w, "<span id='{}' class='invisible'><code>", ns_id)?;
                assoc_type(w, item, &Vec::new(), Some(&tydef.type_), link.anchor(&id))?;
                write!(w, "</code></span></h4>\n")?;
            }
            clean::AssociatedConstItem(ref ty, ref default) => {
                let id = derive_id(format!("{}.{}", item_type, name));
                let ns_id = derive_id(format!("{}.{}", name, item_type.name_space()));
                write!(w, "<h4 id='{}' class=\"{}\">", id, item_type)?;
                write!(w, "<span id='{}' class='invisible'><code>", ns_id)?;
                assoc_const(w, item, ty, default.as_ref(), link.anchor(&id))?;
                write!(w, "</code></span></h4>\n")?;
            }
            clean::AssociatedTypeItem(ref bounds, ref default) => {
                let id = derive_id(format!("{}.{}", item_type, name));
                let ns_id = derive_id(format!("{}.{}", name, item_type.name_space()));
                write!(w, "<h4 id='{}' class=\"{}\">", id, item_type)?;
                write!(w, "<span id='{}' class='invisible'><code>", ns_id)?;
                assoc_type(w, item, bounds, default.as_ref(), link.anchor(&id))?;
                write!(w, "</code></span></h4>\n")?;
            }
            clean::StrippedItem(..) => return Ok(()),
            _ => panic!("can't make docs for trait item with name {:?}", item.name)
        }

        if render_method_item || render_mode == RenderMode::Normal {
            let prefix = render_assoc_const_value(item);

            if !is_default_item {
                if let Some(t) = trait_ {
                    // The trait item may have been stripped so we might not
                    // find any documentation or stability for it.
                    if let Some(it) = t.items.iter().find(|i| i.name == item.name) {
                        // We need the stability of the item from the trait
                        // because impls can't have a stability.
                        document_stability(w, cx, it)?;
                        if item.doc_value().is_some() {
                            document_full(w, item, cx, &prefix)?;
                        } else if show_def_docs {
                            // In case the item isn't documented,
                            // provide short documentation from the trait.
                            document_short(w, it, link, cx, &prefix)?;
                        }
                    }
                } else {
                    document_stability(w, cx, item)?;
                    if show_def_docs {
                        document_full(w, item, cx, &prefix)?;
                    }
                }
            } else {
                document_stability(w, cx, item)?;
                if show_def_docs {
                    document_short(w, item, link, cx, &prefix)?;
                }
            }
        }
        Ok(())
    }

    let traits = &cache().traits;
    let trait_ = i.trait_did().and_then(|did| traits.get(&did));

    if !show_def_docs {
        write!(w, "<span class='docblock autohide'>")?;
    }

    write!(w, "<div class='impl-items'>")?;
    for trait_item in &i.inner_impl().items {
        doc_impl_item(w, cx, trait_item, link, render_mode,
                      false, outer_version, trait_, show_def_docs)?;
    }

    fn render_default_items(w: &mut fmt::Formatter,
                            cx: &Context,
                            t: &clean::Trait,
                            i: &clean::Impl,
                            render_mode: RenderMode,
                            outer_version: Option<&str>,
                            show_def_docs: bool) -> fmt::Result {
        for trait_item in &t.items {
            let n = trait_item.name.clone();
            if i.items.iter().find(|m| m.name == n).is_some() {
                continue;
            }
            let did = i.trait_.as_ref().unwrap().def_id().unwrap();
            let assoc_link = AssocItemLink::GotoSource(did, &i.provided_trait_methods);

            doc_impl_item(w, cx, trait_item, assoc_link, render_mode, true,
                          outer_version, None, show_def_docs)?;
        }
        Ok(())
    }

    // If we've implemented a trait, then also emit documentation for all
    // default items which weren't overridden in the implementation block.
    if let Some(t) = trait_ {
        render_default_items(w, cx, t, &i.inner_impl(),
                             render_mode, outer_version, show_def_docs)?;
    }
    write!(w, "</div>")?;

    if !show_def_docs {
        write!(w, "</span>")?;
    }

    Ok(())
}

fn item_typedef(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
                t: &clean::Typedef) -> fmt::Result {
    write!(w, "<pre class='rust typedef'>")?;
    render_attributes(w, it)?;
    write!(w, "type {}{}{where_clause} = {type_};</pre>",
           it.name.as_ref().unwrap(),
           t.generics,
           where_clause = WhereClause { gens: &t.generics, indent: 0, end_newline: true },
           type_ = t.type_)?;

    document(w, cx, it)?;

    // Render any items associated directly to this alias, as otherwise they
    // won't be visible anywhere in the docs. It would be nice to also show
    // associated items from the aliased type (see discussion in #32077), but
    // we need #14072 to make sense of the generics.
    render_assoc_items(w, cx, it, it.def_id, AssocItemRender::All)
}

fn item_foreign_type(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item) -> fmt::Result {
    writeln!(w, "<pre class='rust foreigntype'>extern {{")?;
    render_attributes(w, it)?;
    write!(
        w,
        "    {}type {};\n}}</pre>",
        VisSpace(&it.visibility),
        it.name.as_ref().unwrap(),
    )?;

    document(w, cx, it)?;

    render_assoc_items(w, cx, it, it.def_id, AssocItemRender::All)
}

impl<'a> fmt::Display for Sidebar<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let cx = self.cx;
        let it = self.item;
        let parentlen = cx.current.len() - if it.is_mod() {1} else {0};
        let mut should_close = false;

        if it.is_struct() || it.is_trait() || it.is_primitive() || it.is_union()
            || it.is_enum() || it.is_mod() || it.is_typedef()
        {
            write!(fmt, "<p class='location'>")?;
            match it.inner {
                clean::StructItem(..) => write!(fmt, "Struct ")?,
                clean::TraitItem(..) => write!(fmt, "Trait ")?,
                clean::PrimitiveItem(..) => write!(fmt, "Primitive Type ")?,
                clean::UnionItem(..) => write!(fmt, "Union ")?,
                clean::EnumItem(..) => write!(fmt, "Enum ")?,
                clean::TypedefItem(..) => write!(fmt, "Type Definition ")?,
                clean::ForeignTypeItem => write!(fmt, "Foreign Type ")?,
                clean::ModuleItem(..) => if it.is_crate() {
                    write!(fmt, "Crate ")?;
                } else {
                    write!(fmt, "Module ")?;
                },
                _ => (),
            }
            write!(fmt, "{}", it.name.as_ref().unwrap())?;
            write!(fmt, "</p>")?;

            if it.is_crate() {
                if let Some(ref version) = cache().crate_version {
                    write!(fmt,
                           "<div class='block version'>\
                            <p>Version {}</p>\
                            </div>",
                           version)?;
                }
            }

            write!(fmt, "<div class=\"sidebar-elems\">")?;
            should_close = true;
            match it.inner {
                clean::StructItem(ref s) => sidebar_struct(fmt, it, s)?,
                clean::TraitItem(ref t) => sidebar_trait(fmt, it, t)?,
                clean::PrimitiveItem(ref p) => sidebar_primitive(fmt, it, p)?,
                clean::UnionItem(ref u) => sidebar_union(fmt, it, u)?,
                clean::EnumItem(ref e) => sidebar_enum(fmt, it, e)?,
                clean::TypedefItem(ref t, _) => sidebar_typedef(fmt, it, t)?,
                clean::ModuleItem(ref m) => sidebar_module(fmt, it, &m.items)?,
                clean::ForeignTypeItem => sidebar_foreign_type(fmt, it)?,
                _ => (),
            }
        }

        // The sidebar is designed to display sibling functions, modules and
        // other miscellaneous information. since there are lots of sibling
        // items (and that causes quadratic growth in large modules),
        // we refactor common parts into a shared JavaScript file per module.
        // still, we don't move everything into JS because we want to preserve
        // as much HTML as possible in order to allow non-JS-enabled browsers
        // to navigate the documentation (though slightly inefficiently).

        write!(fmt, "<p class='location'>")?;
        for (i, name) in cx.current.iter().take(parentlen).enumerate() {
            if i > 0 {
                write!(fmt, "::<wbr>")?;
            }
            write!(fmt, "<a href='{}index.html'>{}</a>",
                   &cx.root_path()[..(cx.current.len() - i - 1) * 3],
                   *name)?;
        }
        write!(fmt, "</p>")?;

        // Sidebar refers to the enclosing module, not this module.
        let relpath = if it.is_mod() { "../" } else { "" };
        write!(fmt,
               "<script>window.sidebarCurrent = {{\
                   name: '{name}', \
                   ty: '{ty}', \
                   relpath: '{path}'\
                }};</script>",
               name = it.name.as_ref().map(|x| &x[..]).unwrap_or(""),
               ty = it.type_().css_class(),
               path = relpath)?;
        if parentlen == 0 {
            // There is no sidebar-items.js beyond the crate root path
            // FIXME maybe dynamic crate loading can be merged here
        } else {
            write!(fmt, "<script defer src=\"{path}sidebar-items.js\"></script>",
                   path = relpath)?;
        }
        if should_close {
            // Closes sidebar-elems div.
            write!(fmt, "</div>")?;
        }

        Ok(())
    }
}

fn get_methods(i: &clean::Impl, for_deref: bool) -> Vec<String> {
    i.items.iter().filter_map(|item| {
        match item.name {
            // Maybe check with clean::Visibility::Public as well?
            Some(ref name) if !name.is_empty() && item.visibility.is_some() && item.is_method() => {
                if !for_deref || should_render_item(item, false) {
                    Some(format!("<a href=\"#method.{name}\">{name}</a>", name = name))
                } else {
                    None
                }
            }
            _ => None,
        }
    }).collect::<Vec<_>>()
}

// The point is to url encode any potential character from a type with genericity.
fn small_url_encode(s: &str) -> String {
    s.replace("<", "%3C")
     .replace(">", "%3E")
     .replace(" ", "%20")
     .replace("?", "%3F")
     .replace("'", "%27")
     .replace("&", "%26")
     .replace(",", "%2C")
     .replace(":", "%3A")
     .replace(";", "%3B")
     .replace("[", "%5B")
     .replace("]", "%5D")
     .replace("\"", "%22")
}

fn sidebar_assoc_items(it: &clean::Item) -> String {
    let mut out = String::new();
    let c = cache();
    if let Some(v) = c.impls.get(&it.def_id) {
        let ret = v.iter()
                   .filter(|i| i.inner_impl().trait_.is_none())
                   .flat_map(|i| get_methods(i.inner_impl(), false))
                   .collect::<String>();
        if !ret.is_empty() {
            out.push_str(&format!("<a class=\"sidebar-title\" href=\"#methods\">Methods\
                                   </a><div class=\"sidebar-links\">{}</div>", ret));
        }

        if v.iter().any(|i| i.inner_impl().trait_.is_some()) {
            if let Some(impl_) = v.iter()
                                  .filter(|i| i.inner_impl().trait_.is_some())
                                  .find(|i| i.inner_impl().trait_.def_id() == c.deref_trait_did) {
                if let Some(target) = impl_.inner_impl().items.iter().filter_map(|item| {
                    match item.inner {
                        clean::TypedefItem(ref t, true) => Some(&t.type_),
                        _ => None,
                    }
                }).next() {
                    let inner_impl = target.def_id().or(target.primitive_type().and_then(|prim| {
                        c.primitive_locations.get(&prim).cloned()
                    })).and_then(|did| c.impls.get(&did));
                    if let Some(impls) = inner_impl {
                        out.push_str("<a class=\"sidebar-title\" href=\"#deref-methods\">");
                        out.push_str(&format!("Methods from {:#}&lt;Target={:#}&gt;",
                                              impl_.inner_impl().trait_.as_ref().unwrap(),
                                              target));
                        out.push_str("</a>");
                        let ret = impls.iter()
                                       .filter(|i| i.inner_impl().trait_.is_none())
                                       .flat_map(|i| get_methods(i.inner_impl(), true))
                                       .collect::<String>();
                        out.push_str(&format!("<div class=\"sidebar-links\">{}</div>", ret));
                    }
                }
            }
            let mut links = HashSet::new();
            let ret = v.iter()
                       .filter_map(|i| {
                           let is_negative_impl = is_negative_impl(i.inner_impl());
                           if let Some(ref i) = i.inner_impl().trait_ {
                               let i_display = format!("{:#}", i);
                               let out = Escape(&i_display);
                               let encoded = small_url_encode(&format!("{:#}", i));
                               let generated = format!("<a href=\"#impl-{}\">{}{}</a>",
                                                       encoded,
                                                       if is_negative_impl { "!" } else { "" },
                                                       out);
                               if !links.contains(&generated) && links.insert(generated.clone()) {
                                   Some(generated)
                               } else {
                                   None
                               }
                           } else {
                               None
                           }
                       })
                       .collect::<String>();
            if !ret.is_empty() {
                out.push_str("<a class=\"sidebar-title\" href=\"#implementations\">\
                              Trait Implementations</a>");
                out.push_str(&format!("<div class=\"sidebar-links\">{}</div>", ret));
            }
        }
    }

    out
}

fn sidebar_struct(fmt: &mut fmt::Formatter, it: &clean::Item,
                  s: &clean::Struct) -> fmt::Result {
    let mut sidebar = String::new();
    let fields = get_struct_fields_name(&s.fields);

    if !fields.is_empty() {
        if let doctree::Plain = s.struct_type {
            sidebar.push_str(&format!("<a class=\"sidebar-title\" href=\"#fields\">Fields</a>\
                                       <div class=\"sidebar-links\">{}</div>", fields));
        }
    }

    sidebar.push_str(&sidebar_assoc_items(it));

    if !sidebar.is_empty() {
        write!(fmt, "<div class=\"block items\">{}</div>", sidebar)?;
    }
    Ok(())
}

fn extract_for_impl_name(item: &clean::Item) -> Option<(String, String)> {
    match item.inner {
        clean::ItemEnum::ImplItem(ref i) => {
            if let Some(ref trait_) = i.trait_ {
                Some((format!("{:#}", i.for_), format!("{:#}", trait_)))
            } else {
                None
            }
        },
        _ => None,
    }
}

fn is_negative_impl(i: &clean::Impl) -> bool {
    i.polarity == Some(clean::ImplPolarity::Negative)
}

fn sidebar_trait(fmt: &mut fmt::Formatter, it: &clean::Item,
                 t: &clean::Trait) -> fmt::Result {
    let mut sidebar = String::new();

    let types = t.items
                 .iter()
                 .filter_map(|m| {
                     match m.name {
                         Some(ref name) if m.is_associated_type() => {
                             Some(format!("<a href=\"#associatedtype.{name}\">{name}</a>",
                                          name=name))
                         }
                         _ => None,
                     }
                 })
                 .collect::<String>();
    let consts = t.items
                  .iter()
                  .filter_map(|m| {
                      match m.name {
                          Some(ref name) if m.is_associated_const() => {
                              Some(format!("<a href=\"#associatedconstant.{name}\">{name}</a>",
                                           name=name))
                          }
                          _ => None,
                      }
                  })
                  .collect::<String>();
    let required = t.items
                    .iter()
                    .filter_map(|m| {
                        match m.name {
                            Some(ref name) if m.is_ty_method() => {
                                Some(format!("<a href=\"#tymethod.{name}\">{name}</a>",
                                             name=name))
                            }
                            _ => None,
                        }
                    })
                    .collect::<String>();
    let provided = t.items
                    .iter()
                    .filter_map(|m| {
                        match m.name {
                            Some(ref name) if m.is_method() => {
                                Some(format!("<a href=\"#method.{name}\">{name}</a>", name=name))
                            }
                            _ => None,
                        }
                    })
                    .collect::<String>();

    if !types.is_empty() {
        sidebar.push_str(&format!("<a class=\"sidebar-title\" href=\"#associated-types\">\
                                   Associated Types</a><div class=\"sidebar-links\">{}</div>",
                                  types));
    }
    if !consts.is_empty() {
        sidebar.push_str(&format!("<a class=\"sidebar-title\" href=\"#associated-const\">\
                                   Associated Constants</a><div class=\"sidebar-links\">{}</div>",
                                  consts));
    }
    if !required.is_empty() {
        sidebar.push_str(&format!("<a class=\"sidebar-title\" href=\"#required-methods\">\
                                   Required Methods</a><div class=\"sidebar-links\">{}</div>",
                                  required));
    }
    if !provided.is_empty() {
        sidebar.push_str(&format!("<a class=\"sidebar-title\" href=\"#provided-methods\">\
                                   Provided Methods</a><div class=\"sidebar-links\">{}</div>",
                                  provided));
    }

    let c = cache();

    if let Some(implementors) = c.implementors.get(&it.def_id) {
        let res = implementors.iter()
                              .filter(|i| i.impl_.for_.def_id()
                                                      .map_or(false, |d| !c.paths.contains_key(&d)))
                              .filter_map(|i| {
                                  if let Some(item) = implementor2item(&c, i) {
                                      match extract_for_impl_name(&item) {
                                          Some((ref name, ref url)) => {
                                              Some(format!("<a href=\"#impl-{}\">{}</a>",
                                                           small_url_encode(url),
                                                           Escape(name)))
                                          }
                                          _ => None,
                                      }
                                  } else {
                                      None
                                  }
                              })
                              .collect::<String>();
        if !res.is_empty() {
            sidebar.push_str(&format!("<a class=\"sidebar-title\" href=\"#foreign-impls\">\
                                       Implementations on Foreign Types</a><div \
                                       class=\"sidebar-links\">{}</div>",
                                      res));
        }
    }

    sidebar.push_str("<a class=\"sidebar-title\" href=\"#implementors\">Implementors</a>");

    sidebar.push_str(&sidebar_assoc_items(it));

    write!(fmt, "<div class=\"block items\">{}</div>", sidebar)
}

fn sidebar_primitive(fmt: &mut fmt::Formatter, it: &clean::Item,
                     _p: &clean::PrimitiveType) -> fmt::Result {
    let sidebar = sidebar_assoc_items(it);

    if !sidebar.is_empty() {
        write!(fmt, "<div class=\"block items\">{}</div>", sidebar)?;
    }
    Ok(())
}

fn sidebar_typedef(fmt: &mut fmt::Formatter, it: &clean::Item,
                   _t: &clean::Typedef) -> fmt::Result {
    let sidebar = sidebar_assoc_items(it);

    if !sidebar.is_empty() {
        write!(fmt, "<div class=\"block items\">{}</div>", sidebar)?;
    }
    Ok(())
}

fn get_struct_fields_name(fields: &[clean::Item]) -> String {
    fields.iter()
          .filter(|f| if let clean::StructFieldItem(..) = f.inner {
              true
          } else {
              false
          })
          .filter_map(|f| match f.name {
              Some(ref name) => Some(format!("<a href=\"#structfield.{name}\">\
                                              {name}</a>", name=name)),
              _ => None,
          })
          .collect()
}

fn sidebar_union(fmt: &mut fmt::Formatter, it: &clean::Item,
                 u: &clean::Union) -> fmt::Result {
    let mut sidebar = String::new();
    let fields = get_struct_fields_name(&u.fields);

    if !fields.is_empty() {
        sidebar.push_str(&format!("<a class=\"sidebar-title\" href=\"#fields\">Fields</a>\
                                   <div class=\"sidebar-links\">{}</div>", fields));
    }

    sidebar.push_str(&sidebar_assoc_items(it));

    if !sidebar.is_empty() {
        write!(fmt, "<div class=\"block items\">{}</div>", sidebar)?;
    }
    Ok(())
}

fn sidebar_enum(fmt: &mut fmt::Formatter, it: &clean::Item,
                e: &clean::Enum) -> fmt::Result {
    let mut sidebar = String::new();

    let variants = e.variants.iter()
                             .filter_map(|v| match v.name {
                                 Some(ref name) => Some(format!("<a href=\"#variant.{name}\">{name}\
                                                                 </a>", name = name)),
                                 _ => None,
                             })
                             .collect::<String>();
    if !variants.is_empty() {
        sidebar.push_str(&format!("<a class=\"sidebar-title\" href=\"#variants\">Variants</a>\
                                   <div class=\"sidebar-links\">{}</div>", variants));
    }

    sidebar.push_str(&sidebar_assoc_items(it));

    if !sidebar.is_empty() {
        write!(fmt, "<div class=\"block items\">{}</div>", sidebar)?;
    }
    Ok(())
}

fn sidebar_module(fmt: &mut fmt::Formatter, _it: &clean::Item,
                  items: &[clean::Item]) -> fmt::Result {
    let mut sidebar = String::new();

    if items.iter().any(|it| it.type_() == ItemType::ExternCrate ||
                             it.type_() == ItemType::Import) {
        sidebar.push_str(&format!("<li><a href=\"#{id}\">{name}</a></li>",
                                  id = "reexports",
                                  name = "Reexports"));
    }

    // ordering taken from item_module, reorder, where it prioritized elements in a certain order
    // to print its headings
    for &myty in &[ItemType::Primitive, ItemType::Module, ItemType::Macro, ItemType::Struct,
                   ItemType::Enum, ItemType::Constant, ItemType::Static, ItemType::Trait,
                   ItemType::Function, ItemType::Typedef, ItemType::Union, ItemType::Impl,
                   ItemType::TyMethod, ItemType::Method, ItemType::StructField, ItemType::Variant,
                   ItemType::AssociatedType, ItemType::AssociatedConst, ItemType::ForeignType] {
        if items.iter().any(|it| {
            if let clean::AutoImplItem(..) = it.inner {
                false
            } else {
                !it.is_stripped() && it.type_() == myty
            }
        }) {
            let (short, name) = match myty {
                ItemType::ExternCrate |
                ItemType::Import          => ("reexports", "Reexports"),
                ItemType::Module          => ("modules", "Modules"),
                ItemType::Struct          => ("structs", "Structs"),
                ItemType::Union           => ("unions", "Unions"),
                ItemType::Enum            => ("enums", "Enums"),
                ItemType::Function        => ("functions", "Functions"),
                ItemType::Typedef         => ("types", "Type Definitions"),
                ItemType::Static          => ("statics", "Statics"),
                ItemType::Constant        => ("constants", "Constants"),
                ItemType::Trait           => ("traits", "Traits"),
                ItemType::Impl            => ("impls", "Implementations"),
                ItemType::TyMethod        => ("tymethods", "Type Methods"),
                ItemType::Method          => ("methods", "Methods"),
                ItemType::StructField     => ("fields", "Struct Fields"),
                ItemType::Variant         => ("variants", "Variants"),
                ItemType::Macro           => ("macros", "Macros"),
                ItemType::Primitive       => ("primitives", "Primitive Types"),
                ItemType::AssociatedType  => ("associated-types", "Associated Types"),
                ItemType::AssociatedConst => ("associated-consts", "Associated Constants"),
                ItemType::ForeignType     => ("foreign-types", "Foreign Types"),
            };
            sidebar.push_str(&format!("<li><a href=\"#{id}\">{name}</a></li>",
                                      id = short,
                                      name = name));
        }
    }

    if !sidebar.is_empty() {
        write!(fmt, "<div class=\"block items\"><ul>{}</ul></div>", sidebar)?;
    }
    Ok(())
}

fn sidebar_foreign_type(fmt: &mut fmt::Formatter, it: &clean::Item) -> fmt::Result {
    let sidebar = sidebar_assoc_items(it);
    if !sidebar.is_empty() {
        write!(fmt, "<div class=\"block items\">{}</div>", sidebar)?;
    }
    Ok(())
}

impl<'a> fmt::Display for Source<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let Source(s) = *self;
        let lines = s.lines().count();
        let mut cols = 0;
        let mut tmp = lines;
        while tmp > 0 {
            cols += 1;
            tmp /= 10;
        }
        write!(fmt, "<pre class=\"line-numbers\">")?;
        for i in 1..lines + 1 {
            write!(fmt, "<span id=\"{0}\">{0:1$}</span>\n", i, cols)?;
        }
        write!(fmt, "</pre>")?;
        write!(fmt, "{}",
               highlight::render_with_highlighting(s, None, None, None, None))?;
        Ok(())
    }
}

fn item_macro(w: &mut fmt::Formatter, cx: &Context, it: &clean::Item,
              t: &clean::Macro) -> fmt::Result {
    w.write_str(&highlight::render_with_highlighting(&t.source,
                                                     Some("macro"),
                                                     None,
                                                     None,
                                                     None))?;
    document(w, cx, it)
}

fn item_primitive(w: &mut fmt::Formatter, cx: &Context,
                  it: &clean::Item,
                  _p: &clean::PrimitiveType) -> fmt::Result {
    document(w, cx, it)?;
    render_assoc_items(w, cx, it, it.def_id, AssocItemRender::All)
}

const BASIC_KEYWORDS: &'static str = "rust, rustlang, rust-lang";

fn make_item_keywords(it: &clean::Item) -> String {
    format!("{}, {}", BASIC_KEYWORDS, it.name.as_ref().unwrap())
}

fn get_index_search_type(item: &clean::Item) -> Option<IndexItemFunctionType> {
    let decl = match item.inner {
        clean::FunctionItem(ref f) => &f.decl,
        clean::MethodItem(ref m) => &m.decl,
        clean::TyMethodItem(ref m) => &m.decl,
        _ => return None
    };

    let inputs = decl.inputs.values.iter().map(|arg| get_index_type(&arg.type_)).collect();
    let output = match decl.output {
        clean::FunctionRetTy::Return(ref return_type) => Some(get_index_type(return_type)),
        _ => None
    };

    Some(IndexItemFunctionType { inputs: inputs, output: output })
}

fn get_index_type(clean_type: &clean::Type) -> Type {
    let t = Type {
        name: get_index_type_name(clean_type, true).map(|s| s.to_ascii_lowercase()),
        generics: get_generics(clean_type),
    };
    t
}

fn get_index_type_name(clean_type: &clean::Type, accept_generic: bool) -> Option<String> {
    match *clean_type {
        clean::ResolvedPath { ref path, .. } => {
            let segments = &path.segments;
            Some(segments[segments.len() - 1].name.clone())
        }
        clean::Generic(ref s) if accept_generic => Some(s.clone()),
        clean::Primitive(ref p) => Some(format!("{:?}", p)),
        clean::BorrowedRef { ref type_, .. } => get_index_type_name(type_, accept_generic),
        // FIXME: add all from clean::Type.
        _ => None
    }
}

fn get_generics(clean_type: &clean::Type) -> Option<Vec<String>> {
    clean_type.generics()
              .and_then(|types| {
                  let r = types.iter()
                               .filter_map(|t| get_index_type_name(t, false))
                               .map(|s| s.to_ascii_lowercase())
                               .collect::<Vec<_>>();
                  if r.is_empty() {
                      None
                  } else {
                      Some(r)
                  }
              })
}

pub fn cache() -> Arc<Cache> {
    CACHE_KEY.with(|c| c.borrow().clone())
}

#[cfg(test)]
#[test]
fn test_unique_id() {
    let input = ["foo", "examples", "examples", "method.into_iter","examples",
                 "method.into_iter", "foo", "main", "search", "methods",
                 "examples", "method.into_iter", "assoc_type.Item", "assoc_type.Item"];
    let expected = ["foo", "examples", "examples-1", "method.into_iter", "examples-2",
                    "method.into_iter-1", "foo-1", "main-1", "search-1", "methods-1",
                    "examples-3", "method.into_iter-2", "assoc_type.Item", "assoc_type.Item-1"];

    let test = || {
        let actual: Vec<String> = input.iter().map(|s| derive_id(s.to_string())).collect();
        assert_eq!(&actual[..], expected);
    };
    test();
    reset_ids(true);
    test();
}

#[cfg(test)]
#[test]
fn test_name_key() {
    assert_eq!(name_key("0"), ("", 0, 1));
    assert_eq!(name_key("123"), ("", 123, 0));
    assert_eq!(name_key("Fruit"), ("Fruit", 0, 0));
    assert_eq!(name_key("Fruit0"), ("Fruit", 0, 1));
    assert_eq!(name_key("Fruit0000"), ("Fruit", 0, 4));
    assert_eq!(name_key("Fruit01"), ("Fruit", 1, 1));
    assert_eq!(name_key("Fruit10"), ("Fruit", 10, 0));
    assert_eq!(name_key("Fruit123"), ("Fruit", 123, 0));
}

#[cfg(test)]
#[test]
fn test_name_sorting() {
    let names = ["Apple",
                 "Banana",
                 "Fruit", "Fruit0", "Fruit00",
                 "Fruit1", "Fruit01",
                 "Fruit2", "Fruit02",
                 "Fruit20",
                 "Fruit100",
                 "Pear"];
    let mut sorted = names.to_owned();
    sorted.sort_by_key(|&s| name_key(s));
    assert_eq!(names, sorted);
}
