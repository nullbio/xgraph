//! Central language identity types and the grammar-free plugin boundary.
//!
//! This module defines `LanguageId`, the shared `LanguageQueries` shape, and the
//! `LanguagePlugin` trait that language units implement. The trait deliberately
//! omits any tree-sitter dependency so this module can compile without grammar
//! crates; concrete plugins extend their own surface with grammar handles as
//! needed.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    Php,
    Blade,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Go,
    Rust,
}

impl LanguageId {
    fn as_str(self) -> &'static str {
        match self {
            LanguageId::Php => "php",
            LanguageId::Blade => "blade",
            LanguageId::JavaScript => "javascript",
            LanguageId::TypeScript => "typescript",
            LanguageId::Tsx => "tsx",
            LanguageId::Python => "python",
            LanguageId::Go => "go",
            LanguageId::Rust => "rust",
        }
    }
}

impl fmt::Display for LanguageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LanguageQueries {
    pub definitions: &'static str,
    pub imports: &'static str,
    pub exports: &'static str,
    pub types: &'static str,
    pub routes: &'static str,
}

pub trait LanguagePlugin: Send + Sync {
    fn id(&self) -> LanguageId;
    fn extensions(&self) -> &'static [&'static str];
    fn queries(&self) -> &'static LanguageQueries;
    fn tree_sitter_language(&self) -> tree_sitter::Language;
    fn extract(&self, source: &[u8], path: &Path) -> crate::extract::ExtractedFile;
}

pub struct LanguageRegistry {
    plugins: HashMap<LanguageId, Arc<dyn LanguagePlugin>>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
        }
    }

    /// Build a registry populated with every supported language plugin.
    pub fn with_all() -> Self {
        let mut registry = Self::new();
        registry.register(crate::languages::php::PhpPlugin::new());
        registry.register(crate::languages::blade::BladePlugin);
        registry.register(crate::languages::javascript::JavaScriptPlugin);
        registry.register(crate::languages::typescript::TypeScriptPlugin::typescript());
        registry.register(crate::languages::typescript::TypeScriptPlugin::tsx());
        registry.register(crate::languages::python::PythonPlugin);
        registry.register(crate::languages::go::GoPlugin);
        registry.register(crate::languages::rust::RustPlugin);
        registry
    }

    pub fn register<P: LanguagePlugin + 'static>(&mut self, plugin: P) {
        self.plugins.insert(plugin.id(), Arc::new(plugin));
    }

    pub fn get(&self, id: LanguageId) -> Option<&dyn LanguagePlugin> {
        self.plugins.get(&id).map(|p| p.as_ref())
    }

    /// Parse and extract a single file via the registered plugin for its language.
    /// Returns `None` if the path doesn't match any supported language.
    pub fn extract_file(
        &self,
        path: &Path,
        source: &[u8],
    ) -> Option<crate::extract::ExtractedFile> {
        let id = self.detect_by_path(path)?;
        let plugin = self.get(id)?;
        Some(plugin.extract(source, path))
    }

    pub fn detect_by_path(&self, path: &Path) -> Option<LanguageId> {
        detect_language_by_path(path)
    }

    pub fn is_laravel_path(path: &Path) -> bool {
        let mut components = path.components();
        let Some(first) = components.next() else {
            return false;
        };
        let first = first.as_os_str();

        if first == "routes" {
            return components.next().is_some();
        }

        if first == "database" {
            return matches!(components.next(), Some(c) if c.as_os_str() == "migrations")
                && components.next().is_some();
        }

        if first == "app" {
            let Some(second) = components.next() else {
                return false;
            };
            let second = second.as_os_str();
            if second == "Models" {
                return components.next().is_some();
            }
            if second == "Http" {
                return matches!(components.next(), Some(c) if c.as_os_str() == "Controllers")
                    && components.next().is_some();
            }
        }

        false
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn detect_language_by_path(path: &Path) -> Option<LanguageId> {
    let file_name = path.file_name()?.to_str()?;

    if has_suffix_ignore_case(file_name, ".blade.php") {
        return Some(LanguageId::Blade);
    }

    let extension = path.extension()?.to_str()?;
    match extension.to_ascii_lowercase().as_str() {
        "php" => Some(LanguageId::Php),
        "js" | "jsx" | "mjs" | "cjs" => Some(LanguageId::JavaScript),
        "ts" | "mts" | "cts" => Some(LanguageId::TypeScript),
        "tsx" => Some(LanguageId::Tsx),
        "py" | "pyi" => Some(LanguageId::Python),
        "go" => Some(LanguageId::Go),
        "rs" => Some(LanguageId::Rust),
        _ => None,
    }
}

fn has_suffix_ignore_case(name: &str, suffix: &str) -> bool {
    if name.len() < suffix.len() {
        return false;
    }
    let cut = name.len() - suffix.len();
    if !name.is_char_boundary(cut) {
        return false;
    }
    name[cut..].eq_ignore_ascii_case(suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct StubPlugin {
        id: LanguageId,
        extensions: &'static [&'static str],
        queries: &'static LanguageQueries,
    }

    impl LanguagePlugin for StubPlugin {
        fn id(&self) -> LanguageId {
            self.id
        }

        fn extensions(&self) -> &'static [&'static str] {
            self.extensions
        }

        fn queries(&self) -> &'static LanguageQueries {
            self.queries
        }

        fn tree_sitter_language(&self) -> tree_sitter::Language {
            tree_sitter_python::LANGUAGE.into()
        }

        fn extract(&self, _source: &[u8], _path: &Path) -> crate::extract::ExtractedFile {
            crate::extract::ExtractedFile::default()
        }
    }

    static STUB_QUERIES: LanguageQueries = LanguageQueries {
        definitions: "(definitions)",
        imports: "(imports)",
        exports: "(exports)",
        types: "(types)",
        routes: "(routes)",
    };

    fn stub(id: LanguageId, extensions: &'static [&'static str]) -> StubPlugin {
        StubPlugin {
            id,
            extensions,
            queries: &STUB_QUERIES,
        }
    }

    #[test]
    fn detect_by_path_returns_php_for_php_files() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/Foo.php")),
            Some(LanguageId::Php)
        );
    }

    #[test]
    fn detect_by_path_returns_blade_for_blade_php_files() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("resources/views/welcome.blade.php")),
            Some(LanguageId::Blade)
        );
    }

    #[test]
    fn detect_by_path_returns_javascript_for_js_variants() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/index.js")),
            Some(LanguageId::JavaScript)
        );
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/index.mjs")),
            Some(LanguageId::JavaScript)
        );
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/index.cjs")),
            Some(LanguageId::JavaScript)
        );
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/Component.jsx")),
            Some(LanguageId::JavaScript)
        );
    }

    #[test]
    fn detect_by_path_returns_typescript_for_ts_variants() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/index.ts")),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/index.mts")),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/index.cts")),
            Some(LanguageId::TypeScript)
        );
    }

    #[test]
    fn detect_by_path_returns_tsx_for_tsx_files() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/Component.tsx")),
            Some(LanguageId::Tsx)
        );
    }

    #[test]
    fn detect_by_path_returns_python_for_py_files() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("pkg/module.py")),
            Some(LanguageId::Python)
        );
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("pkg/stubs.pyi")),
            Some(LanguageId::Python)
        );
    }

    #[test]
    fn detect_by_path_returns_go_for_go_files() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("cmd/xgraph/main.go")),
            Some(LanguageId::Go)
        );
    }

    #[test]
    fn detect_by_path_returns_rust_for_rs_files() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("src/main.rs")),
            Some(LanguageId::Rust)
        );
    }

    #[test]
    fn blade_php_takes_precedence_over_php() {
        let registry = LanguageRegistry::new();
        let path = PathBuf::from("resources/views/layout.blade.php");
        let detected = registry.detect_by_path(&path);
        assert_eq!(detected, Some(LanguageId::Blade));
        assert_ne!(detected, Some(LanguageId::Php));
    }

    #[test]
    fn detect_by_path_handles_non_ascii_file_names_without_panicking() {
        let registry = LanguageRegistry::new();
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("xéblade.php")),
            Some(LanguageId::Php)
        );
        assert_eq!(
            registry.detect_by_path(&PathBuf::from("résumé.blade.php")),
            Some(LanguageId::Blade)
        );
        assert_eq!(registry.detect_by_path(&PathBuf::from("ünknown.xyz")), None);
    }

    #[test]
    fn detect_by_path_returns_none_for_unknown_extensions() {
        let registry = LanguageRegistry::new();
        assert_eq!(registry.detect_by_path(&PathBuf::from("README.md")), None);
        assert_eq!(registry.detect_by_path(&PathBuf::from("data.json")), None);
        assert_eq!(registry.detect_by_path(&PathBuf::from("Makefile")), None);
    }

    #[test]
    fn is_laravel_path_recognises_routes_files() {
        assert!(LanguageRegistry::is_laravel_path(&PathBuf::from(
            "routes/web.php"
        )));
        assert!(LanguageRegistry::is_laravel_path(&PathBuf::from(
            "routes/api.php"
        )));
    }

    #[test]
    fn is_laravel_path_recognises_http_controllers() {
        assert!(LanguageRegistry::is_laravel_path(&PathBuf::from(
            "app/Http/Controllers/UserController.php"
        )));
        assert!(LanguageRegistry::is_laravel_path(&PathBuf::from(
            "app/Http/Controllers/Admin/DashboardController.php"
        )));
    }

    #[test]
    fn is_laravel_path_recognises_models() {
        assert!(LanguageRegistry::is_laravel_path(&PathBuf::from(
            "app/Models/User.php"
        )));
    }

    #[test]
    fn is_laravel_path_recognises_migrations() {
        assert!(LanguageRegistry::is_laravel_path(&PathBuf::from(
            "database/migrations/2024_01_01_create_users.php"
        )));
    }

    #[test]
    fn is_laravel_path_rejects_unrelated_paths() {
        assert!(!LanguageRegistry::is_laravel_path(&PathBuf::from(
            "src/foo.php"
        )));
        assert!(!LanguageRegistry::is_laravel_path(&PathBuf::from(
            "config/app.php"
        )));
        assert!(!LanguageRegistry::is_laravel_path(&PathBuf::from("routes")));
        assert!(!LanguageRegistry::is_laravel_path(&PathBuf::from(
            "database/migrations"
        )));
        assert!(!LanguageRegistry::is_laravel_path(&PathBuf::from(
            "app/Models"
        )));
        assert!(!LanguageRegistry::is_laravel_path(&PathBuf::from(
            "app/Http/Controllers"
        )));
        assert!(!LanguageRegistry::is_laravel_path(&PathBuf::from(
            "app/Services/Foo.php"
        )));
    }

    #[test]
    fn register_and_get_roundtrip() {
        let mut registry = LanguageRegistry::new();
        registry.register(stub(LanguageId::Php, &["php"]));
        registry.register(stub(LanguageId::Python, &["py"]));

        let php = registry.get(LanguageId::Php).expect("php plugin missing");
        assert_eq!(php.id(), LanguageId::Php);
        assert_eq!(php.extensions(), &["php"]);
        assert_eq!(php.queries().definitions, STUB_QUERIES.definitions);

        let python = registry
            .get(LanguageId::Python)
            .expect("python plugin missing");
        assert_eq!(python.id(), LanguageId::Python);
        assert_eq!(python.extensions(), &["py"]);

        assert!(registry.get(LanguageId::Blade).is_none());
    }

    #[test]
    fn register_replaces_existing_plugin_for_same_id() {
        let mut registry = LanguageRegistry::new();
        registry.register(stub(LanguageId::Php, &["php"]));
        registry.register(stub(LanguageId::Php, &["php", "phtml"]));

        let php = registry.get(LanguageId::Php).expect("php plugin missing");
        assert_eq!(php.extensions(), &["php", "phtml"]);
    }

    #[test]
    fn language_id_display_matches_expected_strings() {
        assert_eq!(LanguageId::Php.to_string(), "php");
        assert_eq!(LanguageId::Blade.to_string(), "blade");
        assert_eq!(LanguageId::JavaScript.to_string(), "javascript");
        assert_eq!(LanguageId::TypeScript.to_string(), "typescript");
        assert_eq!(LanguageId::Tsx.to_string(), "tsx");
        assert_eq!(LanguageId::Python.to_string(), "python");
        assert_eq!(LanguageId::Go.to_string(), "go");
        assert_eq!(LanguageId::Rust.to_string(), "rust");
    }

    #[test]
    fn with_all_registers_every_supported_language() {
        let registry = LanguageRegistry::with_all();
        for id in [
            LanguageId::Php,
            LanguageId::Blade,
            LanguageId::JavaScript,
            LanguageId::TypeScript,
            LanguageId::Tsx,
            LanguageId::Python,
            LanguageId::Go,
            LanguageId::Rust,
        ] {
            assert!(registry.get(id).is_some(), "missing plugin for {id}");
        }
    }

    #[test]
    fn extract_file_dispatches_to_language_plugin() {
        let registry = LanguageRegistry::with_all();
        let extracted = registry
            .extract_file(&PathBuf::from("hello.py"), b"def greet():\n    pass\n")
            .expect("python plugin should match .py path");
        assert!(extracted.nodes.iter().any(|n| n.kind == "function"));
    }

    #[test]
    fn extract_file_returns_none_for_unsupported_extension() {
        let registry = LanguageRegistry::with_all();
        assert!(
            registry
                .extract_file(&PathBuf::from("README.md"), b"# hi\n")
                .is_none()
        );
    }
}
