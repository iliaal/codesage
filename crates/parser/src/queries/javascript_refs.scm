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

; Pattern 5: class inheritance (class Foo extends Bar) -- JS heritage form
(class_heritage (identifier) @ref)

; Pattern 6: instantiation (new Foo())
(new_expression constructor: (identifier) @ref)

; Patterns 7-9: the imported BINDING names. Pattern 0 captures only the module
; specifier, so a file that imports a symbol and then uses it in a form no other
; pattern captures — `Foo.staticMethod()` (pattern 3 sees only `staticMethod`),
; `x instanceof Foo`, `obj.Foo = Foo` — produced no reference row naming that
; symbol at all, and the file dropped out of its dependents.
; Appended at the end: pattern indices are positional and map to kinds by
; number in references.rs, so inserting above would renumber every kind.
(import_statement (import_clause (identifier) @ref))
(import_statement (import_clause (named_imports (import_specifier name: (identifier) @ref))))
(import_statement (import_clause (namespace_import (identifier) @ref)))

; Patterns 10-11: bindings that arrive by a route other than `import`.
; `export { x }` names the symbols it forwards, with or without a `from` clause
; — a barrel that re-exports a locally destructured binding has no source, and
; that is the only place those names appear. CommonJS destructuring names
; what it pulls off the module object; both previously recorded only the module
; string, so a barrel file or a `const { a } = require(...)` consumer named no
; symbol and vanished from that symbol's dependents.
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
; Pattern 1 captures only the module string, so the value-destructure filter
; (patterns 13-14) sees no same-file import binding for `axios` and drops
; `const { Foo } = axios` — a require-then-destructure consumer names no
; symbol. Recording the LHS identifier as an ImportBinding admits it to the
; allowlist in references.rs without admitting arbitrary object unpacks
; (`const { data } = resp.body` still drops: `resp` binds nothing here).
(variable_declarator
  name: (identifier) @ref
  value: (call_expression
    function: (identifier) @_req
    arguments: (arguments (string)))
  (#eq? @_req "require"))

; Pattern 16: non-call member access off a same-file import binding,
; `axios.CancelToken.source()` / `typeof axios.CancelToken` / `new
; axios.CancelToken(...)`. Pattern 3 sees a member expression only as a
; callee and records its property (`source`), so a file that reaches an
; export through the module object rather than a named import named the
; export nowhere. The receiver is captured as `@rhs` so the extractor can
; apply the same import-binding gate as patterns 13-14: `response.data`
; binds nothing because `response` is not imported. A member expression that
; is itself the callee is skipped in references.rs to avoid duplicating
; pattern 3's row.
(member_expression
  object: (identifier) @rhs
  property: (property_identifier) @ref)

; Pattern 17: the CommonJS default-unwrapped LOCAL,
; `const axios = require('axios').default`. Pattern 15 requires the
; `require(...)` call to be the whole initializer, so this form left `axios`
; out of the import-binding allowlist and every later `const { X } = axios`
; and `axios.X` was dropped as an arbitrary object unpack.
(variable_declarator
  name: (identifier) @ref
  value: (member_expression
    object: (call_expression
      function: (identifier) @_req
      arguments: (arguments (string)))
    property: (property_identifier))
  (#eq? @_req "require"))
