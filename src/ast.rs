use {
    intervaltree::{Element, IntervalTree},
    lsp_types::{Position, Range},
    pulldown_cmark::{Event, Parser, Tag},
    std::{
        convert::{Into, TryFrom, TryInto},
        iter::FromIterator,
        string::String,
    },
};

fn to_offset(position: &Position, linebreaks: &[usize]) -> Option<usize> {
    let line: usize = position.line.try_into().ok()?;
    let char: usize = position.character.try_into().ok()?;

    let char_prev_lines: usize = linebreaks
        .iter()
        .take(line)
        .map(|c| c + 1)
        .last()
        .unwrap_or(0);

    Some(char_prev_lines + char)
}

#[test]
fn test_to_offset() {
    assert_eq!(to_offset(&Position::new(0, 5), &[]), Some(5));
    assert_eq!(to_offset(&Position::new(0, 0), &[1]), Some(0));
    assert_eq!(to_offset(&Position::new(1, 0), &[2]), Some(3));
    assert_eq!(to_offset(&Position::new(2, 0), &[2, 4]), Some(5));
}

fn to_position(offset: usize, linebreaks: &[usize]) -> Option<Position> {
    let (a, _): (Vec<usize>, _) = linebreaks.iter().partition(|&c| *c < offset);

    let line = a.len().try_into().ok()?;
    let character = match a.last() {
        Some(c) => offset - c - 1, // Newline is not visible.
        None => offset,
    }
    .try_into()
    .ok()?;

    Some(Position::new(line, character))
}

#[derive(Debug, PartialEq, Clone)]
pub struct Node<'a> {
    pub data: Event<'a>,
    pub range: Range,
    pub offsets: std::ops::Range<usize>,
    pub anchor: Option<String>,
}

#[test]
fn test_offset_to_pos() {
    assert_eq!(
        to_position(0, &[]),
        Some(Position {
            line: 0,
            character: 0
        })
    );

    assert_eq!(
        to_position(1, &[5]),
        Some(Position {
            line: 0,
            character: 1
        },)
    );

    assert_eq!(
        to_position(5, &[5]),
        Some(Position {
            line: 0,
            character: 5
        },)
    );

    assert_eq!(
        to_position(10, &[5]),
        Some(Position {
            line: 1,
            character: 4
        })
    );
}

fn get_linebreaks<P: Into<String>>(input: P) -> Vec<usize> {
    // TODO(bbannier): This could probably be made more efficient if callers could exploit that the
    // result is an ordered set.
    input
        .into()
        .chars()
        .enumerate()
        .filter(|(_, c)| *c == '\n')
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn test_get_linebreaks() {
    assert_eq!(get_linebreaks("A"), Vec::<usize>::new());
    assert_eq!(get_linebreaks("\n"), vec![0]);
    assert_eq!(get_linebreaks("\n\n"), vec![0, 1]);
    assert_eq!(get_linebreaks("A\n\nA\n"), vec![1, 2, 4]);
}

type AstNodes<'a> = Vec<Node<'a>>;

fn try_from_offset(
    offset: std::ops::Range<usize>,
    linebreaks: &[usize],
) -> Result<Range, &'static str> {
    Ok(Range::new(
        to_position(offset.start, &linebreaks).ok_or("Invalid start position")?,
        to_position(offset.end, &linebreaks).ok_or("Invalid end position")?,
    ))
}

pub fn parse<'a>(input: &'a str, linebreaks: &[usize]) -> Option<AstNodes<'a>> {
    let ast = Parser::new(input)
        .into_offset_iter()
        .map(|(event, range)| {
            Some(Node {
                data: event,
                range: try_from_offset(range.clone(), &linebreaks).ok()?,
                offsets: range,
                anchor: None,
            })
        })
        .filter(Option::is_some)
        .collect::<Option<Vec<_>>>()?;

    let anchors = ast.iter().enumerate().map(|(i, node)| match &node.data {
        Event::Start(Tag::Heading(_)) => ast.iter().skip(i).find_map(|node| match &node.data {
            Event::Text(text) => Some(anchor(text)),
            _ => None,
        }),
        _ => None,
    });

    // TODO(bbannier): repeated anchors should be numbered.

    let ast = ast
        .iter()
        .zip(anchors)
        .map(|(node, anchor)| Node {
            anchor,
            ..node.clone()
        })
        .collect();

    Some(ast)
}

#[cfg(test)]
use {
    pulldown_cmark::{CowStr, Tag::*},
    textwrap::dedent,
};

#[test]
fn parse_simple() {
    let input = dedent(
        "
        # H1

        Foo bar.
        ",
    );

    let linebreaks = get_linebreaks(&input);

    let parse = match parse(&input, &linebreaks) {
        None => panic!(),
        Some(parse) => parse,
    };
    assert_eq!(
        parse,
        vec![
            Node {
                data: Event::Start(Heading(1)),
                range: Range::new(Position::new(1, 0), Position::new(2, 0)),
                offsets: 1..6,
                anchor: Some("h1".to_string()),
            },
            Node {
                data: Event::Text(CowStr::Borrowed("H1")),
                range: Range::new(Position::new(1, 2), Position::new(1, 4)),
                offsets: 3..5,
                anchor: None,
            },
            Node {
                data: Event::End(Heading(1)),
                range: Range::new(Position::new(1, 0), Position::new(2, 0)),
                offsets: 1..6,
                anchor: None,
            },
            Node {
                data: Event::Start(Paragraph),
                range: Range::new(Position::new(3, 0), Position::new(4, 0)),
                offsets: 7..16,
                anchor: None,
            },
            Node {
                data: Event::Text(CowStr::Borrowed("Foo bar.")),
                range: Range::new(Position::new(3, 0), Position::new(3, 8)),
                offsets: 7..15,
                anchor: None,
            },
            Node {
                data: Event::End(Paragraph),
                range: Range::new(Position::new(3, 0), Position::new(4, 0)),
                offsets: 7..16,
                anchor: None,
            },
        ],
        "\n{:?}",
        &input
    );
}

#[derive(Debug)]
pub struct ParsedDocument<'a> {
    // TODO(bbannier): We do not need to store each `Node` twice, but could e.g., reference `ast`
    // for `tree`.
    ast: AstNodes<'a>,
    tree: IntervalTree<usize, Node<'a>>,
    linebreaks: Vec<usize>,
}

impl<'a> ParsedDocument<'a> {
    /// Get all nodes overlapping `position`.
    pub fn at(&self, position: &Position) -> Vec<&Node> {
        let start = match to_offset(position, &self.linebreaks) {
            Some(offset) => offset,
            _ => return vec![],
        };
        let end = 1 + match to_offset(&position, &self.linebreaks) {
            Some(offset) => offset,
            _ => return vec![],
        };

        self.tree
            .query(start..end)
            .map(|e| &e.value)
            .filter(|n| match n.data {
                // Drop end tags as they are redudant with start tags.
                Event::End(_) => false,
                _ => true,
            })
            .collect()
    }

    pub fn nodes(&self) -> &AstNodes<'a> {
        &self.ast
    }
}

impl<'a> TryFrom<&'a str> for ParsedDocument<'a> {
    type Error = &'static str;

    fn try_from(input: &'a str) -> Result<Self, Self::Error> {
        let linebreaks = get_linebreaks(input);

        let ast = parse(&input, &linebreaks).ok_or("could not parse input")?;

        let tree: IntervalTree<usize, Node<'a>> = IntervalTree::from_iter(ast.iter().map(|n| {
            let range = n.offsets.start..n.offsets.end;
            Element {
                range,
                value: (*n).clone(),
            }
        }));

        Ok(ParsedDocument {
            ast,
            tree,
            linebreaks,
        })
    }
}

#[test]
fn test_query() {
    let s = "asdasad sdaasa aasd asdasdasdada";
    let ast = ParsedDocument::try_from(s).unwrap();

    assert_eq!(
        ast.at(&Position::new(0, 0)),
        vec![
            &Node {
                data: Event::Text(CowStr::Borrowed("asdasad sdaasa aasd asdasdasdada")),
                range: Range::new(Position::new(0, 0), Position::new(0, 32)),
                offsets: 0..32,
                anchor: None
            },
            &Node {
                data: Event::Start(Paragraph),
                range: Range::new(Position::new(0, 0), Position::new(0, 32)),
                offsets: 0..32,
                anchor: None
            }
        ]
    );
}

fn anchor(text: &str) -> String {
    let mut s = text
        .trim()
        .replace("-", "--")
        .replace(" ", "-")
        .to_lowercase();
    s.retain(|c| c.is_alphanumeric() || c == '-');

    s
}

#[test]
fn test_anchor() {
    assert_eq!(anchor("Foo"), "foo");
    assert_eq!(anchor(" Foo"), "foo");
    assert_eq!(anchor("FOO"), "foo");
    assert_eq!(anchor("Foo Bar"), "foo-bar");
    assert_eq!(anchor("Hi-Hat"), "hi--hat");
    assert_eq!(anchor("Foo %1-2"), "foo-1--2");
}
