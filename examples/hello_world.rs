#[macro_use]
extern crate log;
extern crate httparse;
#[macro_use]
extern crate rux_http;
#[macro_use]
extern crate rux;
extern crate env_logger;

use httparse::{Request, Response, Header, EMPTY_HEADER};
use rux::*;
use rux::buf::*;
use rux::daemon::*;
use rux::epoll::*;
use rux::handler::*;
use rux::mux::*;
use rux::prop::server::*;

use rux_http::{HttpResult, HttpResponseEvent, HttpEvent, HttpHandler};

const BUF_SIZE: usize = 2048;
const EPOLL_BUF_CAP: usize = 2048;
const EPOLL_LOOP_MS: isize = -1;
const MAX_CONN: usize = 50;
static DEFAULT_MAX_MESSAGE_SIZE: &'static usize = &(1024 * 1024);
static OK: &'static [u8] = b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Headers: origin, content-type, accept\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Max-Age: 1728000\r\nAllow-Control-Allow-Methods: GET,POST,OPTIONS\r\nContent-Type: text/plain\r\nServer: Rux-Http/0.1\r\nContent-Length: 0\r\n\r\n";

pub struct HelloHandler<'b> {
  sockfd: RawFd,
  handler: HttpHandler<'b>,
}

impl<'b> Handler<MuxEvent<'b, Resource<'b>>, MuxCmd> for HelloHandler<'b> {
  fn on_next(&mut self, event: MuxEvent<'b, Resource<'b>>) -> MuxCmd {
    let resource = event.resource;
    let events = event.events;

    match self.handler.on_next(HttpEvent {
      events: events,
      input_buffer: &mut resource.input_buffer,
      output_buffer: &mut resource.output_buffer,
      headers: &mut resource.headers
    }) {
      HttpResult::Close | HttpResult::Flushed => MuxCmd::Close,
      HttpResult::Incomplete => MuxCmd::Keep,
      HttpResult::Parsed { .. } => {
        resource.output_buffer.write(OK).unwrap();
        match self.handler.try_write(&mut resource.output_buffer) {
          HttpResult::Close => MuxCmd::Close,
          HttpResult::Flushed => {
            // TODO self.handler.keep_alive ?
            // self.handler.reset(self.epfd);
            MuxCmd::Close
          }
          HttpResult::Parsed { .. } => unimplemented!(),
          HttpResult::Incomplete => MuxCmd::Keep,
        }
      }
    }
  }
}

impl<'b> EpollHandler for HelloHandler<'b> {
  fn interests() -> EpollEventKind {
    HttpHandler::interests()
  }

  fn with_epfd(&mut self, epfd: EpollFd) {
    self.handler.with_epfd(epfd);
  }
}

#[derive(Clone, Debug)]
struct Factory;

#[derive(Clone, Debug)]
struct Resource<'b> {
  input_buffer: ByteBuffer,
  output_buffer: ByteBuffer,
  headers: [Header<'b>; 24],
}

impl<'b> Reset for HelloHandler<'b> {
  fn reset(&mut self) {
    self.handler.reset();
  }
}

impl<'b> Reset for Resource<'b> {
  fn reset(&mut self) {
    self.input_buffer.reset();
    self.output_buffer.reset();
    self.headers = [EMPTY_HEADER; 24];
  }
}

impl<'b> HandlerFactory<'b, HelloHandler<'b>, Resource<'b>> for Factory {
  fn new_resource(&self) -> Resource<'b> {
    Resource {
      headers: [EMPTY_HEADER; 24],
      input_buffer: ByteBuffer::with_capacity(BUF_SIZE),
      output_buffer: ByteBuffer::with_capacity(BUF_SIZE),
    }
  }

  fn new_handler(&mut self, epfd: EpollFd, sockfd: RawFd) -> HelloHandler<'b> {
    HelloHandler {
      sockfd: sockfd,
      handler: HttpHandler::new(epfd, sockfd),
    }
  }
}

fn main() {

  ::env_logger::init().unwrap();

  info!("BUF_SIZE: {}; EPOLL_BUF_CAP: {}; EPOLL_LOOP_MS: {}; MAX_CONN: {}",
        BUF_SIZE,
        EPOLL_BUF_CAP,
        EPOLL_LOOP_MS,
        MAX_CONN);

  let config = ServerConfig::tcp(("127.0.0.1", 9999))
    .unwrap()
    .max_conn(MAX_CONN)
    .io_threads(6)
    //.io_threads(1)
    .epoll_config(EpollConfig {
      loop_ms: EPOLL_LOOP_MS,
      buffer_capacity: EPOLL_BUF_CAP,
    });

  let server = Server::new(config, Factory).unwrap();

  Daemon::build(server)
    .with_sched(SCHED_FIFO, None)
    .run()
    .unwrap();
}
