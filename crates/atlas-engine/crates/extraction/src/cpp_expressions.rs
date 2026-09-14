//! C++ expression boundaries shared by structural extraction and lazy inspection.
//! These helpers do not select operator targets or prove evaluation/completeness.
use tree_sitter::Node;
use types::TextRange;

/// Known unevaluated operands do not contribute invocation regions. A nested
/// function/closure body is investigated independently of its enclosing operand.
pub fn may_be_evaluated(node: Node<'_>, source: &str) -> bool {
    for parent in std::iter::successors(node.parent(), |node| node.parent()) {
        if matches!(parent.kind(), "function_definition" | "lambda_expression")
            && parent.child_by_field_name("body").is_some_and(|body| {
                body.start_byte() <= node.start_byte() && node.end_byte() <= body.end_byte()
            })
        {
            break;
        }
        if matches!(
            parent.kind(),
            "sizeof_expression" | "decltype" | "noexcept" | "requires_expression"
        ) {
            return false;
        }
        // The pinned grammar represents expression-form noexcept as a call.
        if parent.kind() == "call_expression"
            && parent
                .child_by_field_name("function")
                .and_then(|callee| callee.utf8_text(source.as_bytes()).ok())
                == Some("noexcept")
        {
            return false;
        }
    }
    true
}

/// Regions for which ordinary call-reference lookup does not analyze operator,
/// conversion or implicit invocation semantics. Built-in operations may occur;
/// this reports work not done, never a missing or selected function call.
pub fn unexamined_invocation_region(node: Node<'_>, source: &str) -> Option<TextRange> {
    let kind = node.kind();
    if !may_be_evaluated(node, source) {
        return None;
    }
    if kind == "for_range_loop" {
        // The body's written calls are independent of the implicit iteration
        // operations. Preserve exactly the header, including the closing ')'.
        let mut cursor = node.walk();
        let close = node
            .children(&mut cursor)
            .find(|child| child.kind() == ")")?;
        let mut range = crate::languages::node_range(node);
        let end = crate::languages::node_range(close);
        range.end_byte = end.end_byte;
        range.end_line = end.end_line;
        range.end_column = end.end_column;
        return Some(range);
    }
    if !kind.ends_with("_expression")
        || matches!(
            kind,
            // Calls/receivers, callable containment and allocation already have
            // their own facts. Parentheses add no operation of their own.
            "call_expression"
                | "field_expression"
                | "lambda_expression"
                | "parenthesized_expression"
                | "new_expression"
                | "sizeof_expression"
                | "requires_expression"
        )
    {
        return None;
    }
    Some(crate::languages::node_range(node))
}
