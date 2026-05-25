//! Language registry, parser workers, and extraction.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, bounded};
use tree_sitter::{Language, Parser, Query, QueryCursor, QueryError, Tree};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LanguageKey(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryName(pub u32);

#[derive(Debug)]
pub struct ParseRequest {
    pub language: LanguageKey,
    pub bytes: Vec<u8>,
    pub old_tree: Option<Tree>,
}

#[derive(Debug)]
pub struct ParseResult {
    pub tree: Tree,
}

#[derive(Debug)]
pub enum ParseError {
    UnknownLanguage(LanguageKey),
    LanguageInitFailed(LanguageKey),
    ParseReturnedNone,
    WorkerGone,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownLanguage(key) => write!(f, "unknown language key: {}", key.0),
            Self::LanguageInitFailed(key) => {
                write!(f, "failed to initialise parser for language: {}", key.0)
            }
            Self::ParseReturnedNone => f.write_str("tree-sitter parser returned no tree"),
            Self::WorkerGone => f.write_str("parser worker pool has shut down"),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub enum RegisterError {
    InvalidQuery {
        query: QueryName,
        source: QueryError,
    },
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidQuery { query, source } => {
                write!(f, "failed to compile query {}: {}", query.0, source)
            }
        }
    }
}

impl std::error::Error for RegisterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidQuery { source, .. } => Some(source),
        }
    }
}

struct LanguageEntry {
    language: Language,
    queries: HashMap<QueryName, Arc<Query>>,
}

type Registry = Arc<RwLock<HashMap<LanguageKey, Arc<LanguageEntry>>>>;

struct Job {
    request: ParseRequest,
    reply: Sender<Result<ParseResult, ParseError>>,
}

pub struct ParserPool {
    sender: Option<Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
    registry: Registry,
}

impl ParserPool {
    pub fn new() -> Self {
        let workers = num_cpus::get_physical().saturating_sub(1).max(1);
        Self::with_workers(workers)
    }

    pub fn with_workers(n: usize) -> Self {
        let worker_count = n.max(1);
        let capacity = worker_count * 2;
        let (sender, receiver) = bounded::<Job>(capacity);
        let registry: Registry = Arc::new(RwLock::new(HashMap::new()));

        let mut workers = Vec::with_capacity(worker_count);
        for id in 0..worker_count {
            let rx = receiver.clone();
            let reg = Arc::clone(&registry);
            let handle = thread::Builder::new()
                .name(format!("xgraph-parser-{id}"))
                .spawn(move || worker_loop(rx, reg))
                .unwrap_or_else(|_| panic!("failed to spawn parser worker {id}"));
            workers.push(handle);
        }

        Self {
            sender: Some(sender),
            workers,
            registry,
        }
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub fn register_language(
        &self,
        key: LanguageKey,
        language: Language,
        queries: Vec<(QueryName, String)>,
    ) -> Result<(), RegisterError> {
        {
            let read = self
                .registry
                .read()
                .unwrap_or_else(|poison| poison.into_inner());
            if read.contains_key(&key) {
                return Ok(());
            }
        }

        let mut compiled: HashMap<QueryName, Arc<Query>> = HashMap::with_capacity(queries.len());
        for (name, source) in queries {
            let query =
                Query::new(&language, &source).map_err(|source| RegisterError::InvalidQuery {
                    query: name,
                    source,
                })?;
            compiled.insert(name, Arc::new(query));
        }

        let entry = Arc::new(LanguageEntry {
            language,
            queries: compiled,
        });

        let mut write = self
            .registry
            .write()
            .unwrap_or_else(|poison| poison.into_inner());
        write.entry(key).or_insert(entry);
        Ok(())
    }

    pub fn query(&self, language: LanguageKey, query: QueryName) -> Option<Arc<Query>> {
        let read = self
            .registry
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        read.get(&language)
            .and_then(|entry| entry.queries.get(&query).cloned())
    }

    pub fn parse(&self, request: ParseRequest) -> Result<ParseResult, ParseError> {
        let sender = self.sender.as_ref().ok_or(ParseError::WorkerGone)?;
        let (reply_tx, reply_rx) = bounded::<Result<ParseResult, ParseError>>(1);
        let job = Job {
            request,
            reply: reply_tx,
        };
        sender.send(job).map_err(|_| ParseError::WorkerGone)?;
        reply_rx.recv().map_err(|_| ParseError::WorkerGone)?
    }

    pub fn shutdown(mut self) {
        self.shutdown_in_place();
    }

    fn shutdown_in_place(&mut self) {
        drop(self.sender.take());
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Default for ParserPool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ParserPool {
    fn drop(&mut self) {
        self.shutdown_in_place();
    }
}

fn worker_loop(receiver: Receiver<Job>, registry: Registry) {
    let mut parsers: HashMap<LanguageKey, Parser> = HashMap::new();
    let mut _cursor = QueryCursor::new();
    while let Ok(job) = receiver.recv() {
        let result = handle_job(&mut parsers, &registry, &mut _cursor, job.request);
        let _ = job.reply.send(result);
    }
}

fn handle_job(
    parsers: &mut HashMap<LanguageKey, Parser>,
    registry: &Registry,
    _cursor: &mut QueryCursor,
    request: ParseRequest,
) -> Result<ParseResult, ParseError> {
    let parser = parser_for(parsers, registry, request.language)?;
    let tree = parser
        .parse(&request.bytes, request.old_tree.as_ref())
        .ok_or(ParseError::ParseReturnedNone)?;
    Ok(ParseResult { tree })
}

fn parser_for<'a>(
    parsers: &'a mut HashMap<LanguageKey, Parser>,
    registry: &Registry,
    key: LanguageKey,
) -> Result<&'a mut Parser, ParseError> {
    use std::collections::hash_map::Entry;

    match parsers.entry(key) {
        Entry::Occupied(slot) => Ok(slot.into_mut()),
        Entry::Vacant(slot) => {
            let entry = {
                let read = registry.read().unwrap_or_else(|poison| poison.into_inner());
                read.get(&key)
                    .cloned()
                    .ok_or(ParseError::UnknownLanguage(key))?
            };
            let mut parser = Parser::new();
            parser
                .set_language(&entry.language)
                .map_err(|_| ParseError::LanguageInitFailed(key))?;
            Ok(slot.insert(parser))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use super::*;

    const PHP_KEY: LanguageKey = LanguageKey(1);
    const TAGS_QUERY: QueryName = QueryName(1);

    fn php_language() -> Language {
        tree_sitter_php::LANGUAGE_PHP.into()
    }

    fn register_php(pool: &ParserPool) {
        pool.register_language(
            PHP_KEY,
            php_language(),
            vec![(TAGS_QUERY, tree_sitter_php::TAGS_QUERY.to_string())],
        )
        .expect("register php");
    }

    #[test]
    fn parses_php_source() {
        let pool = ParserPool::with_workers(2);
        register_php(&pool);

        let source = b"<?php echo 'hi';".to_vec();
        let result = pool
            .parse(ParseRequest {
                language: PHP_KEY,
                bytes: source,
                old_tree: None,
            })
            .expect("parse");

        let root = result.tree.root_node();
        assert!(!root.has_error(), "tree should parse cleanly");
        assert_eq!(root.kind(), "program");
    }

    #[test]
    fn concurrent_parses_do_not_crash() {
        let pool = Arc::new(ParserPool::with_workers(4));
        register_php(&pool);

        let parse_count = AtomicUsize::new(0);
        let parse_count = Arc::new(parse_count);

        let mut handles = Vec::new();
        for thread_id in 0..8 {
            let pool = Arc::clone(&pool);
            let parse_count = Arc::clone(&parse_count);
            handles.push(thread::spawn(move || {
                for i in 0..16 {
                    let source = format!("<?php $a_{thread_id}_{i} = {i} + 1;").into_bytes();
                    let result = pool
                        .parse(ParseRequest {
                            language: PHP_KEY,
                            bytes: source,
                            old_tree: None,
                        })
                        .expect("parse");
                    assert!(!result.tree.root_node().has_error());
                    parse_count.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }

        assert_eq!(parse_count.load(Ordering::Relaxed), 8 * 16);
    }

    #[test]
    fn register_language_is_idempotent() {
        let pool = ParserPool::with_workers(1);
        register_php(&pool);
        let first = pool.query(PHP_KEY, TAGS_QUERY).expect("query present");

        pool.register_language(
            PHP_KEY,
            php_language(),
            vec![(TAGS_QUERY, "(((".to_string())],
        )
        .expect("second registration is a no-op");

        let second = pool
            .query(PHP_KEY, TAGS_QUERY)
            .expect("query still present");
        assert!(
            Arc::ptr_eq(&first, &second),
            "second registration must not replace the compiled query"
        );
    }

    #[test]
    fn shutdown_joins_workers() {
        let pool = ParserPool::with_workers(3);
        register_php(&pool);

        let _ = pool
            .parse(ParseRequest {
                language: PHP_KEY,
                bytes: b"<?php $x = 1;".to_vec(),
                old_tree: None,
            })
            .expect("parse");

        pool.shutdown();
    }

    #[test]
    fn parse_unknown_language_errors() {
        let pool = ParserPool::with_workers(1);
        let err = pool
            .parse(ParseRequest {
                language: LanguageKey(999),
                bytes: b"<?php".to_vec(),
                old_tree: None,
            })
            .expect_err("unknown language");
        match err {
            ParseError::UnknownLanguage(key) => assert_eq!(key, LanguageKey(999)),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_after_shutdown_returns_worker_gone() {
        let mut pool = ParserPool::with_workers(1);
        pool.shutdown_in_place();
        let err = pool
            .parse(ParseRequest {
                language: PHP_KEY,
                bytes: b"<?php".to_vec(),
                old_tree: None,
            })
            .expect_err("worker gone");
        assert!(matches!(err, ParseError::WorkerGone));
    }
}
