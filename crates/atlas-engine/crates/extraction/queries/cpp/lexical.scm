;; C++ lexical binding captures: parameters, locals, for-loop vars, catch vars

;; --- Function parameters ---
(parameter_declaration
  declarator: (identifier) @lexical.parameter)

(parameter_declaration
  declarator: (pointer_declarator declarator: (identifier) @lexical.parameter))
(parameter_declaration
  declarator: (reference_declarator (identifier) @lexical.parameter))
(parameter_declaration
  declarator: (function_declarator
    declarator: (parenthesized_declarator
      (pointer_declarator declarator: (identifier) @lexical.parameter))))

;; --- Local variable declarations ---
(declaration
  declarator: (identifier) @lexical.local)
(declaration
  declarator: (pointer_declarator declarator: (identifier) @lexical.local))
(declaration
  declarator: (reference_declarator (identifier) @lexical.local))

(declaration
  declarator: (init_declarator
    declarator: (identifier) @lexical.local))
(declaration
  declarator: (init_declarator
    declarator: (pointer_declarator declarator: (identifier) @lexical.local)))
(declaration
  declarator: (init_declarator
    declarator: (reference_declarator (identifier) @lexical.local)))

;; --- For-loop initializer (for (int i = 0; ...)) ---
(for_statement
  initializer: (declaration
    declarator: (init_declarator
      declarator: (identifier) @lexical.local)))

;; --- Range-based for (for (auto x : ...)) ---
(for_statement
  initializer: (declaration
    declarator: (structured_binding_declarator
      (identifier) @lexical.local)))

;; --- Catch variable (catch (exception& e)) ---
;; NOTE: tree-sitter-cpp catch_clause has children.multiple; identifier
;; is not a valid direct child.  Catch bindings are captured via the
;; parameter_declaration pattern at the top of this file instead.
;; (catch_clause ...) intentionally omitted.

;; --- Lambda parameter ---
;; Lambda parameters are captured by the parameter_declaration pattern above.
;; (lambda_expression ...) intentionally omitted.
