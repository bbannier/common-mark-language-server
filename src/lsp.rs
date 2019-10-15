use {
    crate::ast,
    log::info,
    lsp_server::{Connection, Message, RequestId, Response},
    lsp_types::{
        notification::{DidChangeTextDocument, DidOpenTextDocument, Notification},
        request::{
            Completion, FoldingRangeRequest, GotoDefinition, GotoDefinitionResponse, HoverRequest,
            References, Request,
        },
        CompletionItem, CompletionOptions, CompletionParams, DidChangeTextDocumentParams,
        DidOpenTextDocumentParams, FoldingRange, FoldingRangeKind, FoldingRangeParams,
        FoldingRangeProviderCapability, Hover, HoverContents, InitializeParams, Location,
        MarkedString, ReferenceParams, ServerCapabilities, TextDocumentPositionParams,
        TextDocumentSyncCapability, TextDocumentSyncKind,
    },
    pulldown_cmark::{Event, Tag},
    std::{
        collections::HashMap,
        convert::{TryFrom, TryInto},
        error::Error,
        path::Path,
    },
    url::Url,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Sync + Send>>;

fn request_cast<R>(
    req: lsp_server::Request,
) -> std::result::Result<(RequestId, R::Params), lsp_server::Request>
where
    R: Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}

fn notification_cast<N>(
    not: lsp_server::Notification,
) -> std::result::Result<N::Params, lsp_server::Notification>
where
    N: Notification,
    N::Params: serde::de::DeserializeOwned,
{
    not.extract(N::METHOD)
}

rental! {
    pub mod rentals {
    use ast::ParsedDocument;
        #[rental(covariant)]
        pub struct Document {
            document: String,
            parsed: ParsedDocument<'document>,
        }
    }
}

pub struct Server {
    connection: Connection,
    // TODO(bbannier): Take versions into account.
    documents: HashMap<Url, rentals::Document>,
    root_uri: Url,
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::Full)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec!["](".into()]),
            ..Default::default()
        }),
        hover_provider: Some(true),
        references_provider: Some(true),
        definition_provider: Some(true),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        ..Default::default()
    }
}
pub fn run_server(connection: Connection) -> Result<()> {
    let server_capabilities = server_capabilities();
    let initialize_params = connection.initialize(serde_json::to_value(server_capabilities)?)?;
    // FIXME(bbannier): use these.
    let initialize_params: InitializeParams = serde_json::from_value(initialize_params)?;

    let cwd = Url::from_file_path(std::env::current_dir()?).ok();
    let root_path = {
        if let Some(root_path) = initialize_params.root_path {
            Url::from_file_path(root_path).ok()
        } else {
            None
        }
    };

    let root_uri = initialize_params
        .root_uri
        .unwrap_or_else(|| root_path.unwrap_or_else(|| cwd.expect("could not determie root_uri")));

    let mut server = Server {
        connection,
        documents: HashMap::new(),
        root_uri,
    };

    server.main_loop()
}

impl Server {
    pub fn main_loop(&mut self) -> Result<()> {
        info!("starting example main loop");

        while let Some(msg) = self.connection.receiver.iter().next() {
            match msg {
                Message::Request(req) => {
                    if self.connection.handle_shutdown(&req)? {
                        return Ok(());
                    }
                    let req = match request_cast::<HoverRequest>(req) {
                        Ok((id, params)) => {
                            self.handle_hover(id, params)?;
                            continue;
                        }
                        Err(req) => req,
                    };
                    let req = match request_cast::<Completion>(req) {
                        Ok((id, params)) => {
                            self.handle_completion(id, params)?;
                            continue;
                        }
                        Err(req) => req,
                    };
                    let req = match request_cast::<References>(req) {
                        Ok((id, params)) => {
                            self.handle_references(id, params)?;
                            continue;
                        }
                        Err(req) => req,
                    };
                    let req = match request_cast::<GotoDefinition>(req) {
                        Ok((id, params)) => {
                            self.handle_gotodefinition(id, params)?;
                            continue;
                        }
                        Err(req) => req,
                    };
                    match request_cast::<FoldingRangeRequest>(req) {
                        Ok((id, params)) => {
                            self.handle_folding_range_request(id, params)?;
                            continue;
                        }
                        Err(req) => req,
                    };
                }
                Message::Response(_resp) => {}
                Message::Notification(not) => {
                    let not = match notification_cast::<DidOpenTextDocument>(not) {
                        Ok(params) => {
                            self.handle_did_open_text_document(params)?;
                            continue;
                        }
                        Err(not) => not,
                    };
                    match notification_cast::<DidChangeTextDocument>(not) {
                        Ok(params) => {
                            self.handle_did_change_text_document(params)?;
                            continue;
                        }
                        Err(not) => not,
                    };
                }
            }
        }

        info!("finished example main loop");

        Ok(())
    }

    fn handle_hover(
        &self,
        id: lsp_server::RequestId,
        params: TextDocumentPositionParams,
    ) -> Result<()> {
        info!("got hover request #{}: {:?}", id, params);
        let uri = params.text_document.uri;
        let document = match self.documents.get(&uri) {
            Some(document) => document.all(),
            None => {
                info!("did not find file '{}' in database", &uri);
                self.connection
                    .sender
                    .send(Message::Response(Response::new_ok(
                        id,
                        Option::<HoverContents>::None,
                    )))?;
                return Ok(());
            }
        };

        // We select the node with the shortest range overlapping the range.
        let nodes = document.parsed.at(&params.position);
        let node = nodes
            .iter()
            .min_by(|x, y| x.offsets.len().cmp(&y.offsets.len()));

        let result = node.map(|node| Hover {
            // TODO(bbannier): Maybe just introduce a `Into<MarkedString>` for the data.
            contents: HoverContents::Array(vec![MarkedString::String(pretty(&node))]),
            range: Some(node.range),
        });

        self.connection
            .sender
            .send(Message::Response(Response::new_ok(id, result)))?;
        Ok(())
    }

    fn handle_completion(&self, id: lsp_server::RequestId, params: CompletionParams) -> Result<()> {
        // Do a simple check whether we are actually completing a link. We only check whether the
        // character before the completion position is a literal `](`.
        let good_position = match self
            .documents
            .get(&params.text_document_position.text_document.uri)
        {
            None => false, // Document unknown.
            Some(document) => {
                let position = &params.text_document_position.position;
                let character: usize = position.character.try_into()?;
                character >= 2
                    && document
                        .all()
                        .document
                        .lines()
                        .nth(position.line.try_into()?)
                        .unwrap()[character - 2..character]
                        == *"]("
            }
        };
        if !good_position {
            self.connection
                .sender
                .send(Message::Response(Response::new_ok(
                    id.clone(),
                    Vec::<CompletionItem>::new(),
                )))?;
            return Ok(());
        }

        // For now just complete anchors.
        let items = self
            .documents
            .iter()
            .filter_map(|(uri, document)| {
                let document = document.all();
                let anchors = document
                    .parsed
                    .nodes()
                    .iter()
                    .filter_map(|node| match &node.anchor {
                        Some(anchor) => {
                            let detail = &document.document[node.offsets.clone()];
                            let reference = full_reference(
                                (&anchor, uri),
                                &self.root_uri,
                                &params.text_document_position.text_document.uri,
                            )?;

                            Some((reference, detail))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();

                if anchors.is_empty() {
                    None
                } else {
                    Some(anchors)
                }
            })
            .flatten()
            .map(|(label, detail)| CompletionItem::new_simple(label, detail.into()))
            .collect::<Vec<CompletionItem>>();

        self.connection
            .sender
            .send(Message::Response(Response::new_ok(id, items)))?;
        Ok(())
    }

    fn handle_references(&self, id: lsp_server::RequestId, params: ReferenceParams) -> Result<()> {
        let text_document_position = params.text_document_position;

        let nodes: Vec<_> = self
            .documents
            .get(&text_document_position.text_document.uri)
            .iter()
            .flat_map(|document| document.all().parsed.at(&text_document_position.position))
            .collect();

        let (anchor, anchor_range) = match nodes
            .iter()
            .filter(|node| match &node.data {
                Event::Start(Tag::Link(_, _, _)) => true,
                _ => false,
            })
            .min_by_key(|node| node.offsets.len())
            .map(|node| match &node.data {
                Event::Start(Tag::Link(_, dest, _)) => (
                    String::from(dest.as_ref()).trim_start_matches('#').into(),
                    node.range,
                ),
                _ => unreachable!(),
            })
            .or_else(|| {
                nodes
                    .iter()
                    .filter(|node| node.anchor.is_some())
                    .min_by_key(|node| node.offsets.len())
                    .map(|node| {
                        (
                            node.anchor
                                .as_ref()
                                .expect("we should have filtered for some anchors previously")
                                .clone(),
                            node.range,
                        )
                    })
            }) {
            Some((anchor, range)) => (anchor, range),
            _ => {
                // No anchor found at position, return empty result.
                self.connection
                    .sender
                    .send(Message::Response(Response::new_ok(
                        id,
                        Option::<Vec<Location>>::None,
                    )))?;
                return Ok(());
            }
        };

        let declaration = if params.context.include_declaration {
            vec![Location::new(
                text_document_position.text_document.uri.clone(),
                anchor_range,
            )]
        } else {
            vec![]
        };

        let result = self
            .documents
            .iter()
            .flat_map(move |(uri, document)| {
                let uri = uri;
                let request_uri = text_document_position.text_document.uri.clone();
                let anchor = anchor.clone();
                document
                    .all()
                    .parsed
                    .nodes()
                    .iter()
                    .filter_map(move |node| match &node.data {
                        Event::Start(Tag::Link(_, reference, _))
                            if reference.as_ref()
                                == full_reference(
                                    (&anchor, uri),
                                    &self.root_uri,
                                    &request_uri,
                                )? =>
                        {
                            Some(Location::new(uri.clone(), node.range))
                        }
                        _ => None,
                    })
            })
            .chain(declaration.iter().cloned())
            .collect::<Vec<_>>();

        self.connection
            .sender
            .send(Message::Response(Response::new_ok(id, result)))?;
        Ok(())
    }

    fn handle_gotodefinition(
        &self,
        id: lsp_server::RequestId,
        params: TextDocumentPositionParams,
    ) -> Result<()> {
        let result: Option<GotoDefinitionResponse> = self
            .documents
            .get(&params.text_document.uri)
            .and_then(|document| {
                // Extract any link at the current position.
                document
                    .all()
                    .parsed
                    .at(&params.position)
                    .iter()
                    .find_map(|node| match &node.data {
                        Event::Start(Tag::Link(_, dest, _)) => Some(dest.as_ref()),
                        _ => None,
                    })
            })
            .and_then(|dest| {
                // Translate reference to uri & anchor.
                let dest = if dest.starts_with('#') {
                    dest.trim_start_matches('#')
                } else {
                    // Does not look like a local reference.
                    return None;
                };

                let components: Vec<_> = dest.rsplitn(2, '/').collect();
                let anchor = components[0];
                let uri = if components.len() == 2 {
                    Url::from_file_path(format!("{}/{}", self.root_uri, components[1])).ok()?
                } else {
                    params.text_document.uri
                };

                Some((uri, anchor))
            })
            .and_then(|(uri, anchor)| {
                // Obtain dest node and create response.
                self.documents.get(&uri).and_then(|document| {
                    document.all().parsed.nodes().iter().find_map(|node| {
                        if let Some(node_anchor) = &node.anchor {
                            if node_anchor == anchor {
                                return Some(Location::new(uri.clone(), node.range).into());
                            }
                        }
                        None
                    })
                })
            });

        self.connection
            .sender
            .send(Message::Response(Response::new_ok(id, result)))?;
        Ok(())
    }

    fn handle_folding_range_request(
        &self,
        id: lsp_server::RequestId,
        params: FoldingRangeParams,
    ) -> Result<()> {
        let result: Option<Vec<_>> =
            self.documents
                .get(&params.text_document.uri)
                .map(|document| {
                    let nodes = document.all().parsed.nodes();

                    let last_node = nodes.iter().max_by_key(|node| node.offsets.end);

                    let headings = {
                        let mut xs = nodes
                            .iter()
                            .filter_map(|node| match &node.data {
                                Event::Start(Tag::Heading(level)) => Some((level, node)),
                                _ => None,
                            })
                            .collect::<Vec<_>>();

                        // Ensure correct ordering as we use these to look up the next section below.
                        xs.sort_unstable_by_key(|(_, x)| x.offsets.start);

                        xs
                    };

                    nodes
                        .iter()
                        .filter_map(|node| match &node.data {
                            Event::Start(Tag::Heading(level)) => {
                                // Translate headings into sections.

                                // We can reuse a heading's start tag, but need to generate a corresponding end tag.
                                let end =
                                    headings
                                        .iter()
                                        .skip_while(|(_, n)| n.range != node.range)
                                        .skip(1).skip_while(|(&l, _)| l > *level)
                                        .next()
                                        . map(|(_,n)|
                                            // We let the range end before the next section. This
                                            // is safe as we need to have lines preceeding the
                                            // _next_ section.
                                            n.range.start.line - 1)
                                        .unwrap_or(last_node.expect("if we iterate anything at all there should be a last node").range.end.line)
                                    ;

                                Some(FoldingRange {
                                    start_line: node.range.start.line,
                                    start_character: None,
                                    end_line: end,
                                    end_character: None,
                                    kind: Some(FoldingRangeKind::Region),
                                })
                            }
                            Event::Start(_) => Some(FoldingRange {
                                start_line: node.range.start.line,
                                start_character: None,
                                end_line: node.range.end.line,
                                end_character: None,
                                kind: Some(FoldingRangeKind::Region),
                            }),
                            _ => None,
                        })
                        .collect()
                });

        self.connection
            .sender
            .send(Message::Response(Response::new_ok(id, result)))?;
        Ok(())
    }

    fn handle_did_open_text_document(&mut self, params: DidOpenTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        self.update_document(uri, text)
    }

    fn handle_did_change_text_document(
        &mut self,
        mut params: DidChangeTextDocumentParams,
    ) -> Result<()> {
        let uri = params.text_document.uri;
        let text = params
            .content_changes
            .pop()
            .ok_or_else(|| "empty changes".to_string())?
            .text;

        self.update_document(uri, text)
    }

    fn update_document(&mut self, uri: Url, text: String) -> Result<()> {
        #[allow(clippy::redundant_closure)]
        let document =
            match rentals::Document::try_new(text, |text| ast::ParsedDocument::try_from(text)) {
                Ok(document) => document,
                Err(_) => {
                    return Ok(());
                }
            };

        self.documents.insert(uri, document);

        Ok(())
    }
}

fn pretty_link(
    link_type: pulldown_cmark::LinkType,
    dest: &pulldown_cmark::CowStr,
    title: &pulldown_cmark::CowStr,
) -> String {
    let link_type = match link_type {
        pulldown_cmark::LinkType::Inline => "inline",
        pulldown_cmark::LinkType::Reference => "reference",
        pulldown_cmark::LinkType::Collapsed => "collapsed",
        pulldown_cmark::LinkType::Shortcut => "shortcut",
        pulldown_cmark::LinkType::Autolink => "autolink",
        pulldown_cmark::LinkType::Email => "email address",
        pulldown_cmark::LinkType::CollapsedUnknown => {
            "collapsed link without destination in document"
        }
        pulldown_cmark::LinkType::ReferenceUnknown => {
            "reference link without destination in document"
        }
        pulldown_cmark::LinkType::ShortcutUnknown => {
            "shortcut link without destination in document"
        }
    }
    .to_string();

    let mut result = vec![link_type];
    if !dest.is_empty() {
        result.push(format!("destination: {}", dest));
    }
    if !title.is_empty() {
        result.push(format!("title: {}", title));
    }

    result.join(", ")
}

fn pretty(node: &ast::Node) -> String {
    let event = &node.data;
    let event = match event {
        Event::Code(_) => "Inline code".to_string(),
        Event::Start(tag) | Event::End(tag) => match tag {
            Tag::Paragraph => "Paragraph".to_string(),
            Tag::Heading(level) => format!("Heading (level: {})", level),
            Tag::BlockQuote => "Blockquote".to_string(),
            Tag::CodeBlock(_) => "Code block".to_string(),
            Tag::Emphasis => "Emphasis".to_string(),
            Tag::FootnoteDefinition(_) => "Footnote definition".to_string(),
            Tag::Image(link_type, dest, title) => {
                format!("Image ({})", pretty_link(*link_type, dest, title))
            }
            Tag::Link(link_type, dest, title) => {
                format!("Link ({})", pretty_link(*link_type, dest, title))
            }
            Tag::Item => "Item".to_string(),
            Tag::List(option) => match option {
                Some(option) => format!("List (first item: {})", option),
                None => "List".to_string(),
            },
            Tag::Strikethrough => "Strikethrough".to_string(),
            Tag::Strong => "Strong".to_string(),
            Tag::Table(alignment) => format!(
                "Table (alignment: {})",
                alignment
                    .iter()
                    .map(|align| match align {
                        pulldown_cmark::Alignment::None => "none",
                        pulldown_cmark::Alignment::Center => "center",
                        pulldown_cmark::Alignment::Left => "left",
                        pulldown_cmark::Alignment::Right => "right",
                    }
                    .to_string())
                    .collect::<Vec<String>>()
                    .join(" | ")
            ),
            Tag::TableCell => "Table cell".to_string(),
            Tag::TableRow => "Table row".to_string(),
            Tag::TableHead => "Table head".to_string(),
        },
        Event::FootnoteReference(_) => "Footnote reference".to_string(),
        Event::SoftBreak => "Soft break".to_string(),
        Event::HardBreak => "Hard break".to_string(),
        Event::Html(_) => "Html".to_string(),
        Event::Rule => "Rule".to_string(),
        Event::TaskListMarker(_) => "Task list marker".to_string(),
        Event::Text(_) => "Text".to_string(),
    };

    match &node.anchor {
        None => event.to_string(),
        Some(anchor) => format!("{}\nanchor: {}", event, anchor),
    }
}

fn full_reference(target: (&str, &Url), base: &Url, source: &Url) -> Option<String> {
    Some(if target.1 == source {
        format!("#{}", target.0)
    } else {
        format!("#{}/{}", make_relative(target.1, base)?, target.0)
    })
}

fn make_relative(file: &Url, base: &Url) -> Option<String> {
    let file = Path::new(file.path());
    let root_uri = Path::new(base.path());

    // This assumes `file` is under `base` which seems reasonable.
    file.strip_prefix(root_uri)
        .map(|path| {
            path.to_str()
                .expect("unable to convert path back into string which should be possible")
                .into()
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        lsp_server::Connection,
        lsp_types::{
            notification::{Exit, Initialized, Notification},
            request::{Initialize, References, Request, Shutdown},
            ClientCapabilities, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
            DidOpenTextDocumentParams, HoverContents, InitializedParams, Location, MarkedString,
            Position, Range, ReferenceContext, ReferenceParams, TextDocumentIdentifier,
            TextDocumentItem, VersionedTextDocumentIdentifier,
        },
        serde::{Deserialize, Serialize},
        std::cell::Cell,
        textwrap::dedent,
    };

    struct TestServer {
        _thread: jod_thread::JoinHandle<()>,
        client: Connection,
        req_id: Cell<u64>,
    }

    impl TestServer {
        fn new() -> TestServer {
            let (connection, client) = Connection::memory();
            let _thread = jod_thread::Builder::new()
                .name("test server".to_string())
                .spawn(|| {
                    run_server(connection).unwrap();
                })
                .unwrap();

            let req_id = Cell::new(0);

            let server = TestServer {
                _thread,
                client,
                req_id,
            };

            server.send_request::<Initialize>(InitializeParams {
                capabilities: ClientCapabilities::default(),
                initialization_options: None,
                process_id: None,
                root_path: None,
                root_uri: None,
                trace: None,
                workspace_folders: None,
            });

            server.send_notification::<Initialized>(InitializedParams {});

            server
        }

        fn send_request<R>(&self, params: R::Params) -> R::Result
        where
            R: Request,
            R::Params: Serialize,
            for<'de> <R as Request>::Result: Deserialize<'de>,
        {
            let id = self.req_id.get();
            self.req_id.set(id + 1);

            self.client
                .sender
                .send(lsp_server::Message::from(lsp_server::Request::new(
                    id.into(),
                    R::METHOD.into(),
                    params,
                )))
                .unwrap();

            let response = match self.client.receiver.recv().unwrap() {
                lsp_server::Message::Response(response) => response,
                _ => panic!(),
            }
            .result
            .unwrap();

            serde_json::from_value(response).unwrap()
        }

        fn send_notification<N>(&self, params: N::Params)
        where
            N: Notification,
            N::Params: Serialize,
        {
            let not = lsp_server::Notification::new(N::METHOD.into(), params);
            self.client
                .sender
                .send(lsp_server::Message::Notification(not))
                .unwrap();
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.send_request::<Shutdown>(());
            self.send_notification::<Exit>(());
        }
    }

    #[test]
    fn test_make_relative() {
        let root_uri = Url::from_file_path("/foo/bar").unwrap();
        assert_eq!(
            make_relative(&Url::from_file_path("/foo/bar/baz.md").unwrap(), &root_uri).unwrap(),
            "baz.md"
        );

        assert_eq!(
            make_relative(
                &Url::from_file_path("/foo/bar/baz/quz.md").unwrap(),
                &root_uri
            )
            .unwrap(),
            "baz/quz.md"
        );
    }

    #[test]
    fn test_full_reference() {
        let uri = Url::from_file_path("/foo/bar.md").unwrap();
        let anchor = "baz";
        let base = Url::from_file_path("/foo").unwrap();
        let source = Url::from_file_path("/foo/quaz.md").unwrap();

        assert_eq!(
            full_reference((anchor, &uri), &base, &uri),
            Some("#baz".into())
        );
        assert_eq!(
            full_reference((anchor, &uri), &base, &source),
            Some("#bar.md/baz".into())
        );
    }

    // This test checks hover handling, and as side effects also the loading of docs via
    // `DidOpenTextDocument` and modification via `DidChangeTextDocument` notifications.
    #[test]
    fn hover() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo/bar.md").unwrap();

        // Prime the server with a document with a heading.
        server.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "markdown".into(),
                1,
                "# heading".into(),
            ),
        });

        assert_eq!(
            server.send_request::<HoverRequest>(TextDocumentPositionParams::new(
                TextDocumentIdentifier { uri: uri.clone() },
                Position::new(0, 0)
            ),),
            Some(Hover {
                contents: HoverContents::Array(vec![MarkedString::from_markdown(
                    "Heading (level: 1)\nanchor: heading".to_string()
                )]),
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 9))),
            }),
            "The first character should match a heading"
        );

        assert_eq!(
            server.send_request::<HoverRequest>(TextDocumentPositionParams::new(
                TextDocumentIdentifier { uri: uri.clone() },
                Position::new(0, 2)
            ),),
            Some(Hover {
                contents: HoverContents::Array(vec![MarkedString::from_markdown(
                    "Text".to_string()
                )]),
                range: Some(Range::new(Position::new(0, 2), Position::new(0, 9))),
            }),
            "The third character should match text"
        );

        // Change the document to contain inline code.
        server.send_notification::<DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier::new(uri.clone(), 2),
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                text: "`int main()`".into(),
                range: None,
                range_length: None,
            }],
        });

        assert_eq!(
            server.send_request::<HoverRequest>(TextDocumentPositionParams::new(
                TextDocumentIdentifier { uri: uri.clone() },
                Position::new(0, 3)
            ),),
            Some(Hover {
                contents: HoverContents::Array(vec![MarkedString::from_markdown(
                    "Inline code".to_string()
                )]),
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 12))),
            }),
            "The fourth character should match inline code"
        );
    }

    #[test]
    fn completion() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    # heading
                    [reference](
                    ",
                ),
            ),
        });

        assert_eq!(
            server.send_request::<Completion>(CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri.clone()),
                    Position::new(2, 12),
                ),
                context: None,
            }),
            Some(CompletionResponse::from(vec![CompletionItem::new_simple(
                "#heading".into(),
                "# heading\n".into()
            )])),
            "Completion at reference should complete heading"
        );

        assert_eq!(
            server.send_request::<Completion>(CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri.clone()),
                    Position::new(2, 2),
                ),
                context: None,
            }),
            Some(CompletionResponse::from(vec![])),
            "Completion in the middle of reference should not complete anything",
        );

        assert_eq!(
            server.send_request::<Completion>(CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri.clone()),
                    Position::new(1, 0),
                ),
                context: None,
            }),
            Some(CompletionResponse::from(vec![])),
            "Completion at heading should not complete anything"
        );
    }

    #[test]
    fn references() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    # h1
                    [ref1](#h1)
                    [ref2](#h2)

                    # h2
                    [ref1](#h1)
                    ",
                ),
            ),
        });

        assert_eq!(
            server.send_request::<References>(ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.clone()),
                    position: Position::new(0, 0),
                },
                context: ReferenceContext {
                    include_declaration: false,
                },
            }),
            None
        );

        assert_eq!(
            server.send_request::<References>(ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.clone()),
                    position: Position::new(1, 0),
                },
                context: ReferenceContext {
                    include_declaration: true,
                },
            }),
            Some(vec![
                Location::new(
                    uri.clone(),
                    Range::new(Position::new(2, 0), Position::new(2, 11))
                ),
                Location::new(
                    uri.clone(),
                    Range::new(Position::new(6, 0), Position::new(6, 11))
                ),
                Location::new(
                    uri.clone(),
                    Range::new(Position::new(1, 0), Position::new(2, 0))
                ),
            ])
        );

        assert_eq!(
            server.send_request::<References>(ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.clone()),
                    position: Position::new(1, 0),
                },
                context: ReferenceContext {
                    include_declaration: false,
                },
            }),
            Some(vec![
                Location::new(
                    uri.clone(),
                    Range::new(Position::new(2, 0), Position::new(2, 11))
                ),
                Location::new(
                    uri.clone(),
                    Range::new(Position::new(6, 0), Position::new(6, 11))
                ),
            ])
        );

        assert_eq!(
            server.send_request::<References>(ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.clone()),
                    position: Position::new(2, 7),
                },
                context: ReferenceContext {
                    include_declaration: true,
                },
            }),
            Some(vec![
                Location::new(
                    uri.clone(),
                    Range::new(Position::new(2, 0), Position::new(2, 11))
                ),
                Location::new(
                    uri.clone(),
                    Range::new(Position::new(6, 0), Position::new(6, 11))
                ),
                Location::new(
                    uri.clone(),
                    Range::new(Position::new(2, 0), Position::new(2, 11))
                ),
            ])
        );
    }

    #[test]
    fn gotodefinition() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    # h1
                    [ref1](#h1)
                    ",
                ),
            ),
        });

        assert_eq!(
            server.send_request::<GotoDefinition>(TextDocumentPositionParams::new(
                TextDocumentIdentifier::new(uri.clone()),
                Position::new(2, 0),
            )),
            Some(GotoDefinitionResponse::Scalar(Location::new(
                uri,
                Range::new(Position::new(1, 0), Position::new(2, 0))
            )))
        );
    }

    #[test]
    fn folding_range_request() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    # h1

                    Some section here
                    with multiple lines.

                    ## h2

                    This section has a paragraph
                    followed by a code block,

                    ```
                    int main() {{
                    ```
                    ",
                ),
            ),
        });

        assert_eq!(
            server.send_request::<FoldingRangeRequest>(FoldingRangeParams {
                text_document: TextDocumentIdentifier::new(uri)
            }),
            Some(vec![
                FoldingRange {
                    start_line: 1,
                    start_character: None,
                    end_line: 13,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region)
                },
                FoldingRange {
                    start_line: 3,
                    start_character: None,
                    end_line: 5,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region)
                },
                FoldingRange {
                    start_line: 6,
                    start_character: None,
                    end_line: 13,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region)
                },
                FoldingRange {
                    start_line: 8,
                    start_character: None,
                    end_line: 10,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region)
                },
                FoldingRange {
                    start_line: 11,
                    start_character: None,
                    end_line: 13,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region)
                }
            ]),
        );
    }
}
