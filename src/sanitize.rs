/// Sanitize institution-provided strings before they hit the webview.
pub fn sanitize_user_text(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .map(|c| match c {
            '<' => '‹',
            '>' => '›',
            '&' => '＆',
            '"' => '”',
            '\'' => '’',
            other => other,
        })
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markup() {
        let s = sanitize_user_text("<script>alert(1)</script>");
        assert!(!s.contains('<'));
        assert!(!s.contains('>'));
    }
}
