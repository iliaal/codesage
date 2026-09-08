; TS heritage uses `extends_clause`; JS's bare `class_heritage` cannot compile here.

; Pattern 0: import statement (captures the module source string)
(import_statement source: (string) @ref)

; Pattern 1: require("module") -- captures the module string
(call_expression
  function: (identifier) @_fn
  arguments: (arguments (string) @ref)
  (#eq? @_fn "require"))

; Pattern 2: function call (simple identifier)
(call_expression function: (identifier) @ref)

; Pattern 3: function call (member expression like obj.method())
(call_expression function: (member_expression property: (property_identifier) @ref))

; Pattern 4: re-export (export { x } from "./mod" / export * from "./mod")
(export_statement source: (string) @ref)

; Pattern 5: instantiation (new Foo())
(new_expression constructor: (identifier) @ref)

; Pattern 6: class inheritance (class Foo extends Bar) -- TS extends_clause form
(extends_clause value: (identifier) @ref)

; Patterns 7-9: local, named, and namespace import bindings.
(import_statement (import_clause (identifier) @ref))
(import_statement (import_clause (named_imports (import_specifier name: (identifier) @ref))))
(import_statement (import_clause (namespace_import (identifier) @ref)))

; Patterns 10-11: re-exported names and CommonJS destructuring bindings.
(export_statement (export_clause (export_specifier name: (identifier) @ref)))
(variable_declarator
  name: (object_pattern (shorthand_property_identifier_pattern) @ref)
  value: (call_expression
    function: (identifier) @_req
    arguments: (arguments (string)))
  (#eq? @_req "require"))

; Pattern 12: aliased CommonJS destructuring, `const { a: localA } = require(...)`.
; The shorthand form above is a `shorthand_property_identifier_pattern`; an
; alias is a `pair_pattern`, whose KEY names the exported symbol.
(variable_declarator
  name: (object_pattern (pair_pattern key: (property_identifier) @ref))
  value: (call_expression
    function: (identifier) @_req
    arguments: (arguments (string)))
  (#eq? @_req "require"))

; Patterns 13-14: value destructuring; references.rs requires imported `@rhs`.
(variable_declarator
  name: (object_pattern (shorthand_property_identifier_pattern) @ref)
  value: (identifier) @rhs)
(variable_declarator
  name: (object_pattern (pair_pattern key: (property_identifier) @ref))
  value: (identifier) @rhs)

; Pattern 15: require-bound local for the receiver allowlist in references.rs.
(variable_declarator
  name: (identifier) @ref
  value: (call_expression
    function: (identifier) @_req
    arguments: (arguments (string)))
  (#eq? @_req "require"))

; Pattern 16: non-call member access off a same-file import binding. Mirrors
; the JS pattern of the same index; see javascript_refs.scm.
(member_expression
  object: (identifier) @rhs
  property: (property_identifier) @ref)

; Pattern 17: `const axios = require('axios').default`. Mirrors the JS
; pattern of the same index; see javascript_refs.scm.
(variable_declarator
  name: (identifier) @ref
  value: (member_expression
    object: (call_expression
      function: (identifier) @_req
      arguments: (arguments (string)))
    property: (property_identifier))
  (#eq? @_req "require"))

; Patterns 18-19: TS import-equals (`import axios = require('axios')`).
; Unlike ordinary imports, both binding and source live in `import_require_clause`.
(import_statement (import_require_clause (identifier) @ref))
(import_statement (import_require_clause source: (string) @ref))

; Pattern 20: a type written through the module namespace,
; `const h: axios.AxiosHeaders = ...`. Type positions parse as
; `nested_type_identifier`, not `member_expression`, so pattern 16 does not
; reach them. Same `@rhs` import-binding gate as pattern 16.
(nested_type_identifier
  module: (identifier) @rhs
  name: (type_identifier) @ref)
