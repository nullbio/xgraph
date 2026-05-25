; PHP definition captures.
;
; Each pattern binds a stable @kind capture (so the extractor switches on the
; capture name rather than the pattern index) plus a @name capture for the
; identifier the daemon will surface as the node's display name.

(namespace_definition
  name: (namespace_name) @name) @namespace

(class_declaration
  name: (name) @name) @class

(interface_declaration
  name: (name) @name) @interface

(trait_declaration
  name: (name) @name) @trait

(enum_declaration
  name: (name) @name) @enum

(enum_case
  name: (name) @name) @enum_case

(function_definition
  name: (name) @name) @function

(method_declaration
  name: (name) @name) @method

(property_declaration
  (property_element
    name: (variable_name) @name)) @property

(const_declaration
  (const_element
    (name) @name)) @constant

(base_clause
  [(name) (qualified_name)] @name) @extends

(class_interface_clause
  [(name) (qualified_name)] @name) @implements

(use_declaration
  [(name) (qualified_name)] @name) @trait_use
