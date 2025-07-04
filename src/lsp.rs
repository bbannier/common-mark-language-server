use {
    crate::ast,
    anyhow::{Result, anyhow},
    crossbeam_channel::{Receiver, RecvError, Sender, select},
    log::{debug, info},
    lsp_server::{Connection, Message, Notification, Request, RequestId, Response},
    lsp_types::{
        CompletionItem, CompletionOptions, CompletionParams, Diagnostic, DiagnosticSeverity,
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DocumentSymbolParams, DocumentSymbolResponse, FoldingRange, FoldingRangeKind,
        FoldingRangeParams, FoldingRangeProviderCapability, GotoDefinitionParams,
        GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
        InitializeParams, Location, MarkedString, OneOf, Position, PublishDiagnosticsParams, Range,
        ReferenceParams, RenameParams, ServerCapabilities, SymbolInformation, SymbolKind,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, WorkspaceEdit,
        WorkspaceSymbolParams, notification, request,
    },
    ouroboros::self_referencing,
    pulldown_cmark::{self as m},
    salsa::Setter,
    serde::{Deserialize, Serialize},
    static_assertions::assert_eq_size,
    std::{
        collections::{HashMap, VecDeque},
        fmt,
        path::Path,
    },
    url::Url,
};

fn request_cast<R>(
    req: lsp_server::Request,
) -> std::result::Result<(RequestId, R::Params), lsp_server::Request>
where
    R: request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD).map_err(|e| match e {
        lsp_server::ExtractError::MethodMismatch(r) => r,
        lsp_server::ExtractError::JsonError { method, error } => {
            panic!("malformed request {method}: {error}")
        }
    })
}

fn notification_cast<N>(
    not: lsp_server::Notification,
) -> std::result::Result<N::Params, lsp_server::Notification>
where
    N: notification::Notification,
    N::Params: serde::de::DeserializeOwned,
{
    not.extract(N::METHOD).map_err(|e| match e {
        lsp_server::ExtractError::MethodMismatch(r) => r,
        lsp_server::ExtractError::JsonError { method, error } => {
            panic!("malformed notification {method}: {error}")
        }
    })
}

#[self_referencing]
pub struct Document {
    text: String,
    #[borrows(text)]
    #[covariant]
    parsed: ast::ParsedDocument<'this>,
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.borrow_parsed();
        f.debug_struct("Document")
            .field("document", &self.borrow_parsed())
            .finish()
    }
}

impl PartialEq for Document {
    fn eq(&self, other: &Self) -> bool {
        self.borrow_parsed() == other.borrow_parsed()
    }
}

impl Eq for Document {}

#[derive(Debug)]
enum Task {
    LoadFile(Box<(Url, (Url, Range))>),
    UpdateDocument(Url, String, Option<i32>),
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

type Documents = HashMap<Url, Document>;

fn get_symbols(documents: &Documents, uri: &Url) -> Option<Vec<SymbolInformation>> {
    documents.get(uri).map(|document| {
        document
            .borrow_parsed()
            .nodes()
            .iter()
            .filter_map(|node: &ast::Node| {
                node.anchor.as_ref().map(|_| {
                    #[allow(deprecated)]
                    SymbolInformation {
                        name: document.borrow_text()[node.offsets.clone()].into(),
                        location: Location::new(uri.clone(), node.range),
                        kind: SymbolKind::STRING,
                        deprecated: None,
                        container_name: None,
                        tags: None,
                    }
                })
            })
            .collect()
    })
}

fn get_link_at<'a>(documents: &'a Documents, uri: &Url, position: Position) -> Option<&'a str> {
    documents.get(uri).and_then(|document| {
        // Extract any link at the current position.
        document
            .borrow_parsed()
            .at(position)
            .iter()
            .find_map(|node| match &node.data {
                m::Event::Start(m::Tag::Link { dest_url, .. }) => Some(dest_url.as_ref()),
                _ => None,
            })
    })
}

fn get_destination(documents: &Documents, source: &Url, dest: &str) -> Option<Location> {
    from_reference(dest, source).and_then(|(uri, anchor)| {
        // Obtain dest node and create result.
        documents.get(&uri).and_then(|document| {
            document.borrow_parsed().nodes().iter().find_map(|node| {
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

struct Server {
    connection: Connection,
    tasks: Tasks,
    documents: Documents,
    root_uri: Url,
    open_document: Option<Url>,

    db: DbImpl,
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
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec!["](".into()]),
            ..CompletionOptions::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    }
}
pub fn run_server(connection: Connection) -> Result<()> {
    let server_capabilities = server_capabilities();
    let initialize_params = connection.initialize(serde_json::to_value(server_capabilities)?)?;
    let initialize_params: InitializeParams = serde_json::from_value(initialize_params)?;

    let cwd = Url::from_file_path(std::env::current_dir()?).ok();
    let root_uri = initialize_params
        .workspace_folders
        .and_then(|folders| folders.first().map(|f| f.uri.clone()))
        .unwrap_or_else(|| cwd.expect("could not determine root_uri"));

    let tasks = Tasks::new();

    let db = DbImpl::default();

    let server = Server {
        connection,
        tasks,
        documents: Documents::new(),
        root_uri,
        open_document: None,
        db,
    };

    main_loop(server)
}

fn main_loop(server: Server) -> Result<()> {
    info!("starting main loop");

    let mut server = server;

    loop {
        let event = select! {
            recv(server.connection.receiver) -> msg => match msg {
                Ok(msg) => Event::Api(msg),
                Err(RecvError) => return Err(anyhow!("client exited without shutdown")),
            },
            recv(server.tasks.receiver) -> task => match task {
                Ok(task) => Event::Task(task),
                Err(RecvError) => continue,
            }
        };

        debug!("processing event: {event:?}");

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
                Task::LoadFile(uri_source) => server.load_file(uri_source.0, &uri_source.1)?,
                Task::UpdateDocument(uri, document, version) => {
                    server.update_document(uri, document, version);
                }
            },
        }
    }

    info!("finished main loop");

    Ok(())
}

fn on_request(req: Request, server: &mut Server) -> Result<()> {
    match handle_request(req, server) {
        None => Ok(()),
        Some(response) => server.respond(response),
    }
}

fn on_notification(not: Notification, server: &mut Server) -> Result<()> {
    let not = match notification_cast::<notification::DidOpenTextDocument>(not) {
        Ok(params) => {
            return server.handle_did_open_text_document(params);
        }
        Err(not) => not,
    };
    let not = match notification_cast::<notification::DidCloseTextDocument>(not) {
        Ok(params) => {
            server.handle_did_close_text_document(&params);
            return Ok(());
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
    fn respond(&self, response: Response) -> Result<()> {
        debug!("sending response: {response:?}");

        self.connection
            .sender
            .send(Message::Response(response))
            .map_err(Into::into)
    }

    fn notify<N>(&self, params: N::Params) -> Result<()>
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
            .map_err(Into::into)
    }

    fn add_task(&self, task: Task) -> Result<()> {
        debug!("adding task: {task:?}");

        self.tasks.sender.send(task)?;
        Ok(())
    }

    fn handle_status_request(&self, id: lsp_server::RequestId) -> Response {
        // This function does not accept parameters since `StatusRequest` is empty.
        assert_eq_size!(StatusRequest, ());
        Response::new_ok(
            id,
            StatusResponse {
                is_idle: self.tasks.receiver.is_empty(),
            },
        )
    }

    fn handle_hover(&self, id: lsp_server::RequestId, params: HoverParams) -> Response {
        let params = params.text_document_position_params;
        let uri = params.text_document.uri;
        let Some(document) = self.documents.get(&uri) else {
            info!("did not find file '{}' in database", &uri);
            return Response::new_ok(id, Option::<HoverContents>::None);
        };

        // We select the node with the shortest range overlapping the range.
        let nodes = document.borrow_parsed().at(params.position);
        let node = nodes
            .iter()
            .min_by(|x, y| x.offsets.len().cmp(&y.offsets.len()));

        let result = node.map(|node| Hover {
            // TODO(bbannier): Maybe just introduce a `Into<MarkedString>` for the data.
            contents: HoverContents::Array(vec![MarkedString::String(pretty(node))]),
            range: Some(node.range),
        });

        Response::new_ok(id, result)
    }

    fn handle_completion(&self, id: lsp_server::RequestId, params: &CompletionParams) -> Response {
        // Do a simple check whether we are actually completing a link. We only check whether the
        // character before the completion position is a literal `](`.
        let good_position = match self
            .documents
            .get(&params.text_document_position.text_document.uri)
        {
            None => false, // Document unknown.
            Some(document) => {
                let position = &params.text_document_position.position;

                let character = usize::try_from(position.character)
                    .expect("could not cast u64 column number to usize");

                character >= 2
                    && document
                        .borrow_text()
                        .lines()
                        .nth(
                            usize::try_from(position.line)
                                .expect("could not cast u64 line number to usize"),
                        )
                        .unwrap()[character - 2..character]
                        == *"]("
            }
        };
        if !good_position {
            return Response::new_ok(id, Vec::<CompletionItem>::new());
        }

        // For now just complete anchors.
        let items = self
            .documents
            .iter()
            .filter_map(|(uri, document)| {
                let anchors = document
                    .borrow_parsed()
                    .nodes()
                    .iter()
                    .filter_map(|node| match &node.anchor {
                        Some(anchor) => {
                            let detail = &document.borrow_text()[node.offsets.clone()];
                            let reference = full_reference(
                                (anchor, uri),
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

        Response::new_ok(id, items)
    }

    fn handle_references(&self, id: lsp_server::RequestId, params: ReferenceParams) -> Response {
        let text_document_position = params.text_document_position;

        let nodes: Vec<_> = self
            .documents
            .get(&text_document_position.text_document.uri)
            .iter()
            .flat_map(|document| document.borrow_parsed().at(text_document_position.position))
            .collect();

        let Some((anchor, anchor_range)) = nodes
            .iter()
            .filter(|node| matches!(&node.data, m::Event::Start(m::Tag::Link { .. })))
            .min_by_key(|node| node.offsets.len())
            .map(|node| match &node.data {
                m::Event::Start(m::Tag::Link { dest_url, .. }) => (
                    String::from(dest_url.as_ref())
                        .trim_start_matches('#')
                        .into(),
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
            })
        else {
            // No anchor found at position, return empty result.
            return Response::new_ok(id, Option::<Vec<Location>>::None);
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
                let request_uri = text_document_position.text_document.uri.clone();
                let anchor = anchor.clone();
                document
                    .borrow_parsed()
                    .nodes()
                    .iter()
                    .filter_map(move |node| match &node.data {
                        m::Event::Start(m::Tag::Link { dest_url, .. })
                            if dest_url.as_ref()
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

        Response::new_ok(id, result)
    }

    fn handle_gotodefinition(
        &self,
        id: lsp_server::RequestId,
        params: GotoDefinitionParams,
    ) -> Response {
        let params = params.text_document_position_params;
        let result: Option<GotoDefinitionResponse> =
            get_link_at(&self.documents, &params.text_document.uri, params.position).and_then(
                |dest| {
                    get_destination(&self.documents, &params.text_document.uri, dest)
                        .map(Into::into)
                },
            );

        Response::new_ok(id, result)
    }

    fn handle_folding_range_request(
        &self,
        id: lsp_server::RequestId,
        params: &FoldingRangeParams,
    ) -> Response {
        let result: Option<Vec<_>> =
            self.documents
                .get(&params.text_document.uri)
                .map(|document| {
                    let nodes = document.borrow_parsed().nodes();

                    let last_node = nodes.iter().max_by_key(|node| node.offsets.end);

                    let headings = {
                        let mut xs = nodes
                            .iter()
                            .filter_map(|node| match &node.data {
                                m::Event::Start(m::Tag::Heading { level, .. }) => {
                                    Some((level, node))
                                }
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
                            m::Event::Start(m::Tag::Heading { level, .. }) => {
                                // Translate headings into sections.

                                // We can reuse a heading's start tag, but need to generate a corresponding end tag.
                                let end =
                                    headings
                                        .iter()
                                        .skip_while(|(_, n)| n.range != node.range)
                                        .skip(1).find_map(|(l, n)| if *l <= level {
                                            // We let the range end before the next section. This
                                            // is safe as we need to have lines preceeding the
                                            // _next_ section.
                                            Some(n.range.start.line - 1)
                                        } else {None})
                            .unwrap_or(
                                last_node
                                    .expect(
                                        "if we iterate anything at all there should be a last node",
                                    )
                                    .range
                                    .end
                                    .line,
                            );

                                Some(FoldingRange {
                                    start_line: node.range.start.line,
                                    start_character: None,
                                    end_line: end,
                                    end_character: None,
                                    kind: Some(FoldingRangeKind::Region),
                                    ..FoldingRange::default()
                                })
                            }
                            _ => None,
                        })
                        .collect()
                });

        Response::new_ok(id, result)
    }

    fn handle_document_symbol_request(
        &self,
        id: RequestId,
        params: &DocumentSymbolParams,
    ) -> Response {
        Response::new_ok(
            id,
            get_symbols(&self.documents, &params.text_document.uri)
                .map(DocumentSymbolResponse::from),
        )
    }

    fn handle_workspace_symbol(&self, id: RequestId, params: &WorkspaceSymbolParams) -> Response {
        let result: Vec<_> = self
            .documents
            .keys()
            .flat_map(|uri| match get_symbols(&self.documents, uri) {
                Some(symbols) => symbols
                    .iter()
                    .filter(|symbol: &&SymbolInformation| symbol.name.contains(&params.query))
                    .cloned()
                    .collect::<Vec<_>>(),
                None => vec![],
            })
            .collect();

        Response::new_ok(id, result)
    }

    fn handle_rename(&self, id: RequestId, params: &RenameParams) -> Response {
        let source_uri = params.text_document_position.text_document.uri.clone();
        let document = if let Some(document) = self.documents.get(&source_uri) {
            document.borrow_parsed()
        } else {
            info!("did not find file '{}' in database", &source_uri);
            return Response::new_ok(id, Option::<WorkspaceEdit>::None);
        };

        // We only support renaming headings so select `Heading` node at position.
        let nodes = document.at(params.text_document_position.position);

        // Check that we have both a `Heading` and some `Text` at the position.
        if !(nodes
            .iter()
            .any(|node| matches!(&node.data, m::Event::Start(m::Tag::Heading { .. })))
            && nodes
                .iter()
                .any(|node| matches!(&node.data, m::Event::Text(_))))
        {
            return Response::new_ok(id, Option::<WorkspaceEdit>::None);
        }

        let header_text_node = nodes
            .iter()
            .find(|node| matches!(&node.data, m::Event::Text(_)))
            .expect("selection should contain a 'Text' node at this point");

        let header_anchor = nodes
            .iter()
            .find_map(|node| match &node.data {
                m::Event::Start(m::Tag::Heading { .. }) => Some(
                    node.anchor
                        .as_ref()
                        .expect("headings should always contain a generated anchor")
                        .clone(),
                ),
                _ => None,
            })
            .expect("selection should contain a 'Heading' node at this point");

        // Find all references to the heading.
        let source_uri_ = source_uri.clone();
        let references = self
            .documents
            .iter()
            .flat_map(|(referencing_uri, document)| {
                let source_uri = source_uri_.clone();
                let header_anchor = header_anchor.clone();

                document
                    .borrow_parsed()
                    .nodes()
                    .iter()
                    .filter_map(move |node| match &node.data {
                        m::Event::Start(m::Tag::Link { dest_url, .. }) => {
                            match from_reference(dest_url, referencing_uri) {
                                Some((target_uri, target_anchor)) => {
                                    if target_uri == source_uri
                                        && target_anchor == Some(&header_anchor)
                                    {
                                        Some((referencing_uri.clone(), node))
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    })
            });

        // Compute anchor edits.
        let new_name = &params.new_name;
        let new_anchor = ast::anchor(new_name);

        let mut edits = HashMap::new();

        // Insert edit for the heading text.
        edits.insert(
            source_uri,
            vec![TextEdit::new(header_text_node.range, new_name.clone())],
        );

        for (url, node) in references {
            if !edits.contains_key(&url) {
                edits.insert(url.clone(), vec![]);
            }

            // Compute the original range of the reference.
            //
            // TODO(bbannier): In general the destination part of a link can contain spurious
            // whitespace which we currently do not handle. Add some normalization here.
            let end = Position::new(
                node.range.end.line,
                node.range.end.character - 1, /* ] */
            );

            let anchor_position: u32 = header_anchor
                .len()
                .try_into()
                .expect("anchor position not representable");

            let start = Position::new(end.line, end.character - anchor_position);

            edits
                .get_mut(&url)
                .expect("default value should exist in map")
                .push(TextEdit::new(Range::new(start, end), new_anchor.clone()));
        }

        Response::new_ok(id, WorkspaceEdit::new(edits))
    }

    fn handle_did_open_text_document(&mut self, params: DidOpenTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        self.open_document = Some(uri.clone());

        self.add_task(Task::UpdateDocument(uri, text, Some(version)))
    }

    fn handle_did_close_text_document(&mut self, _params: &DidCloseTextDocumentParams) {
        self.open_document = None;
    }

    fn handle_did_change_text_document(
        &mut self,
        mut params: DidChangeTextDocumentParams,
    ) -> Result<()> {
        let uri = params.text_document.uri;
        let text = match params.content_changes.pop() {
            Some(t) => t.text,
            None => return Ok(()),
        };

        let version = params.text_document.version;

        self.add_task(Task::UpdateDocument(uri, text, Some(version)))
    }

    fn update_document(&mut self, uri: Url, text: String, _version: Option<i32>) {
        if let Some(document) = self.documents.get_mut(&uri) {
            if document.borrow_text() == &text {
                debug!(
                    "not update {} as the new version is identical to the stored one",
                    &uri
                );
                return;
            }
        }

        info!("updating {}", &uri);

        if let Some(file) = self.db.file(&uri) {
            file.set_text(&mut self.db).to(text);
        } else {
            self.db
                .files
                .insert(uri.clone(), SourceFile::new(&self.db, text));
        }

        let document = parsed(&self.db, &uri).unwrap();

        // Discover other documents we should parse and schedule them for parsing.
        let _dependencies = document
            .borrow_parsed()
            .nodes()
            .iter()
            .filter_map(|node: &ast::Node| match &node.data {
                m::Event::Start(m::Tag::Link { dest_url, .. }) => {
                    let (document, _anchor) = from_reference(dest_url.as_ref(), &uri)?;

                    if document == uri {
                        return None;
                    }

                    match document.scheme() {
                        "file" => {
                            // foo.
                            self.add_task(Task::LoadFile(Box::new((
                                document,
                                (uri.clone(), node.range),
                            ))))
                            .ok()
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        self.documents.insert(uri, document);
    }

    fn load_file(&mut self, uri: Url, source: &(Url, Range)) -> Result<()> {
        let Ok(document) = std::fs::read_to_string(uri.to_file_path().unwrap()) else {
            return self.notify::<notification::PublishDiagnostics>(PublishDiagnosticsParams::new(
                source.0.clone(),
                vec![Diagnostic::new(
                    source.1,
                    Some(DiagnosticSeverity::ERROR),
                    None,                              // code
                    None,                              // source
                    format!("file '{uri}' not found"), // message
                    None,                              // related info
                    None,                              // tag
                )],
                None, // version
            ));
        };

        // This document does not exist and cannot be `updating`.
        self.add_task(Task::UpdateDocument(uri, document, None))
    }
}

fn handle_request(req: Request, server: &mut Server) -> Option<Response> {
    let req = match request_cast::<request::HoverRequest>(req) {
        Ok((id, params)) => {
            return Some(server.handle_hover(id, params));
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::Completion>(req) {
        Ok((id, params)) => {
            return Some(server.handle_completion(id, &params));
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::References>(req) {
        Ok((id, params)) => {
            return Some(server.handle_references(id, params));
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::GotoDefinition>(req) {
        Ok((id, params)) => {
            return Some(server.handle_gotodefinition(id, params));
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::FoldingRangeRequest>(req) {
        Ok((id, params)) => {
            return Some(server.handle_folding_range_request(id, &params));
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::DocumentSymbolRequest>(req) {
        Ok((id, params)) => {
            return Some(server.handle_document_symbol_request(id, &params));
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::WorkspaceSymbolRequest>(req) {
        Ok((id, params)) => {
            return Some(server.handle_workspace_symbol(id, &params));
        }
        Err(req) => req,
    };
    let req = match request_cast::<request::Rename>(req) {
        Ok((id, params)) => {
            return Some(server.handle_rename(id, &params));
        }
        Err(req) => req,
    };
    let req = match request_cast::<StatusRequest>(req) {
        Ok((id, ())) => {
            return Some(server.handle_status_request(id));
        }
        Err(req) => req,
    };

    info!("Cannot handle request '{req:?}'");

    None
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
        result.push(format!("destination: {dest}"));
    }
    if !title.is_empty() {
        result.push(format!("title: {title}"));
    }

    result.join(", ")
}

fn pretty(node: &ast::Node) -> String {
    let event = &node.data;
    let event = match event {
        m::Event::InlineHtml(_) => "Inline HTML".to_string(),
        m::Event::InlineMath(_) => "Inline math".to_string(),
        m::Event::DisplayMath(_) => "Display math".to_string(),
        m::Event::Code(_) => "Inline code".to_string(),
        m::Event::Start(tag) => match tag {
            m::Tag::MetadataBlock(..) => "Metadata block".to_string(),
            m::Tag::HtmlBlock { .. } => "HTML block".to_string(),
            m::Tag::Paragraph => "Paragraph".to_string(),
            m::Tag::Heading { level, .. } => format!("Heading (level: {level})"),
            m::Tag::BlockQuote(..) => "Blockquote".to_string(),
            m::Tag::CodeBlock(_) => "Code block".to_string(),
            m::Tag::Emphasis => "Emphasis".to_string(),
            m::Tag::FootnoteDefinition(_) => "Footnote definition".to_string(),
            m::Tag::Image {
                link_type,
                dest_url,
                title,
                ..
            } => {
                format!("Image ({})", pretty_link(*link_type, dest_url, title))
            }
            m::Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            } => {
                format!("Link ({})", pretty_link(*link_type, dest_url, title))
            }
            m::Tag::Item => "Item".to_string(),
            m::Tag::List(option) => match option {
                Some(option) => format!("List (first item: {option})"),
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
        m::Event::End(..) => String::new(),
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
        Some(anchor) => format!("{event}\nanchor: {anchor}"),
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

/// Get link target (file[, anchor]) from a reference in some file.
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
        let (reference_, anchor) = match split.len() {
            1 => (split[0], None),
            2 => (split[1], Some(split[0])),
            _ => return None,
        };

        if reference_.is_empty() {
            (from.as_str(), anchor)
        } else {
            (reference_, anchor)
        }
    };

    let reference = base.join(reference).ok()?;

    Some((reference, anchor))
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;
    use lsp_types::WorkspaceSymbolResponse;

    use {
        super::*,
        lsp_types::{
            InitializedParams, PartialResultParams, ReferenceContext,
            TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
            TextDocumentPositionParams, VersionedTextDocumentIdentifier, WorkDoneProgressParams,
        },
        std::{cell::Cell, thread::sleep, time},
        textwrap::dedent,
    };

    struct TestServer {
        #[allow(dead_code)]
        thread: jod_thread::JoinHandle<()>,
        client: Connection,
        req_id: Cell<i32>,
        notifications: (Sender<Notification>, Receiver<Notification>),
    }

    impl TestServer {
        fn new() -> TestServer {
            // Set up logging. This might fail if another test thread already set up logging.
            let _ = flexi_logger::Logger::try_with_env()
                .expect("could not apply logger setting from environment variables")
                .start();

            let (connection, client) = Connection::memory();
            let thread = jod_thread::Builder::new()
                .name("test server".to_string())
                .spawn(|| {
                    run_server(connection).unwrap();
                })
                .unwrap();

            let req_id = Cell::new(0);

            let notifications = crossbeam_channel::unbounded();

            let server = TestServer {
                thread,
                client,
                req_id,
                notifications,
            };

            server
                .send_request::<request::Initialize>(InitializeParams::default())
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
                    lsp_server::Message::Request(request) => {
                        info!("Dropping message '{request:?}'");
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
            if self
                .client
                .sender
                .send(lsp_server::Message::Notification(not))
                .is_err()
            {
                return;
            }

            // Loop until the server has processed the notification.
            loop {
                debug!("Getting server status");

                match self.send_request::<StatusRequest>(()) {
                    Ok(status) => {
                        debug!("Server status is {status:?}");
                        if status.is_idle {
                            break;
                        }
                    }
                    // We might receive an `RecvError` if no message is available, yet, in which
                    // case we continue. For other errors like e.g., `SendError` we should break;
                    Err(err) => {
                        if !err.is::<RecvError>() {
                            break;
                        }
                    }
                }

                sleep(time::Duration::from_millis(10));
            }
        }

        fn notification<N>(&self) -> Result<N::Params>
        where
            N: notification::Notification,
            N::Params: serde::de::DeserializeOwned,
        {
            let not: Notification = self.notifications.1.recv()?;
            serde_json::from_value(not.params).map_err(std::convert::Into::into)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = self.send_request::<request::Shutdown>(());
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
                .send_request::<request::HoverRequest>(HoverParams {
                    text_document_position_params: TextDocumentPositionParams::new(
                        TextDocumentIdentifier { uri: uri.clone() },
                        Position::new(0, 0)
                    ),
                    work_done_progress_params: WorkDoneProgressParams::default()
                })
                .unwrap(),
            Some(Hover {
                contents: HoverContents::Array(vec![MarkedString::from_markdown(
                    "Heading (level: h1)\nanchor: heading".to_string()
                )]),
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 9))),
            }),
            "The first character should match a heading"
        );

        assert_debug_snapshot!(server.send_request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams::new(
                TextDocumentIdentifier { uri: uri.clone() },
                Position::new(0, 2)
            ),
            work_done_progress_params: WorkDoneProgressParams::default()
        }));

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

        assert_debug_snapshot!(server.send_request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams::new(
                TextDocumentIdentifier { uri },
                Position::new(0, 3)
            ),
            work_done_progress_params: WorkDoneProgressParams::default()
        }));
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

        assert_debug_snapshot!(
            server.send_request::<request::Completion>(CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri.clone()),
                    Position::new(2, 12),
                ),
                context: None,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
        );

        assert_debug_snapshot!(
            server.send_request::<request::Completion>(CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri.clone()),
                    Position::new(2, 2),
                ),
                context: None,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
        );

        assert_debug_snapshot!(
            server.send_request::<request::Completion>(CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri),
                    Position::new(1, 0),
                ),
                context: None,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
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

        assert_debug_snapshot!(server.send_request::<request::References>(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(uri.clone()),
                position: Position::new(0, 0),
            },
            context: ReferenceContext {
                include_declaration: false,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }));

        assert_debug_snapshot!(server.send_request::<request::References>(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(uri.clone()),
                position: Position::new(1, 0),
            },
            context: ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default()
        }));

        assert_debug_snapshot!(server.send_request::<request::References>(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(uri.clone()),
                position: Position::new(1, 0),
            },
            context: ReferenceContext {
                include_declaration: false,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default()
        }));

        assert_debug_snapshot!(server.send_request::<request::References>(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(uri.clone()),
                position: Position::new(2, 7),
            },
            context: ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }));

        assert_debug_snapshot!(server.send_request::<request::References>(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(uri.clone()),
                position: Position::new(9, 0),
            },
            context: ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }));
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

        assert_debug_snapshot!(server.send_request::<request::GotoDefinition>(
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(file2.clone()),
                    Position::new(2, 0),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default()
            }
        ));

        assert_debug_snapshot!(server.send_request::<request::GotoDefinition>(
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(file2.clone()),
                    Position::new(3, 0),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }
        ));

        assert_debug_snapshot!(server.send_request::<request::GotoDefinition>(
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(file2),
                    Position::new(4, 0),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }
        ));
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

        assert_debug_snapshot!(server.send_request::<request::FoldingRangeRequest>(
            FoldingRangeParams {
                text_document: TextDocumentIdentifier::new(uri),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }
        ));
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

        assert_debug_snapshot!(server.send_request::<request::DocumentSymbolRequest>(
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier::new(uri.clone()),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }
        ));
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
        assert_debug_snapshot!(
            server
                .send_request::<request::WorkspaceSymbolRequest>(WorkspaceSymbolParams {
                    query: String::new(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .unwrap()
                .map(|symbols| {
                    let WorkspaceSymbolResponse::Flat(mut symbols) = symbols else {
                        unreachable!()
                    };
                    symbols.sort_unstable_by(|left, right| left.name.cmp(&right.name));
                    symbols
                }),
        );

        // With query matching symbols are returned.
        assert_debug_snapshot!(server.send_request::<request::WorkspaceSymbolRequest>(
            WorkspaceSymbolParams {
                query: "foo".into(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }
        ));
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn test_rename() {
        let server = TestServer::new();

        let file1 = Url::from_file_path("/file1.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                file1.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                      # abc def
                      [abc def](#abc-def)
                      ",
                ),
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
                      [abc def](file1.md#abc-def)
                      ",
                ),
            ),
        });

        let sorted_edits = |edit: WorkspaceEdit| {
            edit.changes.map(|changes| {
                let mut xs: Vec<_> = changes.into_iter().collect();
                xs.sort_by(|a, b| a.0.cmp(&b.0));
                xs
            })
        };

        assert_debug_snapshot!(
            server
                .send_request::<request::Rename>(RenameParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(file1.clone()),
                        Position::new(1, 3),
                    ),
                    new_name: "foo bar".into(),
                    work_done_progress_params: WorkDoneProgressParams {
                        work_done_token: None,
                    },
                })
                .unwrap()
                .map(sorted_edits)
        );

        let file3 = Url::from_file_path("/file3.md").unwrap();
        server.send_notification::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                file3.clone(),
                "markdown".into(),
                1,
                dedent(
                    "
                      # h1
                      ## h2
                      ### h3
                      ",
                ),
            ),
        });

        assert_debug_snapshot!(
            server
                .send_request::<request::Rename>(RenameParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(file3.clone()),
                        Position::new(1, 2)
                    ),
                    new_name: "H1".into(),
                    work_done_progress_params: WorkDoneProgressParams {
                        work_done_token: None
                    },
                })
                .unwrap()
                .map(sorted_edits)
        );

        assert_debug_snapshot!(
            server
                .send_request::<request::Rename>(RenameParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(file3.clone()),
                        Position::new(2, 3)
                    ),
                    new_name: "H2".into(),
                    work_done_progress_params: WorkDoneProgressParams {
                        work_done_token: None
                    },
                })
                .unwrap()
                .map(sorted_edits)
        );

        assert_debug_snapshot!(
            server
                .send_request::<request::Rename>(RenameParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(file3.clone()),
                        Position::new(3, 4)
                    ),
                    new_name: "H3".into(),
                    work_done_progress_params: WorkDoneProgressParams {
                        work_done_token: None
                    },
                })
                .unwrap()
                .map(sorted_edits)
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
                    [ba](bar.md)
                    ",
                ),
            ),
        });

        assert_debug_snapshot!(server.notification::<notification::PublishDiagnostics>());
    }
}

#[salsa::db]
trait Db: salsa::Database {
    fn file(&self, uri: &Url) -> Option<SourceFile>;
}

#[derive(Clone, Default)]
#[salsa::db]
struct DbImpl {
    storage: salsa::Storage<Self>,

    files: HashMap<Url, SourceFile>,
}

impl salsa::Database for DbImpl {}

#[salsa::db]
impl Db for DbImpl {
    fn file(&self, uri: &Url) -> Option<SourceFile> {
        self.files.get(uri).copied()
    }
}

#[salsa::input]
struct SourceFile {
    #[returns(clone)]
    text: String,
}

fn parsed(db: &dyn Db, uri: &Url) -> Option<Document> {
    let file_ = db.file(uri)?;

    let document = DocumentBuilder {
        text: file_.text(db).clone(), // FIXME(bbannier): can we use a ref here?
        parsed_builder: |text: &String| ast::ParsedDocument::from(text.as_str()),
    }
    .build();

    Some(document)
}
