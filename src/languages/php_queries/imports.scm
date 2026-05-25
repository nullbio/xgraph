; PHP `use` imports.
;
; Captures both top-level `use Foo\Bar;` and grouped `use Foo\{Bar, Baz as B};`
; declarations. The extractor reconstructs the fully qualified name from the
; prefix on the `namespace_use_declaration` plus each clause body.

(namespace_use_declaration) @import_declaration
