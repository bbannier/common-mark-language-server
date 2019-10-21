# A Language Server for Markdown

This project is a proof-of concept implementation of the [Language Server
Protocol](https://microsoft.github.io/language-server-protocol/) for Markdown,
in particular [CommonMark](https://commonmark.org/). While this project might
be practically useful, at the moment this is more intended as an experiment.

Currently supported are

* hover
* completion for intra-document links
* jump to definition
* find references
* folding

## Installation

```{.sh}
cargo install --git https://github.com/bbannier/common-mark-language-server
```

### Editor integration

#### Vim

If you are using [vim-lsp](https://github.com/prabirshrestha/vim-lsp) add the
following to your `.vimrc`,

```{.vim}
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
