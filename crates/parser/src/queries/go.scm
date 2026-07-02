; Pattern 0: Function → Function
(function_declaration name: (identifier) @name) @def

; Pattern 1: Method (with receiver) → Method
(method_declaration name: (field_identifier) @name) @def

; Pattern 2: Type declaration → kind refined at runtime (struct/interface/alias)
(type_declaration
  (type_spec
    name: (type_identifier) @name)) @def

; Pattern 3: Type alias (type X = Y) → Constant
(type_declaration
  (type_alias
    name: (type_identifier) @name)) @def

; Pattern 4: Const → Constant
(const_declaration
  (const_spec
    name: (identifier) @name) @def)

; Pattern 5: Package-level var → Constant
; Anchored to source_file so locals inside function bodies are not captured.
(source_file
  (var_declaration
    (var_spec
      name: (identifier) @name) @def))
