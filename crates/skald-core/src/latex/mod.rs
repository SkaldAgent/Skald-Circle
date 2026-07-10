//! LaTeX → PDF compilation service.
//!
//! Used by the file viewer (`GET /api/file?…&compile-latex=true`) to render
//! `.tex` sources into PDFs on demand. Compilation is delegated to `latexmk`
//! (xelatex engine); results are cached on disk keyed by a short SHA-256 of the
//! source content, so unchanged files are served without recompiling.

pub mod compiler;

pub use compiler::{CompileError, LatexCompiler};
