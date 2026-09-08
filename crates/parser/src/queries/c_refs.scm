; Keep top-level pattern order synchronized with `c_ref_kind` in references.rs.

; Pattern 0: #include <lib.h>
(preproc_include path: (system_lib_string) @ref)

; Pattern 1: #include "header.h"
(preproc_include path: (string_literal) @ref)

; Pattern 2: function call
(call_expression function: (identifier) @ref)
