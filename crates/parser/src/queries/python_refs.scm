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

; Pattern 5: attribute method call (self.db.find(...))
(call function: (attribute attribute: (identifier) @ref))

; Pattern 6: relative import module (from .models import User / from . import helpers)
; The captured text keeps the leading dots so the consumer can resolve it
; relative to the importing file.
(import_from_statement module_name: (relative_import) @ref)

; Patterns 7-10: decorators, mirroring Java's annotation patterns (which file
; as Call so `find_references("Test", kind="call")` surfaces decoration
; sites). Only the applied name is captured — `@app.route("/x")` records
; `route`, the same way attribute calls record the method, not the receiver.
; Pattern 7: bare decorator (@property)
(decorator (identifier) @ref)

; Pattern 8: decorator with arguments (@retry(tries=3))
(decorator (call function: (identifier) @ref))

; Pattern 9: dotted decorator (@app.route, no call)
(decorator (attribute attribute: (identifier) @ref))

; Pattern 10: dotted decorator with arguments (@app.route("/x"))
(decorator (call function: (attribute attribute: (identifier) @ref)))
