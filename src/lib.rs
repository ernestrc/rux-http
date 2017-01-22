extern crate httparse;
#[macro_use]
extern crate rux;
#[macro_use]
extern crate log;
#[macro_use]
extern crate error_chain;

mod handler;
pub mod error;

pub use handler::{HttpHandler, HttpResult, HttpEvent, HttpResponseEvent};

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
