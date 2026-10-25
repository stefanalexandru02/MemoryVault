//! Splitting an ingested document into chunks of about 600-800 tokens.
//!
//! Markdown is read as blocks (a heading line, or a run of lines up to the next blank line; a fenced code block
//! is one block) and the blocks are packed in order. A chunk is closed before a heading once it holds 600 tokens,
//! and before any block that would take it past 800; a heading always travels with the block after it. Headings
//! stay inside the chunk text, and each chunk records
//! the heading path in effect at its first line, so a chunk cut from the middle of a long section still says
//! where it comes from. Plain text is the same without headings. A block longer than 800 tokens is cut at line
//! ends (and, for a single huge line, at character boundaries).
//!
//! Sizes use the project-wide estimate of 4 characters per token.

use memvault_core::schemas::common::ContentType;

/// Close a chunk at the next heading once it holds this many characters (600 tokens).
const MIN_CHARS: usize = 2_400;
/// No chunk is longer than this (800 tokens), unless a single block cannot be cut.
const MAX_CHARS: usize = 3_200;
/// Longest heading kept in a heading path.
const MAX_HEADING_CHARS: usize = 120;

/// One chunk of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Headings in effect at the chunk's first line, outermost first.
    pub heading_path: Vec<String>,
    pub content: String,
}

/// The chunks of a document and its first top-level heading, if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunked {
    pub top_heading: Option<String>,
    pub chunks: Vec<Chunk>,
}

struct Block {
    text: String,
    heading: bool,
    /// Heading path at the block's first line (a heading block includes itself).
    path: Vec<String>,
}

pub fn chunk(content_type: ContentType, content: &str) -> Chunked {
    let (blocks, top_heading) = match content_type {
        ContentType::Markdown => markdown_blocks(content),
        ContentType::Text => (paragraph_blocks(content), None),
    };
    Chunked { top_heading, chunks: pack(blocks) }
}

/// `# Title` .. `###### Title`: the level and the text without closing hashes.
fn atx_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let level = trimmed.bytes().take_while(|b| *b == b'#').count();
    let rest = &trimmed[level..];
    if !(1..=6).contains(&level) || !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim();
    (!text.is_empty()).then(|| (level, text.chars().take(MAX_HEADING_CHARS).collect()))
}

/// The fence marker (`` ``` `` or `~~~`) a line opens or closes, if any.
fn fence(line: &str) -> Option<char> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some('`')
    } else if t.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

fn markdown_blocks(content: &str) -> (Vec<Block>, Option<String>) {
    let mut blocks: Vec<Block> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut top_heading = None;
    let mut lines: Vec<&str> = Vec::new();
    let mut open_fence: Option<char> = None;
    let path_of = |stack: &[(usize, String)]| stack.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>();
    let mut start_path = Vec::new();
    let flush = |lines: &mut Vec<&str>, blocks: &mut Vec<Block>, path: &[String]| {
        if !lines.is_empty() {
            blocks.push(Block { text: lines.join("\n"), heading: false, path: path.to_vec() });
            lines.clear();
        }
    };
    for line in content.lines() {
        if let Some(marker) = open_fence {
            lines.push(line);
            if fence(line) == Some(marker) {
                open_fence = None;
            }
            continue;
        }
        if let Some((level, text)) = atx_heading(line) {
            flush(&mut lines, &mut blocks, &start_path);
            while stack.last().is_some_and(|(l, _)| *l >= level) {
                stack.pop();
            }
            stack.push((level, text.clone()));
            if level == 1 && top_heading.is_none() {
                top_heading = Some(text);
            }
            blocks.push(Block { text: line.trim().to_string(), heading: true, path: path_of(&stack) });
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut lines, &mut blocks, &start_path);
            continue;
        }
        if lines.is_empty() {
            start_path = path_of(&stack);
        }
        if let Some(marker) = fence(line) {
            open_fence = Some(marker);
        }
        lines.push(line);
    }
    flush(&mut lines, &mut blocks, &start_path);
    (blocks, top_heading)
}

fn paragraph_blocks(content: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    for line in content.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if !lines.is_empty() {
                blocks.push(Block { text: lines.join("\n"), heading: false, path: Vec::new() });
                lines.clear();
            }
        } else {
            lines.push(line);
        }
    }
    blocks
}

/// `text` cut into pieces of at most `max` characters, at line ends where it can.
fn split_oversized(text: &str, max: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_chars = 0;
    for line in text.split('\n') {
        let mut rest = line;
        // A single line over the limit is cut at character boundaries.
        while rest.chars().count() > max {
            let end = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
            if !cur.is_empty() {
                pieces.push(std::mem::take(&mut cur));
                cur_chars = 0;
            }
            pieces.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        let n = rest.chars().count();
        if !cur.is_empty() && cur_chars + 1 + n > max {
            pieces.push(std::mem::take(&mut cur));
            cur_chars = 0;
        }
        if !cur.is_empty() {
            cur.push('\n');
            cur_chars += 1;
        }
        cur.push_str(rest);
        cur_chars += n;
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

fn pack(blocks: Vec<Block>) -> Vec<Chunk> {
    let mut blocks_in: Vec<Block> = Vec::new();
    for b in blocks {
        if b.text.chars().count() > MAX_CHARS {
            blocks_in.extend(split_oversized(&b.text, MAX_CHARS).into_iter().map(|text| Block {
                text,
                heading: false,
                path: b.path.clone(),
            }));
        } else {
            blocks_in.push(b);
        }
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut cur: Option<Chunk> = None;
    let mut cur_chars = 0;
    let mut ends_with_heading = false;
    for b in blocks_in {
        let n = b.text.chars().count();
        // A heading is never left at the end of a chunk: the block after it comes along, even past the limit.
        let full = cur_chars + 2 + n > MAX_CHARS || (b.heading && cur_chars >= MIN_CHARS);
        if cur.is_some() && full && !ends_with_heading {
            chunks.extend(cur.take());
        }
        ends_with_heading = b.heading;
        match &mut cur {
            Some(c) => {
                c.content.push_str("\n\n");
                c.content.push_str(&b.text);
                cur_chars += 2 + n;
            }
            None => {
                cur = Some(Chunk { heading_path: b.path, content: b.text });
                cur_chars = n;
            }
        }
    }
    chunks.extend(cur);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize) -> String {
        vec!["word"; n].join(" ")
    }

    fn md(content: &str) -> Chunked {
        chunk(ContentType::Markdown, content)
    }

    #[test]
    fn a_short_document_is_one_chunk_with_its_headings_inline() {
        let c = md("# Title\n\nIntro.\n\n## Decision\n\nUse Postgres.\n");
        assert_eq!(c.top_heading.as_deref(), Some("Title"));
        assert_eq!(
            c.chunks,
            vec![Chunk {
                heading_path: vec!["Title".into()],
                content: "# Title\n\nIntro.\n\n## Decision\n\nUse Postgres.".into()
            }]
        );
    }

    #[test]
    fn a_chunk_closes_at_the_next_heading_once_it_is_big_enough() {
        let body = words(520); // 2599 characters
        let c = md(&format!("# Guide\n\n{body}\n\n## Deploy\n\nRun it.\n\n### Rollback\n\nUndo it."));
        assert_eq!(c.chunks.len(), 2);
        assert_eq!(c.chunks[0].heading_path, vec!["Guide"]);
        assert!(c.chunks[0].content.starts_with("# Guide"));
        assert_eq!(c.chunks[1].heading_path, vec!["Guide", "Deploy"]);
        assert!(c.chunks[1].content.starts_with("## Deploy"));
        assert!(c.chunks[1].content.contains("### Rollback"), "a small section joins the chunk it follows");
    }

    #[test]
    fn a_heading_is_never_left_behind_its_section() {
        let big = words(639); // 3194 characters: fills a chunk on its own
        let c = md(&format!("# Doc\n\nintro\n\n{big}\n\n## Next\n\n{big}"));
        assert_eq!(c.chunks.len(), 3, "{c:?}");
        assert!(c.chunks.iter().all(|ch| !ch.content.trim_end().ends_with("## Next") || ch.content.contains(&big)));
        assert!(c.chunks[2].content.starts_with("## Next\n\nword"));
        assert_eq!(c.chunks[2].heading_path, vec!["Doc", "Next"]);
    }

    #[test]
    fn small_sections_are_packed_together() {
        let c = md("# A\n\none\n\n## B\n\ntwo\n\n## C\n\nthree");
        assert_eq!(c.chunks.len(), 1);
    }

    #[test]
    fn a_long_section_is_cut_at_paragraphs_and_keeps_its_heading_path() {
        let para = words(300); // 1499 characters
        let text = format!("# Ops\n\n## Runbook\n\n{para}\n\n{para}\n\n{para}\n\n{para}");
        let c = md(&text);
        assert!(c.chunks.len() >= 2, "{c:?}");
        for chunk in &c.chunks {
            assert!(chunk.content.chars().count() <= MAX_CHARS);
        }
        let last = c.chunks.last().unwrap();
        assert_eq!(last.heading_path, vec!["Ops", "Runbook"]);
        assert!(!last.content.contains("## Runbook"));
    }

    #[test]
    fn heading_levels_pop_the_path() {
        let c = md(&format!("# A\n\n## B\n\n### C\n\n{}\n\n## D\n\ntail", words(600)));
        let d = c.chunks.iter().find(|ch| ch.content.starts_with("## D")).expect("a chunk starts at D");
        assert_eq!(d.heading_path, vec!["A", "D"]);
    }

    #[test]
    fn fenced_code_is_one_block_and_hides_headings() {
        let c = md("# Doc\n\n```sh\n# not a heading\n\nmake build\n```\n\nafter");
        assert_eq!(c.chunks.len(), 1);
        assert!(c.chunks[0].content.contains("```sh\n# not a heading\n\nmake build\n```"));
        assert_eq!(c.chunks[0].heading_path, vec!["Doc"]);
        assert_eq!(c.top_heading.as_deref(), Some("Doc"));
    }

    #[test]
    fn hash_tags_and_deep_indents_are_not_headings() {
        assert_eq!(atx_heading("#hashtag"), None);
        assert_eq!(atx_heading("    # code"), None);
        assert_eq!(atx_heading("####### seven"), None);
        assert_eq!(atx_heading("## Closing ##"), Some((2, "Closing".into())));
        assert_eq!(atx_heading("#"), None);
    }

    #[test]
    fn text_has_no_heading_path() {
        let c = chunk(ContentType::Text, &format!("{}\n\n{}\n\n{}", words(300), words(300), words(300)));
        assert_eq!(c.top_heading, None);
        assert!(c.chunks.len() >= 2);
        assert!(c.chunks.iter().all(|ch| ch.heading_path.is_empty() && ch.content.chars().count() <= MAX_CHARS));
    }

    #[test]
    fn a_single_huge_line_is_cut_on_character_boundaries() {
        let line = "é".repeat(MAX_CHARS * 2 + 10);
        let c = chunk(ContentType::Text, &line);
        assert_eq!(c.chunks.len(), 3);
        assert_eq!(c.chunks.iter().map(|ch| ch.content.chars().count()).sum::<usize>(), line.chars().count());
        assert!(c.chunks.iter().all(|ch| ch.content.chars().count() <= MAX_CHARS));
    }

    #[test]
    fn blank_input_has_no_chunks() {
        assert!(md("\n \n").chunks.is_empty());
        assert!(chunk(ContentType::Text, "").chunks.is_empty());
    }

    #[test]
    fn crlf_and_headings_without_a_top_level() {
        let c = md("## Only\r\n\r\ntext\r\n");
        assert_eq!(c.top_heading, None);
        assert_eq!(c.chunks[0].heading_path, vec!["Only"]);
        assert_eq!(c.chunks[0].content, "## Only\n\ntext");
    }
}
