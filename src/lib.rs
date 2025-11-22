#![no_std]
#![feature(linked_list_retain)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::new_without_default,
    clippy::missing_safety_doc,
    clippy::missing_errors_doc,
    clippy::wildcard_imports,
    clippy::needless_pass_by_value
)]
extern crate alloc;
mod actor;
mod error;
pub mod retries;
pub use actor::*;
pub use error::ErrPrinter;
