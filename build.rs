use std::path::PathBuf;

fn main() {
    let grammar_dir: PathBuf = ["vendor", "tree-sitter-blade", "src"].iter().collect();
    let parser_c = grammar_dir.join("parser.c");
    let scanner_c = grammar_dir.join("scanner.c");
    let tag_h = grammar_dir.join("tag.h");
    let parser_h: PathBuf = [
        "vendor",
        "tree-sitter-blade",
        "src",
        "tree_sitter",
        "parser.h",
    ]
    .iter()
    .collect();

    println!("cargo:rerun-if-changed={}", parser_c.display());
    println!("cargo:rerun-if-changed={}", scanner_c.display());
    println!("cargo:rerun-if-changed={}", tag_h.display());
    println!("cargo:rerun-if-changed={}", parser_h.display());
    println!("cargo:rerun-if-changed=build.rs");

    cc::Build::new()
        .include(&grammar_dir)
        .file(&parser_c)
        .file(&scanner_c)
        .warnings(false)
        .flag_if_supported("-std=c11")
        .compile("tree-sitter-blade");
}
