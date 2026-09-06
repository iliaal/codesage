; TypeScript reference patterns. Patterns 0-5 mirror javascript_refs.scm;
; pattern 6 handles the TS-only `extends_clause` heritage node, which does not
; exist in the JavaScript grammar (so it cannot live in the shared JS file).
; The JS `class_heritage (identifier)` inheritance form is an impossible pattern
; in the TSX grammar (heritage is always wrapped in extends/implements clauses),
; so it is omitted here.

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

; Patterns 7-9: the imported BINDING names. See javascript_refs.scm for why —
; the module specifier alone leaves `Foo.staticMethod()` / `instanceof Foo`
; users with no row naming the symbol.
(import_statement (import_clause (identifier) @ref))
(import_statement (import_clause (named_imports (import_specifier name: (identifier) @ref))))
(import_statement (import_clause (namespace_import (identifier) @ref)))

; Patterns 10-11: re-export and CommonJS destructuring bindings.
; See javascript_refs.scm — the module string alone leaves barrel files and
; `const { a } = require(...)` consumers naming no symbol.
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

; Patterns 13-14: destructuring a module's exports off a VALUE rather than a
; `require(...)` call. A barrel does `const { Foo } = axios;` after importing
; the default export, so the symbols it unwraps are named nowhere else in the
; file. The RHS is captured as `@rhs` so the extractor can keep only
; destructures off a same-file import binding: without that check every
; `const { data } = resp.body` binds a bogus edge to any symbol named `data`.
(variable_declarator
  name: (object_pattern (shorthand_property_identifier_pattern) @ref)
  value: (identifier) @rhs)
(variable_declarator
  name: (object_pattern (pair_pattern key: (property_identifier) @ref))
  value: (identifier) @rhs)

; Pattern 15: the CommonJS require-bound LOCAL, `const axios = require('axios')`.
; Mirrors the JS pattern of the same index: pattern 1 captures only the module
; string, so without the LHS binding the value-destructure filter (patterns
; 13-14) drops `const { Foo } = axios`. See javascript_refs.scm.
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

; Patterns 18-19: TS import-equals, `import axios = require('axios')`. The
; grammar puts `source` on the `import_require_clause`, not on the
; `import_statement`, so pattern 0 never matched it and pattern 7's
; `import_clause` never saw the binding: a file using this form recorded
; neither the module nor the local name. 18 is the binding, 19 the module.
(import_statement (import_require_clause (identifier) @ref))
(import_statement (import_require_clause source: (string) @ref))

; Pattern 20: a type written through the module namespace,
; `const h: axios.AxiosHeaders = ...`. Type positions parse as
; `nested_type_identifier`, not `member_expression`, so pattern 16 does not
; reach them. Same `@rhs` import-binding gate as pattern 16.
(nested_type_identifier
  module: (identifier) @rhs
  name: (type_identifier) @ref)
