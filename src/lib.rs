#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]

#[macro_use]
extern crate rental;

pub mod ast;
pub mod lsp;
