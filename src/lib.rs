#![no_std]
#![feature(linked_list_retain)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::missing_safety_doc,
    clippy::needless_pass_by_value,
    clippy::new_without_default,
    clippy::wildcard_imports
)]
extern crate alloc;

mod actor;
mod error;
mod net;
pub mod retries;
mod v1;
mod v2;

pub use actor::*;
pub use error::ErrPrinter;
pub(crate) use net::*;
pub use v1::*;
pub use v2::*;
