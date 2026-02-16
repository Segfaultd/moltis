pub const SLACK_MAX_MESSAGE_LEN: usize = 40_000;
pub const SLACK_DEFAULT_CHUNK_SIZE: usize = 4_000;

#[must_use]
pub fn markdown_to_mrkdwn(md: &str) -> String {
    let mut text = strip_fence_languages(md);
    text = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    text = convert_links(&text);
    text = text.replace("~~", "~");
    text = convert_bold_markers(&text);
    text = convert_single_asterisk_italic(&text);
    text.replace('\u{1}', "*").replace('\u{2}', "*")
}

#[must_use]
pub fn chunk_message(text: &str, max_len: usize) -> Vec<String> {
    if text.is_empty() || max_len == 0 {
        return vec![text.to_string()];
    }
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.len() > max_len {
        let window = &rest[..max_len];
        let split_at = window
            .rfind('\n')
            .or_else(|| window.rfind(' '))
            .unwrap_or(max_len);
        let split_at = split_at.max(1);
        chunks.push(rest[..split_at].trim_end().to_string());
        rest = rest[split_at..].trim_start();
    }
    if !rest.is_empty() {
        chunks.push(rest.to_string());
    }
    chunks
}

fn strip_fence_languages(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        if line.starts_with("```") && line.len() > 3 {
            out.push_str("```");
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !input.ends_with('\n') {
        let _ = out.pop();
    }
    out
}

fn convert_bold_markers(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            let mut j = i + 2;
            let mut found = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'*' {
                    found = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = found {
                out.push('\u{1}');
                out.push_str(&input[i + 2..end]);
                out.push('\u{2}');
                i = end + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn convert_single_asterisk_italic(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'*'
            && let Some(end) = input[i + 1..].find('*')
        {
            let end = i + 1 + end;
            // Only convert when there is text between delimiters.
            if end > i + 1 {
                out.push('_');
                out.push_str(&input[i + 1..end]);
                out.push('_');
                i = end + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn convert_links(input: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < input.len() {
        let rem = &input[i..];
        if let Some(open) = rem.find('[') {
            out.push_str(&rem[..open]);
            let label_start = i + open + 1;
            let after_label = &input[label_start..];
            if let Some(label_end_rel) = after_label.find("](") {
                let label_end = label_start + label_end_rel;
                let url_start = label_end + 2;
                if let Some(url_end_rel) = input[url_start..].find(')') {
                    let url_end = url_start + url_end_rel;
                    let label = &input[label_start..label_end];
                    let url = &input[url_start..url_end];
                    out.push('<');
                    out.push_str(url);
                    out.push('|');
                    out.push_str(label);
                    out.push('>');
                    i = url_end + 1;
                    continue;
                }
            }
            out.push('[');
            i = label_start;
            continue;
        }
        out.push_str(rem);
        break;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("**bold**", "*bold*")]
    #[case("*italic*", "_italic_")]
    #[case("`code`", "`code`")]
    #[case("~~strike~~", "~strike~")]
    fn markdown_to_slack(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(markdown_to_mrkdwn(input), expected);
    }

    #[test]
    fn fenced_code_block() {
        let input = "```rust\nfn main() {}\n```";
        let output = markdown_to_mrkdwn(input);
        assert!(output.contains("```\nfn main() {}\n```"));
    }

    #[test]
    fn link_conversion() {
        assert_eq!(
            markdown_to_mrkdwn("[click](https://example.com)"),
            "<https://example.com|click>"
        );
    }

    #[test]
    fn chunk_short_message() {
        let chunks = chunk_message("hello", 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_at_newline() {
        let text = "line1\nline2\nline3";
        let chunks = chunk_message(text, 10);
        assert_eq!(chunks[0], "line1");
    }

    #[test]
    fn special_chars_escaped() {
        assert_eq!(
            markdown_to_mrkdwn("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }
}
