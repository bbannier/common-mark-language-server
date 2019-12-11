# A Language Server for Markdown

This project is a proof-of concept implementation of the [Language Server
Protocol](https://microsoft.github.io/language-server-protocol/) for Markdown,
in particular [CommonMark](https://commonmark.org/). While this project might
be practically useful, at the moment this is more intended as an experiment.

The following features are implemented at the moment,

* hover
* completion for intra-document links
* jump to definition
* find references
* renaming of headings renames references across document
* folding
* document and workspace symbol
* basic broken link linting

## Installation

```sh
cargo install --git https://github.com/bbannier/common-mark-language-server
```

### Editor integration

#### Vim

##### coc

If you are using [coc](https://github.com/neoclide/coc.nvim.git) add the
following language server to your `coc-settings.json`,

```json
{
  "languageserver": {
    "common-mark-language-server": {
        "command": "common-mark-language-server",
        "filetypes": ["markdown"]
    }
  }
}
```

##### vim-lsp

If you are using [vim-lsp](https://github.com/prabirshrestha/vim-lsp) add the
following to your `.vimrc`,

```viml
au User lsp_setup call lsp#register_server({
            \ 'name': 'common-mark',
            \ 'cmd': {server_info->['common-mark-language-server']},
            \ 'whitelist': ['markdown'],
            \ })

autocmd FileType markdown setlocal omnifunc=lsp#complete
set foldmethod=expr
            \ foldexpr=lsp#ui#vim#folding#foldexpr()
            \ foldtext=lsp#ui#vim#folding#foldtext()
```

## Current limitations

### Single-threaded server implementation

The server implementation at the moment is single-threaded; all work
(deserializing requests; filesystem I/O; markdown parsing; computing results;
serializing responses) is perform sequentially. This seems not to lead to too
bad UX issues for even medium-sized projects, but will eventually become an
issue.

### No dependency tracking

We do not track dependencies between documents, so after a document update the
whole document database needs to be linted (links in the updated document or
referencing the updated document might now be invalid).
