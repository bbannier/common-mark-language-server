use {
    crate::ast,
    log::info,
    lsp_server::{Connection, Message, RequestId, Response},
    lsp_types::{
        notification::{DidChangeTextDocument, DidOpenTextDocument},
        request::{Completion, HoverRequest},
        CompletionItem, Hover, HoverContents, InitializeParams, MarkedString, ServerCapabilities,
        TextDocumentPositionParams,
    },
    pulldown_cmark::{Event, Tag},
    std::{collections::HashMap, convert::TryFrom, error::Error, path::Path},
    url::Url,
};

fn request_cast<R>(req: lsp_server::Request) -> Result<(RequestId, R::Params), lsp_server::Request>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}

fn notification_cast<N>(
    not: lsp_server::Notification,
) -> Result<N::Params, lsp_server::Notification>
where
    N: lsp_types::notification::Notification,
    N::Params: serde::de::DeserializeOwned,
{
    not.extract(N::METHOD)
}

pub struct Server {
    connection: Connection,
    // TODO(bbannier): Take versions into account.
    documents: HashMap<Url, String>,
    root_uri: Url,
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(lsp_types::TextDocumentSyncCapability::Kind(
            lsp_types::TextDocumentSyncKind::Full,
        )),
        completion_provider: Some(lsp_types::CompletionOptions {
            trigger_characters: Some(vec!["](".into()]),
            ..Default::default()
        }),
        hover_provider: Some(true),
        ..Default::default()
    }
}
pub fn run_server(connection: Connection) -> Result<(), Box<dyn Error + Send + Sync>> {
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
    pub fn main_loop(&mut self) -> Result<(), Box<dyn Error + Sync + Send>> {
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
                    match request_cast::<Completion>(req) {
                        Ok((id, params)) => {
                            self.handle_completion(id, params)?;
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
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        info!("got hover request #{}: {:?}", id, params);
        let uri = params.text_document.uri;
        let contents = match self.documents.get(&uri) {
            Some(xs) => xs,
            None => {
                info!("did not find file '{}' in database", &uri);
                return Ok(());
            }
        };

        // We select the node with the shortest range overlapping the range.
        let ast = ast::ParsedDocument::try_from(contents.as_str())?;
        let nodes = ast.at(&params.position);
        let node = nodes
            .iter()
            .min_by(|x, y| x.offsets.len().cmp(&y.offsets.len()));

        let result = node.map(|node| Hover {
            // TODO(bbannier): Maybe just introduce a `Into<MarkedString>` for the data.
            contents: HoverContents::Array(vec![MarkedString::String(pretty(&node))]),
            range: Some(node.range),
        });

        let result = serde_json::to_value(&result)?;
        let resp = Response {
            id,
            result: Some(result),
            error: None,
        };
        self.connection.sender.send(Message::Response(resp))?;

        Ok(())
    }

    fn handle_completion(
        &self,
        id: lsp_server::RequestId,
        params: lsp_types::CompletionParams,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        // For now just complete anchors.
        let items = self
            .documents
            .iter()
            .filter_map(|(uri, document)| {
                let anchors = ast::ParsedDocument::try_from(document.as_str())
                    .ok()?
                    .nodes()
                    .iter()
                    .filter_map(|node| match &node.anchor {
                        Some(anchor) => {
                            let label = {
                                if uri == &params.text_document_position.text_document.uri {
                                    format!("#{}", anchor)
                                } else {
                                    format!(
                                        "#{}/{}",
                                        make_relative(uri, &self.root_uri)
                                            .expect("file expected to be in workspace"),
                                        anchor
                                    )
                                }
                            };

                            let detail = &document[node.offsets.clone()];

                            Some((label, detail))
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
            .map(|(label, detail)| CompletionItem {
                label,
                detail: Some(detail.into()),
                ..Default::default()
            })
            .collect::<Vec<CompletionItem>>();

        let resp = Response {
            id,
            result: Some(serde_json::to_value(items)?),
            error: None,
        };
        self.connection.sender.send(Message::Response(resp))?;

        Ok(())
    }

    fn handle_did_open_text_document(
        &mut self,
        params: lsp_types::DidOpenTextDocumentParams,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        self.update_document(uri, text)
    }

    fn handle_did_change_text_document(
        &mut self,
        mut params: lsp_types::DidChangeTextDocumentParams,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let uri = params.text_document.uri;
        let text = params
            .content_changes
            .pop()
            .ok_or_else(|| "empty changes".to_string())?
            .text;

        self.update_document(uri, text)
    }

    fn update_document(
        &mut self,
        uri: Url,
        text: String,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.documents.insert(uri, text);

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

#[cfg(test)]
mod tests {
    use {
        super::*,
        lsp_server::Connection,
        lsp_types::{
            notification::{Exit, Initialized, Notification},
            request::{Initialize, Request, Shutdown},
            ClientCapabilities, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
            DidOpenTextDocumentParams, HoverContents, InitializedParams, MarkedString, Position,
            Range, TextDocumentIdentifier, TextDocumentItem, VersionedTextDocumentIdentifier,
        },
        serde::{Deserialize, Serialize},
        std::cell::Cell,
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

    // This test checks hover handling, and as side effects also the loading of docs via
    // `DidOpenTextDocument` and modification via `DidChangeTextDocument` notifications.
    #[test]
    fn handle_hover() {
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
    fn handle_completion() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "markdown".into(),
                1,
                r#"
# heading
[reference](
"#
                .into(),
            ),
        });

        assert_eq!(
            server.send_request::<Completion>(CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri.clone()),
                    Position::new(1, 12),
                ),
                context: None,
            }),
            Some(CompletionResponse::from(vec![CompletionItem::new_simple(
                "#heading".into(),
                "# heading\n".into()
            )])),
            "Completion at heading should not complete anything"
        );

        // // FIXME(bbannier): Make this test pass
        // assert_eq!(
        //     server.send_request::<Completion>(CompletionParams {
        //         text_document_position: TextDocumentPositionParams::new(
        //             TextDocumentIdentifier::new(uri.clone()),
        //             Position::new(0, 0),
        //         ),
        //         context: None,
        //     }),
        //     None,
        //     "Completion at heading should not complete anything"
        // );
    }
}
