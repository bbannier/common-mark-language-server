use {
    intervaltree::{Element, IntervalTree},
    lsp_types::{Position, Range},
    pulldown_cmark::{Event, Parser, Tag},
    regex::Regex,
    std::collections::HashMap,
};

fn to_offset(position: Position, linebreaks: &[usize]) -> Option<usize> {
    let line: usize = position.line.try_into().ok()?;
    let char: usize = position.character.try_into().ok()?;

    let char_prev_lines: usize = linebreaks
        .iter()
        .take(line)
        .map(|c| c + 1)
        .next_back()
        .unwrap_or(0);

    Some(char_prev_lines + char)
}

fn to_position(offset: usize, linebreaks: &[usize]) -> Position {
    let (a, _): (Vec<usize>, _) = linebreaks.iter().partition(|&c| *c < offset);

    let line = a.len();
    let character = match a.last() {
        Some(c) => offset - c - 1, // Newline is not visible.
        None => offset,
    } as u64;

    Position::new(
        line.try_into().expect("line number not representable"),
        character
            .try_into()
            .expect("column number not representable"),
    )
}

#[derive(Debug, PartialEq, Clone)]
pub struct Node<'a> {
    pub data: Event<'a>,
    pub range: Range,
    pub offsets: std::ops::Range<usize>,
    pub anchor: Option<String>,
}

fn get_linebreaks<P: Into<String>>(input: P) -> Vec<usize> {
    // TODO(bbannier): This could probably be made more efficient if callers could exploit that the
    // result is an ordered set.
    input
        .into()
        .chars()
        .enumerate()
        .filter_map(|(i, c)| if c == '\n' { Some(i) } else { None })
        .collect()
}

type AstNodes<'a> = Vec<Node<'a>>;

fn from_offsets(offset: std::ops::Range<usize>, linebreaks: &[usize]) -> Range {
    Range::new(
        to_position(offset.start, linebreaks),
        to_position(offset.end, linebreaks),
    )
}

pub fn parse<'a>(input: &'a str, linebreaks: &[usize]) -> AstNodes<'a> {
    let ast = Parser::new(input)
        .into_offset_iter()
        .map(|(event, range)| Node {
            data: event,
            range: from_offsets(range.clone(), linebreaks),
            offsets: range,
            anchor: None,
        })
        .collect::<Vec<_>>();

    // Counter for the number of occurrences of a anchor's base name.
    let mut repetitions = HashMap::<&str, u64>::new();

    let anchors: Vec<Option<_>> = ast
        .iter()
        .enumerate()
        .map(|(i, node)| match &node.data {
            Event::Start(Tag::Heading { .. }) => {
                ast.iter().skip(i).find_map(|node| match &node.data {
                    Event::Text(text) => {
                        if let Some(count) = repetitions.get_mut(text.as_ref()) {
                            *count += 1;
                        } else {
                            repetitions.insert(text, 0);
                        }

                        Some(anchor(text))
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .collect(); // Collect to unborrow `repetitions`.

    // Filter for just repeating anchors.
    let mut repetitions = repetitions
        .iter()
        .filter_map(|(anchor, &count)| if count > 0 { Some((*anchor, 0)) } else { None })
        .collect::<HashMap<_, _>>();

    // Renumber anchors with repeated anchors base.
    let anchors = anchors.iter().map(|anchor| match anchor {
        Some(anchor) => {
            let anchor: &str = anchor;
            if let Some(count) = repetitions.get_mut(anchor) {
                *count += 1;
                Some(format!("{anchor}-{count}"))
            } else {
                Some(String::from(anchor))
            }
        }
        None => None,
    });

    ast.iter()
        .zip(anchors)
        .map(|(node, anchor)| Node {
            anchor,
            ..node.clone()
        })
        .collect()
}

#[derive(Debug)]
pub struct ParsedDocument<'a> {
    // TODO(bbannier): We do not need to store each `Node` twice, but could e.g., reference `ast`
    // for `tree`.
    ast: AstNodes<'a>,
    tree: IntervalTree<usize, Node<'a>>,
    linebreaks: Vec<usize>,
}

impl PartialEq for ParsedDocument<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.ast == other.ast
    }
}

impl<'a> ParsedDocument<'a> {
    /// Get all nodes overlapping `position`.
    pub fn at(&self, position: Position) -> Vec<&Node<'_>> {
        let Some(start) = to_offset(position, &self.linebreaks) else {
            return vec![];
        };
        let end = 1 + match to_offset(position, &self.linebreaks) {
            Some(offset) => offset,
            None => return vec![],
        };

        self.tree
            .query(start..end)
            .map(|e| &e.value)
            .filter(|n| !matches!(n.data, Event::End(_)))
            .collect()
    }

    pub fn nodes(&self) -> &AstNodes<'a> {
        &self.ast
    }
}

impl<'a> From<&'a str> for ParsedDocument<'a> {
    fn from(input: &'a str) -> Self {
        let linebreaks = get_linebreaks(input);

        let ast = parse(input, &linebreaks);

        let tree: IntervalTree<_, _> = ast
            .iter()
            .map(|n| {
                let range = n.offsets.start..n.offsets.end;
                Element {
                    range,
                    value: (*n).clone(),
                }
            })
            .collect();

        ParsedDocument {
            ast,
            tree,
            linebreaks,
        }
    }
}

pub fn anchor(text: &str) -> String {
    let re = Regex::new(r"-+").unwrap();

    let mut anchor = re
        .replace_all(text, "-")
        .trim()
        .to_ascii_lowercase()
        .replace(' ', "-");

    anchor.retain(|c| c.is_alphanumeric() || c == '-');

    anchor
}

#[cfg(test)]
mod tests {
    use {super::*, insta::assert_debug_snapshot, textwrap::dedent};

    #[test]
    fn test_to_offset() {
        assert_eq!(to_offset(Position::new(0, 5), &[]), Some(5));
        assert_eq!(to_offset(Position::new(0, 0), &[1]), Some(0));
        assert_eq!(to_offset(Position::new(1, 0), &[2]), Some(3));
        assert_eq!(to_offset(Position::new(2, 0), &[2, 4]), Some(5));
    }

    #[test]
    fn test_offset_to_pos() {
        assert_eq!(
            to_position(0, &[]),
            Position {
                line: 0,
                character: 0
            }
        );

        assert_eq!(
            to_position(1, &[5]),
            Position {
                line: 0,
                character: 1
            },
        );

        assert_eq!(
            to_position(5, &[5]),
            Position {
                line: 0,
                character: 5
            },
        );

        assert_eq!(
            to_position(10, &[5]),
            Position {
                line: 1,
                character: 4
            }
        );
    }

    #[test]
    fn test_get_linebreaks() {
        assert_eq!(get_linebreaks("A"), Vec::<usize>::new());
        assert_eq!(get_linebreaks("\n"), vec![0]);
        assert_eq!(get_linebreaks("\n\n"), vec![0, 1]);
        assert_eq!(get_linebreaks("A\n\nA\n"), vec![1, 2, 4]);
    }

    #[test]
    fn parse_simple() {
        let input = dedent(
            "
            # H1

            Foo bar.
            ",
        );

        assert_debug_snapshot!(parse(&input, &get_linebreaks(&input)));
    }

    #[test]
    fn parse_repeated_anchors() {
        let input = dedent(
            "
            # heading
            # heading
            ",
        );

        assert_debug_snapshot!(parse(&input, &get_linebreaks(&input)));
    }

    #[test]
    fn test_query() {
        let s = "asdasad sdaasa aasd asdasdasdada";
        let ast = ParsedDocument::from(s);

        assert_debug_snapshot!(ast.at(Position::new(0, 0)));
    }

    #[test]
    fn test_anchor() {
        assert_eq!(anchor("Foo"), "foo");
        assert_eq!(anchor(" Foo"), "foo");
        assert_eq!(anchor("FOO"), "foo");
        assert_eq!(anchor("Foo Bar"), "foo-bar");
        assert_eq!(anchor("Hi-Hat"), "hi-hat");
        assert_eq!(anchor("Foo %1-2"), "foo-1-2");
        assert_eq!(anchor("double--dash"), "double-dash");
        assert_eq!(anchor("triple---dash"), "triple-dash");
        assert_eq!(anchor("end- dash"), "end--dash");
    }
}
