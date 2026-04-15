; Pattern 0: #include
(preproc_include path: (system_lib_string) @ref)
(preproc_include path: (string_literal) @ref)

; Pattern 1: function call
(call_expression function: (identifier) @ref)
