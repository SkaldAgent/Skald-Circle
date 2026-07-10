use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT};
use crate::core::tools::fs::read_to_string;

pub struct AstOutline;

impl AstOutline {
    pub fn new() -> Self { Self }
}

impl Tool for AstOutline {
    fn name(&self) -> &str { "get_ast_outline" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Return the structural outline of a source file: top-level definitions (functions, classes, \
         structs, methods, traits, interfaces, etc.) without their bodies. \
         Each entry is formatted as 'START-END | <kind>: <name>' where START and END are 1-based \
         line numbers of the full definition — same column format as read_file, so you can pass \
         START/END directly to read_file's start_line/end_line to read just that definition. \
         Much cheaper than reading the full file when you only need to understand the shape of the code. \
         Supported: .rs .py .js .mjs .ts .tsx .go .java .c .h .cpp .cc .hpp .swift .lua .rb .sh .ex .exs \
         .kt .json .toml .yaml .yml .html .css .md .sql"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "Path to the source file. Relative to project root or absolute."
                }
            },
            "required": ["path"]
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        truncate_label(&format!("outline `{path}`"), MAX_LABEL_SHORT)
    }

    fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;

        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        match ext {
            "rs"                        => outline_rust(path),
            "py"                        => outline_ts(path, ts_python(), "Python"),
            "js" | "mjs"                => outline_ts(path, ts_javascript(), "JavaScript"),
            "ts"                        => outline_ts(path, ts_typescript(false), "TypeScript"),
            "tsx"                       => outline_ts(path, ts_typescript(true), "TypeScript/TSX"),
            "go"                        => outline_ts(path, ts_go(), "Go"),
            "java"                      => outline_ts(path, ts_java(), "Java"),
            "c" | "h"                   => outline_ts(path, ts_c(), "C"),
            "cpp" | "cc" | "hpp" | "cxx"=> outline_ts(path, ts_cpp(), "C++"),
            "swift"                     => outline_ts(path, ts_swift(), "Swift"),
            "lua"                       => outline_ts(path, ts_lua(), "Lua"),
            "rb"                        => outline_ts(path, ts_ruby(), "Ruby"),
            "sh" | "bash"               => outline_ts(path, ts_bash(), "Bash"),
            "ex" | "exs"                => outline_ts(path, ts_elixir(), "Elixir"),
            "json"                      => outline_ts(path, ts_json(), "JSON"),
            "yaml" | "yml"              => outline_ts(path, ts_yaml(), "YAML"),
            "html"                      => outline_ts(path, ts_html(), "HTML"),
            "css"                       => outline_ts(path, ts_css(), "CSS"),
            // text-based fallbacks for crates incompatible with tree-sitter 0.26
            "kt" | "kts"                => outline_kotlin(path),
            "toml"                      => outline_toml(path),
            "sql"                       => outline_sql(path),
            "md" | "markdown"           => outline_markdown(path),
            other => Ok(format!(
                "Language not supported for AST outline: .{other}\n\
                 Supported: .rs .py .js .ts .tsx .go .java .c .cpp .swift .lua .rb .sh .ex \
                 .kt .json .toml .yaml .html .css .md .sql"
            )),
        }
    }
}

// ── tree-sitter helpers ────────────────────────────────────────────────────

struct LangConfig {
    language: tree_sitter::Language,
    def_kinds: &'static [&'static str],
    name_field: &'static str,
    container_kinds: &'static [&'static str],
}

fn outline_ts(path: &str, cfg: LangConfig, lang_label: &str) -> Result<String> {
    let source = read_to_string(path)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&cfg.language)
        .map_err(|e| anyhow::anyhow!("tree-sitter language load error: {e}"))?;

    let tree = parser.parse(source.as_bytes(), None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None for {path}"))?;

    let mut out = format!("--- {lang_label} outline: {path} ---\n\n");
    collect_nodes(tree.root_node(), &source, &cfg, 0, &mut out);
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

fn ts_json() -> LangConfig {
    LangConfig {
        language: tree_sitter_json::LANGUAGE.into(),
        def_kinds: &["pair"],
        name_field: "key",
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

// ── text-based fallbacks (crates incompatible with tree-sitter 0.26) ───────

fn outline_kotlin(path: &str) -> Result<String> {
    let source = read_to_string(path)?;
    let mut out = format!("--- Kotlin outline: {path} ---\n\n");
    let re = regex::Regex::new(
        r"(?m)^\s*((?:(?:public|private|protected|internal|open|abstract|override|suspend|inline|data|sealed|companion|object)\s+)*(?:fun|class|object|interface|enum\s+class|data\s+class|sealed\s+class)\s+[\w<>?]+)"
    ).unwrap();
    for cap in re.captures_iter(&source) {
        let start = 1 + source[..cap.get(0).unwrap().start()].matches('\n').count();
        let end   = 1 + source[..cap.get(0).unwrap().end()].matches('\n').count();
        out.push_str(&format!("{start:>4}-{end:>4} | {}\n", cap[1].trim()));
    }
    Ok(out)
}

fn outline_toml(path: &str) -> Result<String> {
    let source = read_to_string(path)?;
    let mut out = format!("--- TOML outline: {path} ---\n\n");
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

fn outline_sql(path: &str) -> Result<String> {
    let source = read_to_string(path)?;
    let mut out = format!("--- SQL outline: {path} ---\n\n");
    let re = regex::Regex::new(
        r#"(?im)^\s*(CREATE\s+(?:OR\s+REPLACE\s+)?(?:TABLE|VIEW|INDEX|UNIQUE\s+INDEX|FUNCTION|PROCEDURE|TRIGGER|SCHEMA|SEQUENCE|TYPE)\s+(?:IF\s+NOT\s+EXISTS\s+)?[\w."]+)"#
    ).unwrap();
    for cap in re.captures_iter(&source) {
        let start = 1 + source[..cap.get(0).unwrap().start()].matches('\n').count();
        let end   = 1 + source[..cap.get(0).unwrap().end()].matches('\n').count();
        out.push_str(&format!("{start:>4}-{end:>4} | {}\n", cap[1].trim()));
    }
    Ok(out)
}

fn outline_markdown(path: &str) -> Result<String> {
    let source = read_to_string(path)?;
    let mut out = format!("--- Markdown outline: {path} ---\n\n");
    for (i, line) in source.lines().enumerate() {
        if line.starts_with('#') {
            let n = i + 1;
            out.push_str(&format!("{n:>4}-{n:>4} | {line}\n"));
        }
    }
    Ok(out)
}

// ── Rust outline (syn-based) ───────────────────────────────────────────────

fn outline_rust(path: &str) -> Result<String> {
    use syn::{File, Item, ImplItem, TraitItem};
    use syn::spanned::Spanned;

    let content = read_to_string(path)?;
    let file: File = syn::parse_file(&content)
        .map_err(|e| anyhow::anyhow!("Parse error in {path}: {e}"))?;

    let mut out = format!("--- Rust outline: {path} ---\n\n");

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
