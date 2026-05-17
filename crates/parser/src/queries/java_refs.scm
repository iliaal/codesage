; Pattern 0: method call -> foo()
(method_invocation
  name: (identifier) @ref)

; Pattern 1: new Foo() -> Instantiation
(object_creation_expression
  type: (type_identifier) @ref)

; Pattern 2: new Foo<T>() -> Instantiation
(object_creation_expression
  type: (generic_type
    (type_identifier) @ref))

; Pattern 3: new pkg.Foo() -> Instantiation
(object_creation_expression
  type: (scoped_type_identifier
    (type_identifier) @ref))

; Pattern 4: class Foo extends Bar -> Inheritance
(superclass
  (type_identifier) @ref)

; Pattern 5: class Foo extends Bar<T> -> Inheritance
(superclass
  (generic_type
    (type_identifier) @ref))

; Pattern 6: class Foo implements Bar -> Inheritance
(super_interfaces
  (type_list
    (type_identifier) @ref))

; Pattern 7: class Foo implements Bar<T> -> Inheritance
(super_interfaces
  (type_list
    (generic_type
      (type_identifier) @ref)))

; Pattern 8: interface Foo extends Bar -> Inheritance
(extends_interfaces
  (type_list
    (type_identifier) @ref))

; Pattern 9: interface Foo extends Bar<T> -> Inheritance
(extends_interfaces
  (type_list
    (generic_type
      (type_identifier) @ref)))

; Pattern 10: import pkg.Type -> Import
(import_declaration
  (scoped_identifier) @ref)

; Pattern 11: import Type -> Import
(import_declaration
  (identifier) @ref)

; Pattern 12: @Override -> annotation type usage (Call). Bare marker form
; (no arguments). Spring/JUnit/JPA route off these — `find_references` on
; the annotation type needs to surface decoration sites.
(marker_annotation
  name: (identifier) @ref)

; Pattern 13: @Test(timeout = 1000) -> annotation type usage (Call). With-args
; form. Same intent as pattern 12.
(annotation
  name: (identifier) @ref)

; Pattern 14: @pkg.Foo (qualified annotation name) -> Call. The scoped form
; surfaces the final identifier so `find_references("Foo")` matches whether
; the user wrote `@Foo` or `@pkg.Foo`.
(marker_annotation
  name: (scoped_identifier
    name: (identifier) @ref))

; Pattern 15: @pkg.Foo(...) qualified form with args.
(annotation
  name: (scoped_identifier
    name: (identifier) @ref))
