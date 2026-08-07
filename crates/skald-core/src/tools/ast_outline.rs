use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT,
};
use crate::tools::fs::{self, MemScope};

pub struct AstOutline {
    /// The `shared-memory` (system) pool. `user-memory` resolves per call from the
    /// `ToolContext`; only the shared store is a global singleton captured here.
    shared_pool: Arc<SqlitePool>,
}

impl AstOutline {
    pub fn new(shared_pool: Arc<SqlitePool>) -> Self { Self { shared_pool } }
}

impl Tool for AstOutline {
    fn name(&self) -> &str { "get_ast_outline" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Filesystem }
    fn display_name(&self) -> &str { "Code Outline" }
    fn icon(&self) -> &str { "outline" }

    fn description(&self) -> &str {
        "Start here when you need to understand a source file you don't already know — especially a large one. \
         Returns the file's structural outline: top-level definitions (functions, classes, structs, methods, \
         traits, interfaces, etc.) without their bodies, so you grasp the whole shape at a fraction of the \
         tokens of reading it. \
         Each entry is formatted as 'START-END | <kind>: <name>' where START and END are 1-based line numbers \
         of the full definition — same column format as read_file, so you pass START/END straight to \
         read_file's start_line/end_line to read just the definition you care about. \
         Typical flow: outline first, then read only the ranges you need — far cheaper than reading the whole file. \
         Paths under user-memory/ (private) or shared-memory/ (shared) outline a note from your memory instead of disk. \
         Supported: .rs .py .js .mjs .ts .tsx .go .java .c .h .cpp .cc .hpp .swift .lua .rb .sh .ex .exs \
         .kt .json .toml .yaml .yml .html .css .md .sql"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "Path to the source file. Relative to `~` (your home) — `shared/{name}/…` and `projects/{owner}/{slug}/…` mounts included — or a container-absolute path (e.g. /tmp/x.py)."
                }
            },
            "required": ["path"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        fs::path_arg(args)
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        truncate_label(&format!("outline `{path}`"), MAX_LABEL_SHORT)
    }

    /// Routes `user-memory/…` / `shared-memory/…` to the note store; every other
    /// path is physical and resolves against the caller's workspace (home,
    /// shared folders, projects) or, for a container-only absolute path, their
    /// container — via the shared fs shuttle.
    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let path = fs::path_arg(&args).unwrap_or_default();
        let Some(m) = fs::classify_memory(&path) else {
            return fs::run_physical(self, &ctx.fs, &path, args);
        };
        let pool = match m.scope {
            MemScope::User   => Arc::clone(&ctx.pool),
            MemScope::Shared => Arc::clone(&self.shared_pool),
        };
        let rel = m.rel;

        Box::new(SimpleExecution::new(Box::pin(async move {
            let Some(doc) = crate::db::memory_docs::get(&pool, &rel).await? else {
                anyhow::bail!("No note at {path}");
            };
            Ok(ToolResult::Text(outline_source(&path, &doc.content)?))
        })))
    }

    fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let display = fs::display_path_arg(&args);
        let abs = fs::resolve(path)?;
        let source = std::fs::read_to_string(&abs)
            .with_context(|| format!("Cannot read file: {display}"))?;
        outline_source(display, &source)
    }
}

/// Outlines `source`, dispatching on `display`'s extension. `display` is the
/// agent-visible path used in the header and in errors — never a host path.
fn outline_source(display: &str, source: &str) -> Result<String> {
    let ext = std::path::Path::new(display)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    match ext {
        "rs"                        => outline_rust(display, source),
        "py"                        => outline_ts(display, source, ts_python(), "Python"),
        "js" | "mjs"                => outline_ts(display, source, ts_javascript(), "JavaScript"),
        "ts"                        => outline_ts(display, source, ts_typescript(false), "TypeScript"),
        "tsx"                       => outline_ts(display, source, ts_typescript(true), "TypeScript/TSX"),
        "go"                        => outline_ts(display, source, ts_go(), "Go"),
        "java"                      => outline_ts(display, source, ts_java(), "Java"),
        "c" | "h"                   => outline_ts(display, source, ts_c(), "C"),
        "cpp" | "cc" | "hpp" | "cxx"=> outline_ts(display, source, ts_cpp(), "C++"),
        "swift"                     => outline_ts(display, source, ts_swift(), "Swift"),
        "lua"                       => outline_ts(display, source, ts_lua(), "Lua"),
        "rb"                        => outline_ts(display, source, ts_ruby(), "Ruby"),
        "sh" | "bash"               => outline_ts(display, source, ts_bash(), "Bash"),
        "ex" | "exs"                => outline_ts(display, source, ts_elixir(), "Elixir"),
        "json"                      => outline_json(display, source),
        "yaml" | "yml"              => outline_ts(display, source, ts_yaml(), "YAML"),
        "html"                      => outline_ts(display, source, ts_html(), "HTML"),
        "css"                       => outline_ts(display, source, ts_css(), "CSS"),
        // text-based fallbacks for crates incompatible with tree-sitter 0.26
        "kt" | "kts"                => outline_kotlin(display, source),
        "toml"                      => outline_toml(display, source),
        "sql"                       => outline_sql(display, source),
        "md" | "markdown"           => outline_markdown(display, source),
        other => Ok(format!(
            "Language not supported for AST outline: .{other}\n\
             Supported: .rs .py .js .ts .tsx .go .java .c .cpp .swift .lua .rb .sh .ex \
             .kt .json .toml .yaml .html .css .md .sql"
        )),
    }
}

// ── tree-sitter helpers ────────────────────────────────────────────────────

struct LangConfig {
    language: tree_sitter::Language,
    def_kinds: &'static [&'static str],
    name_field: &'static str,
    container_kinds: &'static [&'static str],
}

fn outline_ts(display: &str, source: &str, cfg: LangConfig, lang_label: &str) -> Result<String> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&cfg.language)
        .map_err(|e| anyhow::anyhow!("tree-sitter language load error: {e}"))?;

    let tree = parser.parse(source.as_bytes(), None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None for {display}"))?;

    let mut out = format!("--- {lang_label} outline: {display} ---\n\n");
    collect_nodes(tree.root_node(), source, &cfg, 0, &mut out);
    Ok(out)
}

fn collect_nodes(
    node: tree_sitter::Node,
    source: &str,
    cfg: &LangConfig,
    depth: usize,
    out: &mut String,
) {
    let kind = node.kind();

    if cfg.def_kinds.contains(&kind) {
        let start = node.start_position().row + 1;
        let end   = node.end_position().row + 1;
        let name  = extract_name(node, source, cfg.name_field);
        let indent = "  ".repeat(depth);
        out.push_str(&format!("{start:>4}-{end:>4} | {indent}{kind}: {name}\n"));

        for i in 0..node.child_count() {
            let child = node.child(i as u32).unwrap();
            if cfg.container_kinds.contains(&child.kind()) {
                for j in 0..child.child_count() {
                    let inner = child.child(j as u32).unwrap();
                    if cfg.def_kinds.contains(&inner.kind()) {
                        collect_nodes(inner, source, cfg, depth + 1, out);
                    }
                }
            }
        }
        return;
    }

    if depth == 0 {
        for i in 0..node.child_count() {
            collect_nodes(node.child(i as u32).unwrap(), source, cfg, depth, out);
        }
    }
}

/// Extract a display name for a node.
/// 1. Try the named field (e.g. "name", "key").
/// 2. Fall back to node text up to the first `{` or newline, max 120 chars,
///    with whitespace normalised — works for CSS selectors, HTML tags, etc.
fn extract_name(node: tree_sitter::Node, source: &str, name_field: &str) -> String {
    if !name_field.is_empty() {
        if let Some(n) = node.child_by_field_name(name_field) {
            return node_text(n, source);
        }
    }
    let text = source.get(node.byte_range()).unwrap_or("");
    let end = text.find('{')
        .or_else(|| text.find('\n'))
        .unwrap_or(text.len())
        .min(120);
    text[..end].split_whitespace().collect::<Vec<_>>().join(" ")
}

fn node_text(node: tree_sitter::Node, source: &str) -> String {
    source.get(node.byte_range()).unwrap_or("<?>").to_string()
}

// ── language configs ───────────────────────────────────────────────────────

fn ts_python() -> LangConfig {
    LangConfig {
        language: tree_sitter_python::LANGUAGE.into(),
        def_kinds: &["function_definition", "async_function_definition", "class_definition", "decorated_definition"],
        name_field: "name",
        container_kinds: &["block"],
    }
}

fn ts_javascript() -> LangConfig {
    LangConfig {
        language: tree_sitter_javascript::LANGUAGE.into(),
        def_kinds: &[
            "function_declaration", "generator_function_declaration",
            "class_declaration", "method_definition",
            "lexical_declaration", "variable_declaration",
        ],
        name_field: "name",
        container_kinds: &["class_body"],
    }
}

fn ts_typescript(tsx: bool) -> LangConfig {
    let language = if tsx {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    LangConfig {
        language,
        def_kinds: &[
            "function_declaration", "generator_function_declaration",
            "class_declaration", "method_definition",
            "interface_declaration", "type_alias_declaration",
            "enum_declaration", "abstract_class_declaration",
            "lexical_declaration", "variable_declaration",
        ],
        name_field: "name",
        container_kinds: &["class_body"],
    }
}

fn ts_go() -> LangConfig {
    LangConfig {
        language: tree_sitter_go::LANGUAGE.into(),
        def_kinds: &["function_declaration", "method_declaration", "type_declaration", "const_declaration", "var_declaration"],
        name_field: "name",
        container_kinds: &[],
    }
}

fn ts_java() -> LangConfig {
    LangConfig {
        language: tree_sitter_java::LANGUAGE.into(),
        def_kinds: &["class_declaration", "interface_declaration", "enum_declaration", "method_declaration", "constructor_declaration", "annotation_type_declaration"],
        name_field: "name",
        container_kinds: &["class_body", "interface_body", "enum_body"],
    }
}

fn ts_c() -> LangConfig {
    LangConfig {
        language: tree_sitter_c::LANGUAGE.into(),
        def_kinds: &["function_definition", "declaration", "struct_specifier", "enum_specifier", "typedef_declaration"],
        name_field: "declarator",
        container_kinds: &[],
    }
}

fn ts_cpp() -> LangConfig {
    LangConfig {
        language: tree_sitter_cpp::LANGUAGE.into(),
        def_kinds: &["function_definition", "declaration", "class_specifier", "struct_specifier", "enum_specifier", "namespace_definition", "template_declaration"],
        name_field: "name",
        container_kinds: &["field_declaration_list"],
    }
}

fn ts_swift() -> LangConfig {
    LangConfig {
        language: tree_sitter_swift::LANGUAGE.into(),
        def_kinds: &["function_declaration", "class_declaration", "struct_declaration", "protocol_declaration", "enum_declaration", "extension_declaration"],
        name_field: "name",
        container_kinds: &["class_body", "struct_body", "enum_body", "protocol_body"],
    }
}

fn ts_lua() -> LangConfig {
    LangConfig {
        language: tree_sitter_lua::LANGUAGE.into(),
        def_kinds: &["function_declaration", "local_function", "assignment_statement"],
        name_field: "name",
        container_kinds: &[],
    }
}

fn ts_ruby() -> LangConfig {
    LangConfig {
        language: tree_sitter_ruby::LANGUAGE.into(),
        def_kinds: &["method", "singleton_method", "class", "module", "singleton_class"],
        name_field: "name",
        container_kinds: &["body_statement"],
    }
}

fn ts_bash() -> LangConfig {
    LangConfig {
        language: tree_sitter_bash::LANGUAGE.into(),
        def_kinds: &["function_definition"],
        name_field: "name",
        container_kinds: &[],
    }
}

fn ts_elixir() -> LangConfig {
    LangConfig {
        language: tree_sitter_elixir::LANGUAGE.into(),
        def_kinds: &["call"],
        name_field: "target",
        container_kinds: &[],
    }
}

fn ts_yaml() -> LangConfig {
    LangConfig {
        language: tree_sitter_yaml::LANGUAGE.into(),
        def_kinds: &["block_mapping_pair"],
        name_field: "key",
        container_kinds: &[],
    }
}

fn ts_html() -> LangConfig {
    LangConfig {
        language: tree_sitter_html::LANGUAGE.into(),
        def_kinds: &["element"],
        // tag_name is not a named field on element — use text-fallback (first line = opening tag)
        name_field: "",
        // recurse one level: html → head/body children
        container_kinds: &["element"],
    }
}

fn ts_css() -> LangConfig {
    LangConfig {
        language: tree_sitter_css::LANGUAGE.into(),
        def_kinds: &["rule_set", "at_rule"],
        // selectors is not a named field in tree-sitter-css — use text-fallback (text before `{`)
        name_field: "",
        container_kinds: &[],
    }
}

// ── JSON outline (dedicated tree-sitter walker: nested keys) ────────────────
//
// The generic `collect_nodes` only descends through `container_kinds`, which for
// JSON tops out at the first level of the root object (and never enters arrays
// of objects). This walker recurses through the parse tree instead: it lists
// every key at every depth, shows scalar values inline, and expands nested
// objects/arrays. Line ranges keep the read_file contract (`START-END | …`).

const JSON_VALUE_KINDS: &[&str] =
    &["object", "array", "string", "number", "true", "false", "null"];

fn outline_json(display: &str, source: &str) -> Result<String> {
    let mut parser = tree_sitter::Parser::new();
    let language: tree_sitter::Language = tree_sitter_json::LANGUAGE.into();
    parser.set_language(&language)
        .map_err(|e| anyhow::anyhow!("tree-sitter language load error: {e}"))?;
    let tree = parser.parse(source.as_bytes(), None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None for {display}"))?;

    let mut out = format!("--- JSON outline: {display} ---\n\n");
    // document → single top-level value (object or array).
    if let Some(top) = json_first_value(tree.root_node()) {
        json_walk(top, source, 0, &mut out);
    }
    Ok(out)
}

/// First JSON value child of `document` (skips comments/whitespace nodes).
fn json_first_value(document: tree_sitter::Node) -> Option<tree_sitter::Node> {
    for i in 0..document.child_count() {
        let c = document.child(i as u32).unwrap();
        if JSON_VALUE_KINDS.contains(&c.kind()) {
            return Some(c);
        }
    }
    None
}

/// Emit one line per entry of an object/array, recursing into nested containers.
/// Scalars are shown inline; scalar array elements are summarised by the array's
/// header only (not listed) to stay readable on large value arrays.
fn json_walk(node: tree_sitter::Node, source: &str, depth: usize, out: &mut String) {
    const MAX_JSON_DEPTH: usize = 16;
    if depth > MAX_JSON_DEPTH {
        return;
    }
    match node.kind() {
        "object" => {
            for i in 0..node.child_count() {
                let pair = node.child(i as u32).unwrap();
                if pair.kind() != "pair" {
                    continue;
                }
                let (Some(key), Some(val)) = (
                    pair.child_by_field_name("key"),
                    pair.child_by_field_name("value"),
                ) else {
                    continue;
                };
                json_emit(&json_key_text(key, source), val, pair, source, depth, out);
            }
        }
        "array" => {
            let mut idx = 0usize;
            for i in 0..node.child_count() {
                let el = node.child(i as u32).unwrap();
                if !JSON_VALUE_KINDS.contains(&el.kind()) {
                    continue;
                }
                let this = idx;
                idx += 1;
                // Only expand container elements; scalars are covered by the count.
                if el.kind() == "object" || el.kind() == "array" {
                    json_emit(&format!("[{this}]"), el, el, source, depth, out);
                }
            }
        }
        _ => {}
    }
}

/// Emit one entry line (`name: <value-descriptor>`) spanning `span`'s rows,
/// then recurse when the value is itself a container.
fn json_emit(
    name: &str,
    val: tree_sitter::Node,
    span: tree_sitter::Node,
    source: &str,
    depth: usize,
    out: &mut String,
) {
    let start = span.start_position().row + 1;
    let end   = span.end_position().row + 1;
    let indent = "  ".repeat(depth);
    let desc = json_value_desc(val, source);
    out.push_str(&format!("{start:>4}-{end:>4} | {indent}{name}: {desc}\n"));
    if val.kind() == "object" || val.kind() == "array" {
        json_walk(val, source, depth + 1, out);
    }
}

/// Short descriptor of a value: `{N keys}`, `[N items]`, or the scalar literal.
fn json_value_desc(node: tree_sitter::Node, source: &str) -> String {
    match node.kind() {
        "object" => {
            let n = json_count(node, &["pair"]);
            format!("{{{n} {}}}", if n == 1 { "key" } else { "keys" })
        }
        "array" => {
            let n = json_count(node, JSON_VALUE_KINDS);
            format!("[{n} {}]", if n == 1 { "item" } else { "items" })
        }
        _ => {
            let raw = source.get(node.byte_range()).unwrap_or("");
            let one = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            truncate_label(&one, MAX_LABEL_SHORT)
        }
    }
}

/// Number of direct children whose kind is in `kinds`.
fn json_count(node: tree_sitter::Node, kinds: &[&str]) -> usize {
    let mut n = 0;
    for i in 0..node.child_count() {
        if kinds.contains(&node.child(i as u32).unwrap().kind()) {
            n += 1;
        }
    }
    n
}

/// Object key text with the surrounding double-quotes stripped.
fn json_key_text(key: tree_sitter::Node, source: &str) -> String {
    let raw = source.get(key.byte_range()).unwrap_or("");
    raw.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw)
        .to_string()
}

// ── text-based fallbacks (crates incompatible with tree-sitter 0.26) ───────

fn outline_kotlin(display: &str, source: &str) -> Result<String> {
    let mut out = format!("--- Kotlin outline: {display} ---\n\n");
    let re = regex::Regex::new(
        r"(?m)^\s*((?:(?:public|private|protected|internal|open|abstract|override|suspend|inline|data|sealed|companion|object)\s+)*(?:fun|class|object|interface|enum\s+class|data\s+class|sealed\s+class)\s+[\w<>?]+)"
    ).unwrap();
    for cap in re.captures_iter(source) {
        let start = 1 + source[..cap.get(0).unwrap().start()].matches('\n').count();
        let end   = 1 + source[..cap.get(0).unwrap().end()].matches('\n').count();
        out.push_str(&format!("{start:>4}-{end:>4} | {}\n", cap[1].trim()));
    }
    Ok(out)
}

fn outline_toml(display: &str, source: &str) -> Result<String> {
    let mut out = format!("--- TOML outline: {display} ---\n\n");
    for (i, line) in source.lines().enumerate() {
        let t = line.trim();
        if (t.starts_with("[[") && t.ends_with("]]"))
            || (t.starts_with('[') && t.ends_with(']') && !t.starts_with("[["))
        {
            let n = i + 1;
            out.push_str(&format!("{n:>4}-{n:>4} | {t}\n"));
        }
    }
    Ok(out)
}

fn outline_sql(display: &str, source: &str) -> Result<String> {
    let mut out = format!("--- SQL outline: {display} ---\n\n");
    let re = regex::Regex::new(
        r#"(?im)^\s*(CREATE\s+(?:OR\s+REPLACE\s+)?(?:TABLE|VIEW|INDEX|UNIQUE\s+INDEX|FUNCTION|PROCEDURE|TRIGGER|SCHEMA|SEQUENCE|TYPE)\s+(?:IF\s+NOT\s+EXISTS\s+)?[\w."]+)"#
    ).unwrap();
    for cap in re.captures_iter(source) {
        let start = 1 + source[..cap.get(0).unwrap().start()].matches('\n').count();
        let end   = 1 + source[..cap.get(0).unwrap().end()].matches('\n').count();
        out.push_str(&format!("{start:>4}-{end:>4} | {}\n", cap[1].trim()));
    }
    Ok(out)
}

fn outline_markdown(display: &str, source: &str) -> Result<String> {
    let mut out = format!("--- Markdown outline: {display} ---\n\n");
    for (i, line) in source.lines().enumerate() {
        if line.starts_with('#') {
            let n = i + 1;
            out.push_str(&format!("{n:>4}-{n:>4} | {line}\n"));
        }
    }
    Ok(out)
}

// ── Rust outline (syn-based) ───────────────────────────────────────────────

fn outline_rust(display: &str, source: &str) -> Result<String> {
    use syn::{File, Item, ImplItem, TraitItem};
    use syn::spanned::Spanned;

    let file: File = syn::parse_file(source)
        .map_err(|e| anyhow::anyhow!("Parse error in {display}: {e}"))?;

    let mut out = format!("--- Rust outline: {display} ---\n\n");

    for item in &file.items {
        match item {
            Item::Fn(f) => {
                let start = f.sig.fn_token.span().start().line;
                let end   = f.span().end().line;
                let vis = tok(&f.vis);
                let sig = tok(&f.sig);
                out.push_str(&fmt_line(start, end, &format!("{vis}{sig}"), 0));
            }
            Item::Struct(s) => {
                let start = s.struct_token.span().start().line;
                let end   = s.span().end().line;
                let vis = tok(&s.vis);
                let name = &s.ident;
                let generics = tok(&s.generics);
                out.push_str(&fmt_line(start, end, &format!("{vis}struct {name}{generics}"), 0));
            }
            Item::Enum(e) => {
                let start = e.enum_token.span().start().line;
                let end   = e.span().end().line;
                let vis = tok(&e.vis);
                let name = &e.ident;
                let generics = tok(&e.generics);
                out.push_str(&fmt_line(start, end, &format!("{vis}enum {name}{generics}"), 0));
                for v in &e.variants {
                    let vstart = v.ident.span().start().line;
                    let vend   = v.span().end().line;
                    out.push_str(&fmt_line(vstart, vend, &v.ident.to_string(), 1));
                }
            }
            Item::Trait(t) => {
                let start = t.trait_token.span().start().line;
                let end   = t.span().end().line;
                let vis = tok(&t.vis);
                let name = &t.ident;
                let generics = tok(&t.generics);
                out.push_str(&fmt_line(start, end, &format!("{vis}trait {name}{generics}"), 0));
                for item in &t.items {
                    if let TraitItem::Fn(m) = item {
                        let mstart = m.sig.fn_token.span().start().line;
                        let mend   = m.span().end().line;
                        out.push_str(&fmt_line(mstart, mend, &tok(&m.sig), 1));
                    }
                }
            }
            Item::Impl(i) => {
                let start = i.impl_token.span().start().line;
                let end   = i.span().end().line;
                let self_ty = tok(&*i.self_ty);
                let header = if let Some((_, tr, _)) = &i.trait_ {
                    format!("impl {} for {self_ty}", tok(tr))
                } else {
                    format!("impl {self_ty}")
                };
                out.push_str(&fmt_line(start, end, &header, 0));
                for item in &i.items {
                    if let ImplItem::Fn(m) = item {
                        let mstart = m.sig.fn_token.span().start().line;
                        let mend   = m.span().end().line;
                        let vis = tok(&m.vis);
                        let sig = tok(&m.sig);
                        out.push_str(&fmt_line(mstart, mend, &format!("{vis}{sig}"), 1));
                    }
                }
            }
            Item::Type(t) => {
                let start = t.type_token.span().start().line;
                let end   = t.span().end().line;
                let vis = tok(&t.vis);
                let name = &t.ident;
                let ty = tok(&*t.ty);
                out.push_str(&fmt_line(start, end, &format!("{vis}type {name} = {ty}"), 0));
            }
            Item::Const(c) => {
                let start = c.const_token.span().start().line;
                let end   = c.span().end().line;
                let vis = tok(&c.vis);
                let name = &c.ident;
                let ty = tok(&*c.ty);
                out.push_str(&fmt_line(start, end, &format!("{vis}const {name}: {ty}"), 0));
            }
            Item::Mod(m) if m.content.is_some() => {
                let start = m.mod_token.span().start().line;
                let end   = m.span().end().line;
                let vis = tok(&m.vis);
                out.push_str(&fmt_line(start, end, &format!("{vis}mod {}", m.ident), 0));
            }
            _ => {}
        }
    }

    Ok(out)
}

fn tok<T: quote::ToTokens>(node: &T) -> String {
    normalize(node.to_token_stream().to_string())
}

fn normalize(s: String) -> String {
    s.replace(" :: ", "::")
     .replace("& '", "&'")
     .replace(" ' ", "'")
     .replace("< ", "<")
     .replace(" >", ">")
     .replace("( ", "(")
     .replace(" )", ")")
     .replace(", )", ")")
}

fn fmt_line(start: usize, end: usize, s: &str, indent: usize) -> String {
    let prefix = "  ".repeat(indent);
    format!("{start:>4}-{end:>4} | {prefix}{}\n", s.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use serde_json::json;

    use core_api::user_fs::{ProjectMount, UserFs};

    use crate::tools::ExecutionOutcome;

    /// A throwaway owner-schema pool (as `Arc`, ready for a `ToolContext`), plus
    /// its dir for cleanup. `tag` + a counter keep parallel tests off the same file.
    async fn store(tag: &str) -> (Arc<SqlitePool>, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("skald-ast-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::create_user_pool(&dir.join("owner.db"), None).await.unwrap();
        (Arc::new(pool), dir)
    }

    /// Drives the tool through the context-aware path and returns its text result.
    async fn drive(tool: &AstOutline, ctx: &ToolContext, args: Value) -> Result<String, String> {
        match tool.run_with(ctx, args).wait().await {
            ExecutionOutcome::Completed(r) => Ok(r.to_wire()),
            ExecutionOutcome::Failed(e)    => Err(e),
            ExecutionOutcome::Cancelled    => Err("cancelled".into()),
        }
    }

    /// Physical paths resolve against the caller's `UserFs` — home-relative for
    /// `~/…` and bare paths, the project mount for `projects/{owner}/{slug}/…` —
    /// never against the server process cwd. Regression for the single-user
    /// leftover that made `get_ast_outline projects/…/x.py` fail with
    /// "Cannot read file" while every other fs tool worked.
    #[tokio::test]
    async fn outline_routes_home_and_project_paths_through_user_fs() {
        let (shared, sdir) = store("phys-shared").await;
        let (user,   udir) = store("phys-user").await;

        let root = std::env::temp_dir().join(format!("skald-astphys-{}", uuid::Uuid::new_v4()));
        let home = root.join("homes").join("u1");
        let project = root.join("projects").join("owner-id").join("budget");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let py = "def hello(name):\n    return f\"hi {name}\"\n";
        std::fs::write(home.join("x.py"), py).unwrap();
        std::fs::write(project.join("y.py"), py).unwrap();

        let fs = Arc::new(UserFs::new(
            "u1", home.clone(), "skald-u1", PathBuf::from("/root"), vec![],
            vec![ProjectMount {
                owner_username: "alice".into(),
                slug: "budget".into(),
                host: project.clone(),
                container: PathBuf::from("/root/projects/alice/budget"),
                can_write: false,
            }],
            None,
        ));
        let ctx = ToolContext { session_id: 1, user_id: "u1".into(), pool: Arc::clone(&user), fs, mcp: None };
        let tool = AstOutline::new(Arc::clone(&shared));

        // `/homes/u1` only ever appears in the resolved host path, never in the
        // agent namespace — a robust, OS-independent leak detector.
        let leak_marker = "/homes/u1";

        // Home: both the `~/` spelling and a bare relative path.
        for p in ["~/x.py", "x.py"] {
            let out = drive(&tool, &ctx, json!({"path": p})).await.unwrap();
            assert!(out.contains("function_definition: hello"), "{p}: {out}");
            assert!(out.contains("outline: ~/x.py") || out.contains(&format!("outline: {p}")), "{p}: {out}");
            assert!(!out.contains(leak_marker), "host path leaked for {p}: {out}");
        }

        // A project mount the caller belongs to.
        let out = drive(&tool, &ctx, json!({"path": "projects/alice/budget/y.py"})).await.unwrap();
        assert!(out.contains("Python outline: projects/alice/budget/y.py"), "{out}");
        assert!(out.contains("function_definition: hello"), "{out}");
        assert!(!out.contains(leak_marker), "host path leaked: {out}");

        // A project the caller cannot reach is an error, and a missing file
        // names the agent path — never the host path.
        assert!(drive(&tool, &ctx, json!({"path": "projects/bob/budget/y.py"})).await.is_err());
        let err = drive(&tool, &ctx, json!({"path": "~/nope.py"})).await.unwrap_err();
        assert!(err.contains("~/nope.py"), "{err}");
        assert!(!err.contains(leak_marker), "host path leaked in error: {err}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&udir);
        let _ = std::fs::remove_dir_all(&sdir);
    }

    /// Memory paths outline the note from the right store (user vs shared), and
    /// a missing note errors instead of falling through to the disk router.
    #[tokio::test]
    async fn outline_reads_memory_notes_from_the_right_store() {
        let (shared, sdir) = store("mem-shared").await;
        let (user,   udir) = store("mem-user").await;

        crate::db::memory_docs::upsert(&user,   "notes.md", "# Private\n\ntext\n## Sub\n").await.unwrap();
        crate::db::memory_docs::upsert(&shared, "house.md", "# Shared\n").await.unwrap();

        let fs = Arc::new(UserFs::new(
            "u1", PathBuf::from("/tmp"), "skald-u1", PathBuf::from("/root"), vec![], vec![], None,
        ));
        let ctx = ToolContext { session_id: 1, user_id: "u1".into(), pool: Arc::clone(&user), fs, mcp: None };
        let tool = AstOutline::new(Arc::clone(&shared));

        let out = drive(&tool, &ctx, json!({"path": "user-memory/notes.md"})).await.unwrap();
        assert!(out.contains("Markdown outline: user-memory/notes.md"), "{out}");
        assert!(out.contains("# Private") && out.contains("## Sub"), "{out}");

        let out = drive(&tool, &ctx, json!({"path": "shared-memory/house.md"})).await.unwrap();
        assert!(out.contains("# Shared"), "{out}");

        assert!(drive(&tool, &ctx, json!({"path": "user-memory/ghost.md"})).await.is_err());

        let _ = std::fs::remove_dir_all(&udir);
        let _ = std::fs::remove_dir_all(&sdir);
    }
}
