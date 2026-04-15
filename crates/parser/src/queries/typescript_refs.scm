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
