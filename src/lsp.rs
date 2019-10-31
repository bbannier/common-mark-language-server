use {
    crate::ast,
    crossbeam_channel::{select, Receiver, RecvError, Sender},
    log::{debug, info},
    lsp_server::{Connection, Message, Notification, Request, RequestId, Response},
    lsp_types::*,
    pulldown_cmark as m,
    serde::{Deserialize, Serialize},
    static_assertions::assert_eq_size,
    std::{
        collections::{HashMap, VecDeque},
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
    R: request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}

fn notification_cast<N>(
    not: lsp_server::Notification,
) -> std::result::Result<N::Params, lsp_server::Notification>
where
    N: notification::Notification,
    N::Params: serde::de::DeserializeOwned,
{
    not.extract(N::METHOD)
}

rental! {
    pub mod rentals {
    use ast::ParsedDocument;
        #[rental(covariant)]
        pub struct Document {
            text: String,
            parsed: ParsedDocument<'text>,
        }
    }
}

struct Document {
    version: Option<i64>,
    document: rentals::Document,
    updating: bool,
}

#[derive(Debug)]
enum Task {
    LoadFile(Box<(Url, (Url, Range))>),
    UpdateDocument(Url, String, Option<i64>),
    RunLint,
}

#[derive(Debug)]
enum Event {
    Api(lsp_server::Message),
    Task(Task),
}

struct Tasks {
    sender: Sender<Task>,
    receiver: Receiver<Task>,
}

impl Tasks {
    fn new() -> Tasks {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Tasks { sender, receiver }
    }
}

pub struct Server {
    connection: Connection,
    tasks: Tasks,
    documents: HashMap<Url, Document>,
    root_uri: Url,
}

struct StatusRequest;

#[derive(Debug, Deserialize, Serialize)]
struct StatusResponse {
    is_idle: bool,
}

impl request::Request for StatusRequest {
    type Params = ();
    type Result = StatusResponse;
    const METHOD: &'static str = "common-mark-language-server/status";
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
        document_symbol_provider: Some(true),
        workspace_symbol_provider: Some(true),
        ..Default::default()
    }
}
pub fn run_server(connection: Connection) -> Result<()> {
    let server_capabilities = server_capabilities();
    let initialize_params = connection.initialize(serde_json::to_value(server_capabilities)?)?;
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

    let tasks = Tasks::new();

    let server = Server {
        connection,
        tasks,
        documents: HashMap::new(),
        root_uri,
    };

    main_loop(server)
}

fn main_loop(server: Server) -> Result<()> {
    info!("starting main loop");

    let mut server = server;

    loop {
        #[allow(clippy::drop_copy, clippy::zero_ptr)]
        let event = select! {
            recv(server.connection.receiver) -> msg => match msg {
                Ok(msg) => Event::Api(msg),
                Err(RecvError) => return Err("client exited without shutdown".into()),
            },
            recv(server.tasks.receiver) -> task => match task {
                Ok(task) => Event::Task(task),
                Err(RecvError) => continue,
            }
        };

        debug!("processing event: {:?}", event);

        match event {
            Event::Api(api) => match api {
                Message::Request(req) => {
                    if server.connection.handle_shutdown(&req)? {
                        break;
                    }

                    on_request(req, &mut server)?;
                }
                Message::Notification(not) => {
                    on_notification(not, &mut server)?;
                }
                Message::Response(_resp) => {}
            },
            Event::Task(task) => match task {
                Task::LoadFile(uri_source) => server.load_file(uri_source.0, uri_source.1)?,
                Task::UpdateDocument(uri, document, version) => {
                    server.update_document(uri, document, version)?
                }
                Task::RunLint => server.run_lint()?,
            },
        }
    }

    info!("finished main loop");

    Ok(())
}

fn on_request(req: Request, server: &mut Server) -> Result<()> {
    let req = match request_cast::<request::HoverRequest>(req) {
        Ok((id, params)) => {
            return server.handle_hover(id, params);
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::Completion>(req) {
        Ok((id, params)) => {
            return server.handle_completion(id, params);
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::References>(req) {
        Ok((id, params)) => {
            return server.handle_references(id, params);
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::GotoDefinition>(req) {
        Ok((id, params)) => {
            return server.handle_gotodefinition(id, params);
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::FoldingRangeRequest>(req) {
        Ok((id, params)) => {
            return server.handle_folding_range_request(id, params);
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::DocumentSymbolRequest>(req) {
        Ok((id, params)) => {
            return server.handle_document_symbol_request(id, params);
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::WorkspaceSymbol>(req) {
        Ok((id, params)) => {
            return server.handle_workspace_symbol(id, params);
        }
        Err(req) => req,
    };
    match request_cast::<StatusRequest>(req) {
        Ok((id, _)) => {
            return server.handle_status_request(id);
        }
        Err(req) => req,
    };
    Ok(())
}

fn on_notification(not: Notification, server: &mut Server) -> Result<()> {
    let not = match notification_cast::<notification::DidOpenTextDocument>(not) {
        Ok(params) => {
            return server.handle_did_open_text_document(params);
        }
        Err(not) => not,
    };
    match notification_cast::<notification::DidChangeTextDocument>(not) {
        Ok(params) => {
            return server.handle_did_change_text_document(params);
        }
        Err(not) => not,
    };

    Ok(())
}

impl Server {
    fn response<R>(&self, id: RequestId, response: R) -> Result<()>
    where
        R: Serialize + std::fmt::Debug,
    {
        debug!("sending response: {:?}", response);

        self.connection
            .sender
            .send(Message::Response(Response::new_ok(id, response)))
            .map_err(|err| err.into())
    }

    fn notification<N>(&self, params: N::Params) -> Result<()>
    where
        N: notification::Notification,
        N::Params: Serialize,
    {
        self.connection
            .sender
            .send(Message::Notification(Notification::new(
                N::METHOD.into(),
                params,
            )))
            .map_err(|err| err.into())
    }

    fn add_task(&mut self, task: Task) -> Result<()> {
        debug!("adding task: {:?}", task);

        self.tasks.sender.send(task)?;
        Ok(())
    }

    fn handle_status_request(&mut self, id: lsp_server::RequestId) -> Result<()> {
        // This function does not accept parameters since `StatusRequest` is empty.
        assert_eq_size!(StatusRequest, ());
        self.response(
            id,
            StatusResponse {
                is_idle: self.tasks.receiver.is_empty(),
            },
        )
    }

    fn handle_hover(
        &self,
        id: lsp_server::RequestId,
        params: TextDocumentPositionParams,
    ) -> Result<()> {
        let uri = params.text_document.uri;
        let document = match self.documents.get(&uri) {
            Some(document) => document.document.all(),
            None => {
                info!("did not find file '{}' in database", &uri);
                self.response(id, Option::<HoverContents>::None)?;
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

        self.response(id, result)?;
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
                        .document
                        .all()
                        .text
                        .lines()
                        .nth(position.line.try_into()?)
                        .unwrap()[character - 2..character]
                        == *"]("
            }
        };
        if !good_position {
            self.response(id, Vec::<CompletionItem>::new())?;
            return Ok(());
        }

        // For now just complete anchors.
        let items = self
            .documents
            .iter()
            .filter_map(|(uri, document)| {
                let document = document.document.all();
                let anchors = document
                    .parsed
                    .nodes()
                    .iter()
                    .filter_map(|node| match &node.anchor {
                        Some(anchor) => {
                            let detail = &document.text[node.offsets.clone()];
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

        self.response(id, items)?;
        Ok(())
    }

    fn handle_references(&self, id: lsp_server::RequestId, params: ReferenceParams) -> Result<()> {
        let text_document_position = params.text_document_position;

        let nodes: Vec<_> = self
            .documents
            .get(&text_document_position.text_document.uri)
            .iter()
            .flat_map(|document| {
                document
                    .document
                    .all()
                    .parsed
                    .at(&text_document_position.position)
            })
            .collect();

        let (anchor, anchor_range) = match nodes
            .iter()
            .filter(|node| match &node.data {
                m::Event::Start(m::Tag::Link(_, _, _)) => true,
                _ => false,
            })
            .min_by_key(|node| node.offsets.len())
            .map(|node| match &node.data {
                m::Event::Start(m::Tag::Link(_, dest, _)) => (
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
                self.response(id, Option::<Vec<Location>>::None)?;
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

        let result =
            self.documents
                .iter()
                .flat_map(move |(uri, document)| {
                    let uri = uri;
                    let request_uri = text_document_position.text_document.uri.clone();
                    let anchor = anchor.clone();
                    document.document.all().parsed.nodes().iter().filter_map(
                        move |node| match &node.data {
                            m::Event::Start(m::Tag::Link(_, reference, _))
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
                        },
                    )
                })
                .chain(declaration.iter().cloned())
                .collect::<Vec<_>>();

        self.response(id, result)?;
        Ok(())
    }

    fn handle_gotodefinition(
        &self,
        id: lsp_server::RequestId,
        params: TextDocumentPositionParams,
    ) -> Result<()> {
        let result: Option<request::GotoDefinitionResponse> = self
            .documents
            .get(&params.text_document.uri)
            .and_then(|document| {
                // Extract any link at the current position.
                document
                    .document
                    .all()
                    .parsed
                    .at(&params.position)
                    .iter()
                    .find_map(|node| match &node.data {
                        m::Event::Start(m::Tag::Link(_, dest, _)) => Some(dest.as_ref()),
                        _ => None,
                    })
            })
            .and_then(|dest| {
                self.get_destination(&params.text_document.uri, dest)
                    .map(|location| location.into())
            });

        self.response(id, result)?;
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
                    let nodes = document.document.all().parsed.nodes();

                    let last_node = nodes.iter().max_by_key(|node| node.offsets.end);

                    let headings = {
                        let mut xs = nodes
                            .iter()
                            .filter_map(|node| match &node.data {
                                m::Event::Start(m::Tag::Heading(level)) => Some((level, node)),
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
                            m::Event::Start(m::Tag::Heading(level)) => {
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
                            _ => None,
                        })
                        .collect()
                });

        self.response(id, result)?;
        Ok(())
    }

    fn handle_document_symbol_request(
        &mut self,
        id: RequestId,
        params: DocumentSymbolParams,
    ) -> Result<()> {
        self.response(
            id,
            self.get_symbols(&params.text_document.uri)
                .map(DocumentSymbolResponse::from),
        )?;
        Ok(())
    }

    fn handle_workspace_symbol(
        &mut self,
        id: RequestId,
        params: WorkspaceSymbolParams,
    ) -> Result<()> {
        let result: Vec<_> = self
            .documents
            .keys()
            .map(|uri| match self.get_symbols(uri) {
                Some(symbols) => symbols
                    .iter()
                    .filter(|symbol: &&SymbolInformation| symbol.name.contains(&params.query))
                    .cloned()
                    .collect::<Vec<_>>(),
                None => vec![],
            })
            .flatten()
            .collect();

        self.response(id, result)?;
        Ok(())
    }

    fn handle_did_open_text_document(&mut self, params: DidOpenTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        if let Some(document) = self.documents.get_mut(&uri) {
            document.updating = true;
        }

        self.add_task(Task::UpdateDocument(uri, text, Some(version)))
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

        let version = params.text_document.version;

        if let Some(document) = self.documents.get_mut(&uri) {
            document.updating = true;
        }

        self.add_task(Task::UpdateDocument(uri, text, version))
    }

    fn update_document(&mut self, uri: Url, text: String, version: Option<i64>) -> Result<()> {
        let existing_version = self
            .documents
            .get(&uri)
            .and_then(|document| document.version);
        if existing_version > version {
            info!("not updating {} as more recent version is known", &uri);
            return Ok(());
        }

        info!("updating {}", &uri);

        #[allow(clippy::redundant_closure)]
        let document =
            match rentals::Document::try_new(text, |text| ast::ParsedDocument::try_from(text)) {
                Ok(document) => document,
                Err(err) => {
                    // TODO(bbannier): add a test for parse error notifications.
                    self.notification::<notification::PublishDiagnostics>(
                        PublishDiagnosticsParams::new(
                            uri,
                            vec![Diagnostic::new(
                                Range::new(Position::new(0, 0), Position::new(1, 0)), // source range
                                Some(DiagnosticSeverity::Error),
                                None,                                              // code
                                None,                                              // source
                                format!("could not parse `{}`: {}", err.1, err.0), // message
                                None,                                              // related info
                            )],
                        ),
                    )?;
                    return Ok(());
                }
            };

        // Discover other documents we should parse and schedule them for parsing.
        let _dependencies = document
            .all()
            .parsed
            .nodes()
            .iter()
            .filter_map(|node: &ast::Node| match &node.data {
                m::Event::Start(m::Tag::Link(_, dest, _)) => {
                    let (document, _anchor) = from_reference(dest.as_ref(), &uri)?;

                    match document.scheme() {
                        "file" => Some((document, node.range)),
                        _ => None,
                    }
                }
                _ => None,
            })
            .map(|(document, source_range)| {
                self.add_task(Task::LoadFile(Box::new((
                    document,
                    (uri.clone(), source_range),
                ))))
                .ok();
            })
            .collect::<Vec<_>>();

        self.documents.insert(
            uri,
            Document {
                document,
                version,
                updating: false,
            },
        );

        // Schedule linter run.
        self.add_task(Task::RunLint)
    }

    fn load_file(&mut self, uri: Url, source: (Url, Range)) -> Result<()> {
        // If the document appeared in the cache it is already tracked by the client.
        if self.documents.contains_key(&uri) {
            return Ok(());
        }

        let document = match std::fs::read_to_string(uri.to_file_path().unwrap()) {
            Ok(text) => text,
            Err(err) => {
                self.notification::<notification::PublishDiagnostics>(
                    // TODO(bbannier): collect all diagnostics globally and push them out at once.
                    PublishDiagnosticsParams::new(
                        source.0,
                        vec![Diagnostic::new(
                            source.1,
                            Some(DiagnosticSeverity::Error),
                            None, // code
                            None, // source
                            format!("could not read file `{}`: {}", uri, err.to_string()), // message
                            None, // related info
                        )],
                    ),
                )?;

                return Ok(());
            }
        };

        // This document does not exist and cannot be `updating`.
        self.add_task(Task::UpdateDocument(uri, document, None))
    }

    fn get_destination(&self, source: &Url, dest: &str) -> Option<Location> {
        from_reference(dest, source).and_then(|(uri, anchor)| {
            // Obtain dest node and create result.
            self.documents.get(&uri).and_then(|document| {
                document
                    .document
                    .all()
                    .parsed
                    .nodes()
                    .iter()
                    .find_map(|node| {
                        if let Some(anchor) = &anchor {
                            if let Some(node_anchor) = &node.anchor {
                                if node_anchor == anchor {
                                    return Some(Location::new(uri.clone(), node.range));
                                }
                            }
                        } else {
                            return Some(Location::new(
                                uri.clone(),
                                Range::new(Position::new(0, 0), Position::new(0, 0)),
                            ));
                        }
                        None
                    })
            })
        })
    }

    fn run_lint(&mut self) -> Result<()> {
        if self.documents.iter().any(|(_, document)| document.updating) {
            debug!("documents are still updating, defering linting");
            return self.add_task(Task::RunLint);
        }

        self.check_references()
            .iter()
            .cloned()
            .map(|(uri, diagnostics)| {
                self.notification::<notification::PublishDiagnostics>(
                    PublishDiagnosticsParams::new(uri, diagnostics),
                )
            })
            .collect::<Result<Vec<()>>>()?;

        Ok(())
    }

    fn check_references(&self) -> Vec<(Url, Vec<Diagnostic>)> {
        info!("checking references");

        self.documents
            .iter()
            .map(|(uri, document)| {
                let diagnostics = document
                    .document
                    .all()
                    .parsed
                    .nodes()
                    .iter()
                    .filter_map(|node| match &node.data {
                        m::Event::Start(m::Tag::Link(_, dest, _)) => {
                            // Ignore non-file links for now.
                            use std::str::FromStr;
                            if let Ok(dest) = Url::from_str(dest.as_ref()) {
                                if dest.scheme() != "file" {
                                    return None;
                                }
                            }

                            match self.get_destination(uri, dest) {
                                // FIXME(bbannier): cache these and only send the same diagnostic once.
                                None => Some(Diagnostic::new(
                                    node.range, // source range
                                    Some(DiagnosticSeverity::Error),
                                    None, // code
                                    None, // source
                                    format!("reference '{}' not found", dest),
                                    None, // related info
                                )),
                                Some(_) => None,
                            }
                        }
                        _ => None,
                    })
                    .inspect(|_| {
                        info!("found invalid reference in `{}`", &uri);
                    })
                    .collect::<Vec<_>>();

                (uri.clone(), diagnostics)
            })
            .collect()
    }

    fn get_symbols(&self, uri: &Url) -> Option<Vec<SymbolInformation>> {
        self.documents.get(uri).map(|document| {
            document
                .document
                .all()
                .parsed
                .nodes()
                .iter()
                .filter_map(|node: &ast::Node| match &node.anchor {
                    Some(_) => Some(SymbolInformation {
                        name: document.document.all().text[node.offsets.start..node.offsets.end]
                            .into(),
                        location: Location::new(uri.clone(), node.range),
                        kind: SymbolKind::String,
                        deprecated: None,
                        container_name: None,
                    }),
                    None => None,
                })
                .collect()
        })
    }
}

fn pretty_link(link_type: m::LinkType, dest: &m::CowStr, title: &m::CowStr) -> String {
    let link_type = match link_type {
        m::LinkType::Inline => "inline",
        m::LinkType::Reference => "reference",
        m::LinkType::Collapsed => "collapsed",
        m::LinkType::Shortcut => "shortcut",
        m::LinkType::Autolink => "autolink",
        m::LinkType::Email => "email address",
        m::LinkType::CollapsedUnknown => "collapsed link without destination in document",
        m::LinkType::ReferenceUnknown => "reference link without destination in document",
        m::LinkType::ShortcutUnknown => "shortcut link without destination in document",
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
        m::Event::Code(_) => "Inline code".to_string(),
        m::Event::Start(tag) | m::Event::End(tag) => match tag {
            m::Tag::Paragraph => "Paragraph".to_string(),
            m::Tag::Heading(level) => format!("Heading (level: {})", level),
            m::Tag::BlockQuote => "Blockquote".to_string(),
            m::Tag::CodeBlock(_) => "Code block".to_string(),
            m::Tag::Emphasis => "Emphasis".to_string(),
            m::Tag::FootnoteDefinition(_) => "Footnote definition".to_string(),
            m::Tag::Image(link_type, dest, title) => {
                format!("Image ({})", pretty_link(*link_type, dest, title))
            }
            m::Tag::Link(link_type, dest, title) => {
                format!("Link ({})", pretty_link(*link_type, dest, title))
            }
            m::Tag::Item => "Item".to_string(),
            m::Tag::List(option) => match option {
                Some(option) => format!("List (first item: {})", option),
                None => "List".to_string(),
            },
            m::Tag::Strikethrough => "Strikethrough".to_string(),
            m::Tag::Strong => "Strong".to_string(),
            m::Tag::Table(alignment) => format!(
                "Table (alignment: {})",
                alignment
                    .iter()
                    .map(|align| match align {
                        m::Alignment::None => "none",
                        m::Alignment::Center => "center",
                        m::Alignment::Left => "left",
                        m::Alignment::Right => "right",
                    }
                    .to_string())
                    .collect::<Vec<String>>()
                    .join(" | ")
            ),
            m::Tag::TableCell => "Table cell".to_string(),
            m::Tag::TableRow => "Table row".to_string(),
            m::Tag::TableHead => "Table head".to_string(),
        },
        m::Event::FootnoteReference(_) => "Footnote reference".to_string(),
        m::Event::SoftBreak => "Soft break".to_string(),
        m::Event::HardBreak => "Hard break".to_string(),
        m::Event::Html(_) => "Html".to_string(),
        m::Event::Rule => "Rule".to_string(),
        m::Event::TaskListMarker(_) => "Task list marker".to_string(),
        m::Event::Text(_) => "Text".to_string(),
    };

    match &node.anchor {
        None => event,
        Some(anchor) => format!("{}\nanchor: {}", event, anchor),
    }
}

fn full_reference(target: (&str, &Url), base: &Url, source: &Url) -> Option<String> {
    Some(if target.1 == source {
        format!("#{}", target.0)
    } else {
        format!("{}#{}", make_relative(target.1, base)?, target.0)
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

fn from_reference<'a>(reference: &'a str, from: &Url) -> Option<(Url, Option<&'a str>)> {
    let base = {
        let mut path = from.to_file_path().ok()?;
        if !path.pop() {
            return None;
        }

        Url::from_directory_path(path).ok()?
    };

    let (reference, anchor) = {
        let split: VecDeque<&str> = reference.rsplitn(2, '#').collect();
        let (reference, anchor) = match split.len() {
            1 => (split[0], None),
            2 => (split[1], Some(split[0])),
            _ => return None,
        };

        if !reference.is_empty() {
            (reference, anchor)
        } else {
            (from.as_str(), anchor)
        }
    };

    let reference = base.join(reference).ok()?;

    Some((reference, anchor))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::RecvError,
        lsp_server::Connection,
        serde::Deserialize,
        std::{cell::Cell, thread::sleep, time},
        textwrap::dedent,
    };

    struct TestServer {
        _thread: jod_thread::JoinHandle<()>,
        client: Connection,
        req_id: Cell<u64>,
        notifications: (Sender<Notification>, Receiver<Notification>),
    }

    impl TestServer {
        fn new() -> TestServer {
            // Set up logging. This might fail if another test thread already set up logging.
            let _ = flexi_logger::Logger::with_env().start();

            let (connection, client) = Connection::memory();
            let _thread = jod_thread::Builder::new()
                .name("test server".to_string())
                .spawn(|| {
                    run_server(connection).unwrap();
                })
                .unwrap();

            let req_id = Cell::new(0);

            let notifications = crossbeam_channel::unbounded();

            let server = TestServer {
                _thread,
                client,
                req_id,
                notifications,
            };

            server
                .send_request::<request::Initialize>(InitializeParams {
                    capabilities: ClientCapabilities::default(),
                    initialization_options: None,
                    process_id: None,
                    root_path: None,
                    root_uri: None,
                    trace: None,
                    workspace_folders: None,
                })
                .unwrap();

            server.send_notification::<notification::Initialized>(InitializedParams {});

            server
        }

        fn send_request<R>(&self, params: R::Params) -> Result<R::Result>
        where
            R: request::Request,
            R::Params: Serialize,
            for<'de> <R as request::Request>::Result: Deserialize<'de>,
        {
            let id = self.req_id.get();
            self.req_id.set(id + 1);

            self.client
                .sender
                .send(lsp_server::Message::from(lsp_server::Request::new(
                    id.into(),
                    R::METHOD.into(),
                    params,
                )))?;

            loop {
                let response = match self
                    .client
                    .receiver
                    .recv_timeout(time::Duration::from_millis(10))
                {
                    Ok(response) => response,
                    Err(err) => return Err(err.into()),
                };

                let response = match response {
                    lsp_server::Message::Response(response) => response,
                    lsp_server::Message::Notification(not) => {
                        self.notifications.0.send(not).unwrap();
                        continue;
                    }
                    otherwise => {
                        info!("Dropping message '{:?}'", otherwise);
                        continue;
                    }
                }
                .result
                .unwrap();

                return Ok(serde_json::from_value(response).unwrap());
            }
        }

        fn send_notification<N>(&self, params: N::Params)
        where
            N: notification::Notification,
            N::Params: Serialize,
        {
            let not = lsp_server::Notification::new(N::METHOD.into(), params);
            self.client
                .sender
                .send(lsp_server::Message::Notification(not))
                .unwrap();

            // Loop until the server has processed the notification.
            loop {
                debug!("Getting server status");

                match self.send_request::<StatusRequest>(()) {
                    Ok(status) => {
                        debug!("Server status is {:?}", status);
                        if status.is_idle {
                            break;
                        }
                    }
                    // We might receive an `RecvError` if no message is available, yet, in which
                    // case we continue. For other errors like e.g., `SendError` we should break;
                    Err(err) => match err.is::<RecvError>() {
                        true => continue,
                        _ => break,
                    },
                };

                sleep(time::Duration::from_millis(10));
            }
        }

        fn notification<N>(&self) -> Result<N::Params>
        where
            N: notification::Notification,
            N::Params: serde::de::DeserializeOwned,
        {
            let not: Notification = self.notifications.1.recv().map_err(|err| err)?;
            serde_json::from_value(not.params).map_err(|err| err.into())
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.send_request::<request::Shutdown>(()).unwrap();
            self.send_notification::<notification::Exit>(());
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
            Some("bar.md#baz".into())
        );
    }

    #[test]
    fn test_from_reference() {
        let root = Url::from_file_path("/").unwrap();
        let base = root.join("foo").unwrap();

        assert_eq!(
            from_reference("#baz", &base.join("foo.md").unwrap()),
            Some((base.join("foo.md").unwrap(), Some("baz")))
        );

        assert_eq!(
            from_reference("bar.md", &base.join("foo.md").unwrap()),
            Some((base.join("bar.md").unwrap(), None))
        );

        assert_eq!(
            from_reference("bar.md#baz", &base.join("foo.md").unwrap()),
            Some((base.join("bar.md").unwrap(), Some("baz")))
        );

        assert_eq!(
            from_reference("../bar.md#baz", &base.join("foo.md").unwrap()),
            Some((root.join("bar.md").unwrap(), Some("baz")))
        );
    }

    // This test checks hover handling, and as side effects also the loading of docs via
    // `DidOpenTextDocument` and modification via `DidChangeTextDocument` notifications.
    #[test]
    fn hover() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo/bar.md").unwrap();

        // Prime the server with a document with a heading.
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "markdown".into(),
                1,
                "# heading".into(),
            ),
        });

        assert_eq!(
            server
                .send_request::<request::HoverRequest>(TextDocumentPositionParams::new(
                    TextDocumentIdentifier { uri: uri.clone() },
                    Position::new(0, 0)
                ))
                .unwrap(),
            Some(Hover {
                contents: HoverContents::Array(vec![MarkedString::from_markdown(
                    "Heading (level: 1)\nanchor: heading".to_string()
                )]),
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 9))),
            }),
            "The first character should match a heading"
        );

        assert_eq!(
            server
                .send_request::<request::HoverRequest>(TextDocumentPositionParams::new(
                    TextDocumentIdentifier { uri: uri.clone() },
                    Position::new(0, 2)
                ))
                .unwrap(),
            Some(Hover {
                contents: HoverContents::Array(vec![MarkedString::from_markdown(
                    "Text".to_string()
                )]),
                range: Some(Range::new(Position::new(0, 2), Position::new(0, 9))),
            }),
            "The third character should match text"
        );

        // Change the document to contain inline code.
        server.send_notification::<notification::DidChangeTextDocument>(
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(uri.clone(), 2),
                content_changes: vec![TextDocumentContentChangeEvent {
                    text: "`int main()`".into(),
                    range: None,
                    range_length: None,
                }],
            },
        );

        assert_eq!(
            server
                .send_request::<request::HoverRequest>(TextDocumentPositionParams::new(
                    TextDocumentIdentifier { uri: uri.clone() },
                    Position::new(0, 3)
                ))
                .unwrap(),
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
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
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
            server
                .send_request::<request::Completion>(CompletionParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(2, 12),
                    ),
                    context: None,
                })
                .unwrap(),
            Some(CompletionResponse::from(vec![CompletionItem::new_simple(
                "#heading".into(),
                "# heading\n".into()
            )])),
            "Completion at reference should complete heading"
        );

        assert_eq!(
            server
                .send_request::<request::Completion>(CompletionParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(2, 2),
                    ),
                    context: None,
                })
                .unwrap(),
            Some(CompletionResponse::from(vec![])),
            "Completion in the middle of reference should not complete anything",
        );

        assert_eq!(
            server
                .send_request::<request::Completion>(CompletionParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(1, 0),
                    ),
                    context: None,
                })
                .unwrap(),
            Some(CompletionResponse::from(vec![])),
            "Completion at heading should not complete anything"
        );
    }

    #[test]
    fn references() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
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

                    # h3
                    [ref1](foo.md#h3)
                    ",
                ),
            ),
        });

        assert_eq!(
            server
                .send_request::<request::References>(ReferenceParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.clone()),
                        position: Position::new(0, 0),
                    },
                    context: ReferenceContext {
                        include_declaration: false,
                    },
                })
                .unwrap(),
            None
        );

        assert_eq!(
            server
                .send_request::<request::References>(ReferenceParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.clone()),
                        position: Position::new(1, 0),
                    },
                    context: ReferenceContext {
                        include_declaration: true,
                    },
                })
                .unwrap(),
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
            server
                .send_request::<request::References>(ReferenceParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.clone()),
                        position: Position::new(1, 0),
                    },
                    context: ReferenceContext {
                        include_declaration: false,
                    },
                })
                .unwrap(),
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
            server
                .send_request::<request::References>(ReferenceParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.clone()),
                        position: Position::new(2, 7),
                    },
                    context: ReferenceContext {
                        include_declaration: true,
                    },
                })
                .unwrap(),
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

        assert_eq!(
            server
                .send_request::<request::References>(ReferenceParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.clone()),
                        position: Position::new(9, 0),
                    },
                    context: ReferenceContext {
                        include_declaration: true,
                    },
                })
                .unwrap(),
            Some(vec![Location::new(
                uri.clone(),
                Range::new(Position::new(9, 0), Position::new(9, 17))
            ),])
        );
    }

    #[test]
    fn gotodefinition() {
        let server = TestServer::new();

        let file1 = Url::from_file_path("/file1.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                file1.clone(),
                "markdown".into(),
                1,
                String::from("# bar"),
            ),
        });

        let file2 = Url::from_file_path("/file2.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                file2.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    # h1
                    [ref1](#h1)
                    [bar](file1.md)
                    [bar_bar](file1.md#bar)
                    ",
                ),
            ),
        });

        assert_eq!(
            server
                .send_request::<request::GotoDefinition>(TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(file2.clone()),
                    Position::new(2, 0),
                ))
                .unwrap(),
            Some(request::GotoDefinitionResponse::Scalar(Location::new(
                file2.clone(),
                Range::new(Position::new(1, 0), Position::new(2, 0))
            )))
        );

        assert_eq!(
            server
                .send_request::<request::GotoDefinition>(TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(file2.clone()),
                    Position::new(3, 0),
                ))
                .unwrap(),
            Some(request::GotoDefinitionResponse::Scalar(Location::new(
                file1.clone(),
                Range::new(Position::new(0, 0), Position::new(0, 0))
            )))
        );

        assert_eq!(
            server
                .send_request::<request::GotoDefinition>(TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(file2.clone()),
                    Position::new(4, 0),
                ))
                .unwrap(),
            Some(request::GotoDefinitionResponse::Scalar(Location::new(
                file1.clone(),
                Range::new(Position::new(0, 0), Position::new(0, 5))
            )))
        );
    }

    #[test]
    fn folding_range_request() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
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
            server
                .send_request::<request::FoldingRangeRequest>(FoldingRangeParams {
                    text_document: TextDocumentIdentifier::new(uri)
                })
                .unwrap(),
            Some(vec![
                FoldingRange {
                    start_line: 1,
                    start_character: None,
                    end_line: 13,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region)
                },
                FoldingRange {
                    start_line: 6,
                    start_character: None,
                    end_line: 13,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region)
                }
            ]),
        );
    }

    #[test]
    fn test_document_symbol_request() {
        let server = TestServer::new();

        let uri = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    # h1
                    ## h2
                    ",
                ),
            ),
        });

        assert_eq!(
            server
                .send_request::<request::DocumentSymbolRequest>(DocumentSymbolParams {
                    text_document: TextDocumentIdentifier::new(uri.clone()),
                })
                .unwrap(),
            Some(DocumentSymbolResponse::from(vec![
                SymbolInformation {
                    name: "# h1\n".into(),
                    location: Location::new(
                        uri.clone(),
                        Range::new(Position::new(1, 0), Position::new(2, 0))
                    ),
                    kind: SymbolKind::String,
                    deprecated: None,
                    container_name: None,
                },
                SymbolInformation {
                    name: "## h2\n".into(),
                    location: Location::new(
                        uri.clone(),
                        Range::new(Position::new(2, 0), Position::new(3, 0))
                    ),
                    kind: SymbolKind::String,
                    deprecated: None,
                    container_name: None,
                },
            ])),
        );
    }

    #[test]
    fn test_workspace_symbol_request() {
        let server = TestServer::new();

        let file1 = Url::from_file_path("/bar.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                file1.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    # bar
                    ",
                ),
            ),
        });

        let file2 = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                file2.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    # foo
                    ",
                ),
            ),
        });

        // An empty query returns all symbols.
        assert_eq!(
            server
                .send_request::<request::WorkspaceSymbol>(WorkspaceSymbolParams {
                    query: "".into(),
                })
                .unwrap()
                .map(|symbols| {
                    let mut symbols = symbols;
                    symbols.sort_unstable_by(|left, right| left.name.cmp(&right.name));
                    symbols
                }),
            Some(vec![
                SymbolInformation {
                    name: "# bar\n".into(),
                    location: Location::new(
                        file1.clone(),
                        Range::new(Position::new(1, 0), Position::new(2, 0))
                    ),
                    kind: SymbolKind::String,
                    deprecated: None,
                    container_name: None,
                },
                SymbolInformation {
                    name: "# foo\n".into(),
                    location: Location::new(
                        file2.clone(),
                        Range::new(Position::new(1, 0), Position::new(2, 0))
                    ),
                    kind: SymbolKind::String,
                    deprecated: None,
                    container_name: None,
                },
            ]),
        );

        // With query matching symbols are returned.
        assert_eq!(
            server
                .send_request::<request::WorkspaceSymbol>(WorkspaceSymbolParams {
                    query: "foo".into(),
                })
                .unwrap(),
            Some(vec![SymbolInformation {
                name: "# foo\n".into(),
                location: Location::new(
                    file2.clone(),
                    Range::new(Position::new(1, 0), Position::new(2, 0))
                ),
                kind: SymbolKind::String,
                deprecated: None,
                container_name: None,
            },]),
        );
    }

    #[test]
    fn test_load_file_error() {
        let server = TestServer::new();

        let file2 = Url::from_file_path("/foo.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                file2.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                    [bar](bar.md)
                    ",
                ),
            ),
        });

        assert_eq!(
            server
                .notification::<notification::PublishDiagnostics>()
                .unwrap(),
            PublishDiagnosticsParams::new(
                file2,
                vec![Diagnostic::new(
                    Range::new(Position::new(1, 0), Position::new(1, 13)),
                    Some(DiagnosticSeverity::Error),
                    None,
                    None,
                    "could not read file `file:///bar.md`: No such file or directory (os error 2)"
                        .into(),
                    None,
                )]
            )
        );
    }
}
