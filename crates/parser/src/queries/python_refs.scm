; Pattern 0: import statement (import os)
(import_statement name: (dotted_name) @ref)

; Pattern 1: from import module (from os.path import join)
(import_from_statement module_name: (dotted_name) @ref)

; Pattern 2: from import specific name (from os.path import join)
(import_from_statement name: (dotted_name) @ref)

; Pattern 3: from import aliased name (from os.path import join as j)
(import_from_statement name: (aliased_import name: (dotted_name) @ref))

; Pattern 4: function/method call
(call function: (identifier) @ref)
