//! Canonical extraction types shared by every language plugin.
//!
//! Language plugins return `ExtractedFile` from `LanguagePlugin::extract`.
//! Downstream consumers (Cozo writer, hot indexes, resolver passes) consume
//! these shapes without depending on per-language modules.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    pub byte: usize,
    pub row: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: Position,
    pub end: Position,
}

pub type LocalNodeId = u32;
pub type LocalRefId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: LocalNodeId,
    pub kind: String,
    pub name: String,
    pub qname: String,
    pub span: Span,
    pub parent: Option<LocalNodeId>,
}

#[derive(Debug, Clone)]
pub struct Ref {
    pub id: LocalRefId,
    pub kind: String,
    pub name: String,
    pub qname: Option<String>,
    pub alias: Option<String>,
    pub span: Span,
    pub container: Option<LocalNodeId>,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractedFile {
    pub path: PathBuf,
    pub nodes: Vec<Node>,
    pub refs: Vec<Ref>,
    pub diagnostics: Vec<Diagnostic>,
}
