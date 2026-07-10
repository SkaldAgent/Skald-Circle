use anyhow::Result;
use regex::Regex;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL};
use super::resolve;

pub struct GrepFiles;

impl GrepFiles {
    pub fn new() -> Self { Self }
}

impl Tool for GrepFiles {
    fn name(&self) -> &str { "grep_files" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Search for a regex pattern across files in a directory or a single file. \
         Use instead of grep/rg in the terminal. \
         Binary files and common build/cache directories (target/, .git/, node_modules/, .venv/) are skipped. \
         Use output_mode='files_only' to get just file paths (faster, lower token cost). \
         Use output_mode='count' for match counts per file. \
         Use context_lines to show surrounding lines around each match (like grep -C)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory or file to search. Relative to project root, or absolute."
                },
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for (case-insensitive by default)."
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "If true, match is case-sensitive. Default: false.",
                    "default": false
                },
                "include_glob": {
                    "type": "string",
                    "description": "Restrict search to files matching this glob pattern, e.g. '*.rs' or '*.py'."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_only", "count"],
                    "description": "'content' (default): matching lines with file path and line number. 'files_only': only the paths of files containing at least one match — use when you need to know which files match without reading content. 'count': number of matches per file.",
                    "default": "content"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Lines of context to show before and after each match in content mode (default 0, max 10). Like grep -C.",
                    "default": 0
                },
                "max_results": {
                    "type": "integer",
                    "description": "Stop after this many results (default 100).",
                    "default": 100
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip the first N results for pagination (default 0).",
                    "default": 0
                }
            },
            "required": ["path", "pattern"]
        })
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let pattern = args["pattern"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                truncate_label(&format!("grep_files `{pattern}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                let path = args["path"].as_str().unwrap_or(".");
                truncate_label(&format!("grep_files `{pattern}` in {path}"), MAX_LABEL_FULL)
            }
        }
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path      = args["path"].as_str().ok_or_else(|| anyhow::anyhow!("Missing: path"))?;
        let pattern        = args["pattern"].as_str().ok_or_else(|| anyhow::anyhow!("Missing: pattern"))?;
        let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(false);
        let include_glob   = args["include_glob"].as_str();
        let output_mode    = args["output_mode"].as_str().unwrap_or("content");
        let context_lines  = args["context_lines"].as_u64().unwrap_or(0).min(10) as usize;
        let max_results    = args["max_results"].as_u64().unwrap_or(100) as usize;
        let offset         = args["offset"].as_u64().unwrap_or(0) as usize;

        let re = {
            let pat = if case_sensitive { pattern.to_string() } else { format!("(?i){pattern}") };
            Regex::new(&pat).map_err(|e| anyhow::anyhow!("Invalid regex: {e}"))?
        };
        let glob_pattern = include_glob.and_then(|g| glob::Pattern::new(g).ok());
        let root = resolve(user_path)?;
        if !root.exists() {
            anyhow::bail!("Path not found: {user_path}");
        }

        // Walkers emit absolute paths (the `path` arg is resolved to an absolute working
        // directory upstream). Strip the queried root so results are shown relative to it,
        // consistent with `list_files` — keeps the model from echoing absolute paths back.
        let root_prefix = format!("{}/", root.display());
        let rel = |s: String| s.strip_prefix(&root_prefix).map(str::to_string).unwrap_or(s);

        match output_mode {
            "files_only" => {
                let mut files: Vec<String> = Vec::new();
                collect_matching_files(&root, &re, &glob_pattern, max_results + offset, &mut files)?;
                let files: Vec<String> = files.into_iter().skip(offset).take(max_results).map(rel).collect();
                if files.is_empty() {
                    return Ok(format!("No files match {:?} in {user_path}.", pattern));
                }
                Ok(format!("{} file(s):\n{}", files.len(), files.join("\n")))
            }
            "count" => {
                let mut counts: Vec<(String, usize)> = Vec::new();
                collect_match_counts(&root, &re, &glob_pattern, max_results + offset, &mut counts)?;
                let counts: Vec<(String, usize)> = counts.into_iter().skip(offset).take(max_results).collect();
                if counts.is_empty() {
                    return Ok(format!("No matches for {:?} in {user_path}.", pattern));
                }
                let lines: Vec<String> = counts.into_iter().map(|(f, n)| format!("{}: {n}", rel(f))).collect();
                Ok(format!("{} file(s):\n{}", lines.len(), lines.join("\n")))
            }
            _ => {
                let mut matches: Vec<String> = Vec::new();
                let mut output_bytes: usize = 0;
                let mut truncated = false;
                search_path(&root, &re, &glob_pattern, max_results + offset, context_lines, &mut matches, &mut output_bytes, &mut truncated)?;

                let matches: Vec<String> = matches.into_iter().skip(offset).take(max_results).map(rel).collect();
                if matches.is_empty() {
                    return Ok(format!("No matches for {:?} in {user_path}.", pattern));
                }
                let mut out = format!("{} match(es):\n", matches.len());
                out.push_str(&matches.join("\n"));
                if truncated {
                    out.push_str(&format!(
                        "\n\n[Output truncated at {MAX_OUTPUT_BYTES} bytes. Narrow your search with a more specific pattern, path, or include_glob.]"
                    ));
                }
                Ok(out)
            }
        }
    }
}

// `secrets` is skipped so a recursive grep rooted at a parent (e.g. the auto-read
// working directory) never descends into and leaks secret values.
const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules", ".venv", "__pycache__", "secrets"];
const MAX_FILE_BYTES: u64 = 200_000;
const MAX_OUTPUT_BYTES: usize = 60_000;
const MAX_LINE_BYTES: usize = 500;

// ── files_only mode ───────────────────────────────────────────────────────────

fn collect_matching_files(
    path:  &std::path::Path,
    re:    &Regex,
    glob:  &Option<glob::Pattern>,
    max:   usize,
    out:   &mut Vec<String>,
) -> Result<()> {
    if out.len() >= max { return Ok(()); }
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if out.len() >= max { break; }
            let p = entry.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if SKIP_DIRS.contains(&name) { continue; }
                collect_matching_files(&p, re, glob, max, out)?;
            } else if file_has_match(&p, re, glob)? {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    } else if file_has_match(path, re, glob)? {
        out.push(path.to_string_lossy().into_owned());
    }
    Ok(())
}

fn file_has_match(path: &std::path::Path, re: &Regex, glob: &Option<glob::Pattern>) -> Result<bool> {
    if let Some(pat) = glob {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !pat.matches(name) { return Ok(false); }
    }
    if let Ok(meta) = path.metadata() {
        if meta.len() > MAX_FILE_BYTES { return Ok(false); }
    }
    let text = match std::fs::read(path) { Ok(b) => b, Err(_) => return Ok(false) };
    if text.iter().take(8000).any(|&b| b == 0) { return Ok(false); }
    let content = match std::str::from_utf8(&text) { Ok(s) => s, Err(_) => return Ok(false) };
    Ok(content.lines().any(|l| re.is_match(l)))
}

// ── count mode ────────────────────────────────────────────────────────────────

fn collect_match_counts(
    path:  &std::path::Path,
    re:    &Regex,
    glob:  &Option<glob::Pattern>,
    max:   usize,
    out:   &mut Vec<(String, usize)>,
) -> Result<()> {
    if out.len() >= max { return Ok(()); }
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if out.len() >= max { break; }
            let p = entry.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if SKIP_DIRS.contains(&name) { continue; }
                collect_match_counts(&p, re, glob, max, out)?;
            } else if let Some(n) = count_file_matches(&p, re, glob)? {
                if n > 0 { out.push((p.to_string_lossy().into_owned(), n)); }
            }
        }
    } else if let Some(n) = count_file_matches(path, re, glob)? {
        if n > 0 { out.push((path.to_string_lossy().into_owned(), n)); }
    }
    Ok(())
}

fn count_file_matches(path: &std::path::Path, re: &Regex, glob: &Option<glob::Pattern>) -> Result<Option<usize>> {
    if let Some(pat) = glob {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !pat.matches(name) { return Ok(None); }
    }
    if let Ok(meta) = path.metadata() {
        if meta.len() > MAX_FILE_BYTES { return Ok(None); }
    }
    let text = match std::fs::read(path) { Ok(b) => b, Err(_) => return Ok(None) };
    if text.iter().take(8000).any(|&b| b == 0) { return Ok(None); }
    let content = match std::str::from_utf8(&text) { Ok(s) => s, Err(_) => return Ok(None) };
    Ok(Some(content.lines().filter(|l| re.is_match(l)).count()))
}

// ── content mode ──────────────────────────────────────────────────────────────

fn search_path(
    path:         &std::path::Path,
    re:           &Regex,
    glob:         &Option<glob::Pattern>,
    max_results:  usize,
    context_lines: usize,
    matches:      &mut Vec<String>,
    output_bytes: &mut usize,
    truncated:    &mut bool,
) -> Result<()> {
    if matches.len() >= max_results || *truncated { return Ok(()); }
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if matches.len() >= max_results || *truncated { break; }
            let p = entry.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if SKIP_DIRS.contains(&name) { continue; }
                search_path(&p, re, glob, max_results, context_lines, matches, output_bytes, truncated)?;
            } else {
                grep_file(&p, re, glob, max_results, context_lines, matches, output_bytes, truncated)?;
            }
        }
    } else {
        grep_file(path, re, glob, max_results, context_lines, matches, output_bytes, truncated)?;
    }
    Ok(())
}

fn grep_file(
    path:          &std::path::Path,
    re:            &Regex,
    glob:          &Option<glob::Pattern>,
    max_results:   usize,
    context_lines: usize,
    matches:       &mut Vec<String>,
    output_bytes:  &mut usize,
    truncated:     &mut bool,
) -> Result<()> {
    if let Some(pat) = glob {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !pat.matches(name) { return Ok(()); }
    }
    if let Ok(meta) = path.metadata() {
        if meta.len() > MAX_FILE_BYTES { return Ok(()); }
    }
    let text = match std::fs::read(path) { Ok(b) => b, Err(_) => return Ok(()) };
    if text.iter().take(8000).any(|&b| b == 0) { return Ok(()); }
    let content = match std::str::from_utf8(&text) { Ok(s) => s, Err(_) => return Ok(()) };
    let display = path.to_string_lossy();
    let lines: Vec<&str> = content.lines().collect();

    if context_lines == 0 {
        for (i, line) in lines.iter().enumerate() {
            if matches.len() >= max_results || *truncated { break; }
            if re.is_match(line) {
                let snippet = if line.len() > MAX_LINE_BYTES { format!("{}…", &line[..MAX_LINE_BYTES]) } else { line.to_string() };
                let entry = format!("{}:{}: {}", display, i + 1, snippet);
                *output_bytes += entry.len();
                if *output_bytes > MAX_OUTPUT_BYTES { *truncated = true; break; }
                matches.push(entry);
            }
        }
    } else {
        let match_indices: Vec<usize> = lines.iter().enumerate()
            .filter(|(_, l)| re.is_match(l))
            .map(|(i, _)| i)
            .collect();
        if match_indices.is_empty() { return Ok(()); }

        let mut windows: Vec<(usize, usize)> = Vec::new();
        for &m in &match_indices {
            let start = m.saturating_sub(context_lines);
            let end   = (m + context_lines).min(lines.len().saturating_sub(1));
            if let Some(last) = windows.last_mut() {
                if start <= last.1 + 1 { last.1 = last.1.max(end); continue; }
            }
            windows.push((start, end));
        }

        let match_set: std::collections::HashSet<usize> = match_indices.into_iter().collect();
        for (wi, (start, end)) in windows.iter().enumerate() {
            if matches.len() >= max_results || *truncated { break; }
            if wi > 0 {
                let sep = format!("{}:---", display);
                *output_bytes += sep.len();
                matches.push(sep);
            }
            for idx in *start..=*end {
                if matches.len() >= max_results || *truncated { break; }
                let marker  = if match_set.contains(&idx) { ">" } else { " " };
                let line    = lines[idx];
                let snippet = if line.len() > MAX_LINE_BYTES { format!("{}…", &line[..MAX_LINE_BYTES]) } else { line.to_string() };
                let entry   = format!("{}{}: {}: {}", marker, display, idx + 1, snippet);
                *output_bytes += entry.len();
                if *output_bytes > MAX_OUTPUT_BYTES { *truncated = true; break; }
                matches.push(entry);
            }
        }
    }
    Ok(())
}
