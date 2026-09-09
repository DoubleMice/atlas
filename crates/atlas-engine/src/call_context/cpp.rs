//! C++ syntax positions used by on-demand investigation. No name/type resolution.
use tree_sitter::Node;
use types::TextRange;

pub(super) fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}
pub(super) fn range(node: Node<'_>) -> TextRange {
    TextRange {
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
        start_line: node.start_position().row as u32,
        start_column: node.start_position().column as u32,
        end_line: node.end_position().row as u32,
        end_column: node.end_position().column as u32,
    }
}
fn at(root: Node<'_>, location: TextRange) -> Option<Node<'_>> {
    root.descendant_for_byte_range(location.start_byte as usize, location.end_byte as usize)
}
pub(super) fn receiver(root: Node<'_>, location: TextRange) -> Option<Node<'_>> {
    let mut node = at(root, location)?;
    loop {
        if node.kind() == "call_expression" {
            let function = node.child_by_field_name("function")?;
            if function.kind() == "field_expression" {
                return function.child_by_field_name("argument");
            }
            return None;
        }
        if matches!(
            node.kind(),
            "declaration" | "expression_statement" | "function_definition"
        ) {
            return None;
        }
        node = node.parent()?;
    }
}

pub(super) fn receiver_name<'a>(node: Node<'_>, source: &'a str) -> Option<(&'a str, bool)> {
    if node.kind() == "identifier" {
        return Some((text(node, source), false));
    }
    if node.kind() == "field_expression" && node.child_by_field_name("argument")?.kind() == "this" {
        let field = node.child_by_field_name("field")?;
        if field.kind() == "field_identifier" {
            return Some((text(field, source), true));
        }
    }
    None
}

pub(super) fn inside_lambda(mut node: Node<'_>) -> bool {
    loop {
        match node.kind() {
            "lambda_expression" => return true,
            "function_definition" => return false,
            _ => {}
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}
pub(super) struct BindingSyntax<'a> {
    pub declaration: Node<'a>,
    pub type_node: Option<Node<'a>>,
    pub initializer: Option<Node<'a>>,
}
pub(super) fn binding(root: Node<'_>, location: TextRange) -> Option<BindingSyntax<'_>> {
    let mut node = at(root, location)?;
    let mut initializer = None;
    loop {
        if node.kind() == "init_declarator" {
            initializer = node.child_by_field_name("value");
        }
        if matches!(
            node.kind(),
            "declaration"
                | "field_declaration"
                | "parameter_declaration"
                | "optional_parameter_declaration"
        ) {
            if node.has_error() {
                return None;
            }
            return Some(BindingSyntax {
                declaration: node,
                type_node: node.child_by_field_name("type"),
                initializer,
            });
        }
        if matches!(
            node.kind(),
            "compound_statement" | "function_definition" | "lambda_expression"
        ) {
            return None;
        }
        node = node.parent()?;
    }
}
pub(super) struct CallableSyntax<'a> {
    pub header: TextRange,
    pub type_node: Option<Node<'a>>,
}
pub(super) fn callable(root: Node<'_>, location: TextRange) -> Option<CallableSyntax<'_>> {
    let mut node = at(root, location)?;
    let mut function = false;
    loop {
        if node.kind() == "function_declarator" {
            if node.has_error() {
                return None;
            }
            function = true;
        }
        if matches!(
            node.kind(),
            "function_definition" | "declaration" | "field_declaration"
        ) {
            if !function {
                return None;
            }
            let mut header = range(node);
            if let Some(body) = node.child_by_field_name("body") {
                header.end_byte = body.start_byte() as u32;
                header.end_line = body.start_position().row as u32;
                header.end_column = body.start_position().column as u32;
            }
            let type_node = node.child_by_field_name("type").filter(|n| !n.has_error());
            return Some(CallableSyntax { header, type_node });
        }
        if matches!(
            node.kind(),
            "compound_statement" | "class_specifier" | "struct_specifier"
        ) {
            return None;
        }
        node = node.parent()?;
    }
}
pub(super) fn type_names(root: Node<'_>) -> Vec<Node<'_>> {
    let mut names = vec![];
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.has_error() {
            continue;
        }
        if node.kind() == "type_identifier" {
            names.push(node);
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    names.sort_by_key(|n| n.start_byte());
    names
}
