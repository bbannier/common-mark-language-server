use {
    crate::ast,
    log::info,
    lsp_server::{Connection, Message, Notification, Request, RequestId, Response},
    lsp_types::{
        notification::{DidChangeTextDocument, DidOpenTextDocument},
        request::{Completion, HoverRequest},
        Hover, HoverContents, InitializeParams, MarkedString, ServerCapabilities,
    },
    pulldown_cmark::{Event, Tag},
    std::{collections::HashMap, convert::TryFrom, error::Error, path::Path},
    url::Url,
};

fn request_cast<R>(req: Request) -> Result<(RequestId, R::Params), Request>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}

fn notification_cast<N>(not: Notification) -> Result<N::Params, Notification>
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
        params: lsp_types::TextDocumentPositionParams,
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

        let ast = ast::Query::try_from(contents.as_str())?;

        let result = ast.at(&params.position).map(|node| Hover {
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
                let anchors = ast::Query::try_from(document.as_str())
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
            .map(|(label, detail)| lsp_types::CompletionItem {
                label,
                detail: Some(detail.into()),
                ..Default::default()
            })
            .collect::<Vec<lsp_types::CompletionItem>>();

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
