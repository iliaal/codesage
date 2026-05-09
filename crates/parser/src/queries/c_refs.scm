; Tree-sitter assigns one pattern_index per top-level S-expression below.
; Keep this list in lock-step with the `c_ref_kind` match in
; `crates/parser/src/references.rs`: adding a pattern here means adding a
; case there.

; Pattern 0: #include <lib.h>
(preproc_include path: (system_lib_string) @ref)

; Pattern 1: #include "header.h"
(preproc_include path: (string_literal) @ref)

; Pattern 2: function call
(call_expression function: (identifier) @ref)
