//! Project-aware import resolvers used as a post-extraction pass on PHP, TS
//! and Python files. Language extractors emit canonical `Ref` records with
//! the import source as written (e.g. `'@/utils'`, `'.helpers'`); the owner
//! consults the appropriate resolver to attach a worktree-relative path or
//! package qname that can be matched against other files' definitions.
//!
//! Each resolver is constructed once per `index_all` pass from the worktree
//! root and reused for every file. They're intentionally cheap to build —
//! `TsAliasResolver` reads one file (`tsconfig.json`); `PythonImportResolver`
//! walks the tree once looking for `__init__.py` markers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Resolves TypeScript / JavaScript import strings against `tsconfig.json`
/// `compilerOptions.paths` aliases.
#[derive(Debug, Default)]
pub struct TsAliasResolver {
    base_url: PathBuf,
    aliases: Vec<(String, Vec<String>)>,
}

impl TsAliasResolver {
    /// Build a resolver from the worktree's root `tsconfig.json`. Returns
    /// `None` if no tsconfig exists or it has no `paths` configuration.
    pub fn from_worktree(root: &Path) -> Option<Self> {
        let path = root.join("tsconfig.json");
        let text = std::fs::read_to_string(&path).ok()?;
        // tsconfig.json frequently has comments / trailing commas. We attempt
        // a strict parse; if that fails, strip line comments and retry once.
        let parsed: TsConfig = serde_json::from_str(&text)
            .or_else(|_| serde_json::from_str(&strip_jsonc(&text)))
            .ok()?;
        let opts = parsed.compiler_options?;
        let paths = opts.paths.unwrap_or_default();
        if paths.is_empty() {
            return None;
        }
        let base_url = opts
            .base_url
            .as_deref()
            .map(|s| root.join(s))
            .unwrap_or_else(|| root.to_path_buf());
        Some(Self {
            base_url,
            aliases: paths.into_iter().collect(),
        })
    }

    /// If `import` matches an alias pattern, return the resolved worktree-
    /// relative path string. Otherwise return `None` so callers fall back to
    /// whatever raw module name the extractor emitted.
    pub fn resolve(&self, import: &str, worktree_root: &Path) -> Option<String> {
        for (pattern, targets) in &self.aliases {
            if let Some(captured) = match_pattern(pattern, import)
                && let Some(target) = targets.first()
            {
                let candidate = substitute_target(target, &captured);
                let resolved = self.base_url.join(&candidate);
                if let Ok(rel) = resolved.strip_prefix(worktree_root) {
                    return Some(rel.to_string_lossy().into_owned());
                }
                return Some(resolved.to_string_lossy().into_owned());
            }
        }
        None
    }
}

#[derive(Deserialize)]
struct TsConfig {
    #[serde(rename = "compilerOptions")]
    compiler_options: Option<TsCompilerOptions>,
}

#[derive(Deserialize)]
struct TsCompilerOptions {
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
    paths: Option<BTreeMap<String, Vec<String>>>,
}

/// Strip line-style `// ...` comments and trailing commas from a JSONC-style
/// string. Best-effort fallback for tsconfig parse failures; not a real
/// JSONC parser.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let cleaned = if let Some(idx) = line.find("//") {
            &line[..idx]
        } else {
            line
        };
        out.push_str(cleaned);
        out.push('\n');
    }
    // Drop trailing commas that follow `}` / `]` in arrays/objects.
    let mut filtered = String::with_capacity(out.len());
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b']' || bytes[j] == b'}') {
                i = j;
                continue;
            }
        }
        filtered.push(bytes[i] as char);
        i += 1;
    }
    filtered
}

/// Match an import against a `tsconfig` paths pattern.
/// `pattern` may contain a single `*` wildcard. Returns the captured slice
/// (or empty string for an exact match) if the pattern matched.
fn match_pattern(pattern: &str, input: &str) -> Option<String> {
    if let Some(star_idx) = pattern.find('*') {
        let prefix = &pattern[..star_idx];
        let suffix = &pattern[star_idx + 1..];
        if !input.starts_with(prefix) || !input.ends_with(suffix) {
            return None;
        }
        let captured = &input[prefix.len()..input.len() - suffix.len()];
        Some(captured.to_string())
    } else if pattern == input {
        Some(String::new())
    } else {
        None
    }
}

fn substitute_target(target: &str, captured: &str) -> String {
    target.replace('*', captured)
}

/// Resolves Python import statements (`from .x import y`,
/// `from ..pkg import z`, `from pkg.sub import w`) to a worktree-relative
/// module path. Discovers package boundaries via `__init__.py` files.
#[derive(Debug, Default)]
pub struct PythonImportResolver {
    /// Directories that are Python packages (i.e. contain `__init__.py`).
    /// All paths are worktree-relative.
    packages: BTreeSet<PathBuf>,
}

impl PythonImportResolver {
    pub fn from_worktree(root: &Path) -> Self {
        let mut packages = BTreeSet::new();
        // Walk shallow first — every dir with __init__.py is a package.
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            if entry.file_name() == "__init__.py"
                && let Some(parent) = entry.path().parent()
                && let Ok(rel) = parent.strip_prefix(root)
            {
                packages.insert(rel.to_path_buf());
            }
        }
        Self { packages }
    }

    /// Resolve `import_module` (the raw value from a Python import statement)
    /// given the importing file's worktree-relative path. Returns the
    /// canonical module path (e.g. `"package.sub.module"`) or `None` if the
    /// import can't be located within the worktree.
    pub fn resolve(&self, importing_file: &Path, import_module: &str) -> Option<String> {
        // Count leading dots: 1 = current package, 2 = parent, etc.
        let leading_dots = import_module.bytes().take_while(|&b| b == b'.').count();
        if leading_dots == 0 {
            // Absolute import — only resolve if its head segment matches a
            // known package.
            let head = import_module.split('.').next()?;
            let head_path = PathBuf::from(head);
            if self.packages.contains(&head_path) || self.is_package_or_module(&head_path) {
                Some(import_module.to_string())
            } else {
                None
            }
        } else {
            let rest = &import_module[leading_dots..];
            // The importing file's package is its containing directory iff
            // that directory has an __init__.py. Climb `leading_dots - 1`
            // additional levels for `..` etc.
            let mut base = importing_file.parent()?.to_path_buf();
            for _ in 1..leading_dots {
                base = base.parent()?.to_path_buf();
            }
            if !self.packages.contains(&base) && !base.as_os_str().is_empty() {
                return None;
            }
            let dotted_base = base
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join(".");
            if rest.is_empty() {
                Some(dotted_base)
            } else if dotted_base.is_empty() {
                Some(rest.to_string())
            } else {
                Some(format!("{dotted_base}.{rest}"))
            }
        }
    }

    fn is_package_or_module(&self, head: &Path) -> bool {
        // Heuristic: if any registered package starts with `head`, treat the
        // head as importable; otherwise the import is external (stdlib or
        // third-party) and not resolvable from the worktree.
        self.packages.iter().any(|p| p.starts_with(head))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ts_alias_matches_wildcard_pattern() {
        let resolver = TsAliasResolver {
            base_url: PathBuf::from("/repo"),
            aliases: vec![("@/*".to_string(), vec!["src/*".to_string()])],
        };
        let resolved = resolver.resolve("@/utils/format", Path::new("/repo"));
        assert_eq!(resolved, Some("src/utils/format".to_string()));
    }

    #[test]
    fn ts_alias_matches_exact_pattern() {
        let resolver = TsAliasResolver {
            base_url: PathBuf::from("/repo"),
            aliases: vec![("common".to_string(), vec!["src/common/index".to_string()])],
        };
        assert_eq!(
            resolver.resolve("common", Path::new("/repo")),
            Some("src/common/index".to_string())
        );
        assert_eq!(resolver.resolve("missing", Path::new("/repo")), None);
    }

    #[test]
    fn python_relative_import_climbs_dots() {
        let mut resolver = PythonImportResolver::default();
        resolver.packages.insert(PathBuf::from("pkg"));
        resolver.packages.insert(PathBuf::from("pkg/sub"));
        // `from .helper import X` inside pkg/sub/mod.py
        let resolved = resolver.resolve(Path::new("pkg/sub/mod.py"), ".helper");
        assert_eq!(resolved, Some("pkg.sub.helper".to_string()));
        // `from ..util import X` inside pkg/sub/mod.py
        let resolved = resolver.resolve(Path::new("pkg/sub/mod.py"), "..util");
        assert_eq!(resolved, Some("pkg.util".to_string()));
    }

    #[test]
    fn python_absolute_import_resolves_when_package_known() {
        let mut resolver = PythonImportResolver::default();
        resolver.packages.insert(PathBuf::from("pkg"));
        resolver.packages.insert(PathBuf::from("pkg/sub"));
        let resolved = resolver.resolve(Path::new("other.py"), "pkg.sub.mod");
        assert_eq!(resolved, Some("pkg.sub.mod".to_string()));
        // External (stdlib / third-party) imports return None so callers
        // can leave the raw import name as-is.
        let resolved = resolver.resolve(Path::new("other.py"), "os.path");
        assert_eq!(resolved, None);
    }

    #[test]
    fn strip_jsonc_removes_line_comments_and_trailing_commas() {
        let input = "{\n  \"a\": 1, // inline\n  \"b\": [1, 2,],\n}";
        let cleaned = strip_jsonc(input);
        // Re-parse to confirm validity.
        let _: serde_json::Value = serde_json::from_str(&cleaned).expect("parses after strip");
    }
}
