use error::*;
use httparse::{Header, Request, Response};
use rux::{RawFd, Reset};
use rux::buf::ByteBuffer;
use rux::epoll::*;
use rux::error::{NixError, errno};
use rux::handler::*;
use rux::mux::{MuxEvent, MuxCmd};
use rux::sys::socket::*;

static ERR: &'static [u8] = b"HTTP/1.1 500 Internal Server Error\r\nAccess-Control-Allow-Headers: origin, content-type, accept\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Max-Age: 1728000\r\nAllow-Control-Allow-Methods: GET,POST,OPTIONS\r\nContent-Type: text/plain\r\nServer: Smeagol/0.1\r\nContent-Length: 0\r\n\r\n";
static DEFAULT_MAX_MESSAGE_SIZE: &'static usize = &(1024 * 1024);

pub struct HttpHandler<'b> {
  epfd: EpollFd,
  sockfd: RawFd,
  is_readable: bool,
  request: Option<Request<'b, 'b>>,
  parsing: bool,
  persist_connection: bool,
  completing: bool,
  is_writable: bool,
  closed_write: bool,
}

// TODO put in different file and implement Into MuxEvent
pub enum HttpResult<'b> {
  Parsed {
    request: Request<'b, 'b>,
    payload: &'b [u8],
  },
  Flushed,
  Incomplete,
  Close,
}

macro_rules! ok {
  ($sel: expr, $output_buffer: expr, $res:expr) => {{
    match $res {
      Ok(r) => r,
      Err(e) => {
        error!("http error: {}", e);
        $sel.persist_connection = false;
        $sel.completing = true;
        $output_buffer.reset();
        $output_buffer.write(ERR).unwrap();
        return $sel.try_write($output_buffer);
      }
    }
  }}
}

#[macro_export]
macro_rules! close {
  ($sel: expr, $output_buffer:expr) => {{
    if !$output_buffer.is_readable() {
      if $sel.closed_write && $sel.completing {
        return HttpResult::Close;
      }
      if $sel.persist_connection {
        $sel.reset();
        return HttpResult::Flushed;
      }
    }
  }}
}

#[macro_export]
macro_rules! next {
  ($cmd:expr) => {{
    match $cmd {
      e @ HttpResult::Close => {
        return e;
      },
      e => e
    }
  }}
}

// TODO provide convert from HttpResult to MuxCmd

impl<'b> HttpHandler<'b> {
  pub fn new(epfd: EpollFd, sockfd: RawFd) -> HttpHandler<'b> {
    HttpHandler {
      epfd: epfd,
      sockfd: sockfd,
      is_readable: false,
      parsing: true,
      request: None,
      persist_connection: true,
      completing: false,
      is_writable: false,
      closed_write: false,
    }
  }

  pub fn try_frame(&mut self, input_buffer: &'b mut ByteBuffer, output_buffer: &mut ByteBuffer)
                   -> HttpResult<'b> {
    trace!("try_frame()");

    if self.request.is_none() {
      return HttpResult::Incomplete;
    }

    let request_length;

    {
      let status =
        ok!(self, output_buffer, self.request.as_mut().unwrap().parse(From::from(&*input_buffer)));

      if !status.is_complete() {
        trace!("try_frame(): Incomplete");
        return HttpResult::Incomplete;
      }

      request_length = status.unwrap();

      let mut length = None;
      let mut conn_maybe = None;
      for header in self.request.as_mut().unwrap().headers.into_iter() {
        if header.name == "Content-Length" {
          length = Some(header.value);
        }
        if header.name == "Connection" {
          conn_maybe = ::std::str::from_utf8(header.value).ok();
        }
      }

      // https://tools.ietf.org/html/rfc7230#page-52
      self.persist_connection =
        conn_maybe.map_or(false, |c| !(c == "close")) ||
        self.request.as_mut().map_or(false, |r| r.version.map_or(false, |v| v == 1)) ||
        conn_maybe.map_or(false, |c| c == "keep-alive") || false;

      if length.is_none() {
        self.parsing = false;
        return HttpResult::Parsed {
          request: self.request.take().unwrap(),
          payload: &mut [],
        };
      }

      let len = ok!(self,
                    output_buffer,
                    ::std::str::from_utf8(length.unwrap())
                      .chain_err(|| "error decoding Content-Length header value"));

      let content_length: usize =
        ok!(self,
            output_buffer,
            len.parse()
              .chain_err(|| "error parsing Content-Length integer value"));

      let buflen = input_buffer.readable();
      let total_length = content_length + request_length;

      if total_length > buflen {
        trace!("try_frame(): could not parse payload: total_length {:?}; buflen {:?}",
               &total_length,
               &buflen);
        return HttpResult::Incomplete;
      }
    }

    self.parsing = false;
    HttpResult::Parsed {
      request: self.request.take().unwrap(),
      payload: input_buffer.slice(request_length),
    }
  }

  pub fn try_read(&mut self, input_buffer: &mut ByteBuffer, output_buffer: &mut ByteBuffer,
                  max_msg_size: usize)
                  -> Result<()> {
    trace!("try_read(): readable: {}; buffer space: {}", self.is_readable, input_buffer.writable());
    while self.is_readable && input_buffer.is_writable() {
      match syscall!(recv(self.sockfd, From::from(&mut *input_buffer), MSG_DONTWAIT))? {
        Some(0) => {
          trace!("try_read(): EOF");
          self.is_readable = false;
          self.closed_write = true;
        }
        Some(cnt) => {
          trace!("try_read(): read {} bytes", cnt);
          input_buffer.extend(cnt)
        }
        None => {
          trace!("try_read(): not ready");
          self.is_readable = false;
        }
      }
    }

    if self.is_readable && !input_buffer.is_writable() {
      // reserve more capacity and retry
      let additional = input_buffer.capacity();
      input_buffer.reserve(additional);
      if input_buffer.capacity() > max_msg_size {
        error!("message surpasses max msg size of {}", max_msg_size);
        bail!(ErrorKind::MessageTooBig(max_msg_size));
      }

      trace!("try_read(): increased input_buffer size by {}", additional);
      return self.try_read(input_buffer, output_buffer, max_msg_size);
    }

    Ok(())
  }

  pub fn try_write(&mut self, output_buffer: &mut ByteBuffer) -> HttpResult<'b> {
    close!(self, output_buffer);

    if !self.is_writable {
      trace!("self.is_writable: {:?}", &self.is_writable);
      return HttpResult::Incomplete;
    }

    let mut len = output_buffer.readable();

    while len > 0 {

      match ok!(self,
                output_buffer,
                syscall!(send(self.sockfd, From::from(&*output_buffer), MSG_DONTWAIT))) {
        None => {
          trace!("try_write: not ready");
          self.is_writable = false;
          break;
        }
        Some(cnt) => {
          trace!("try_write: written {} bytes", cnt);
          output_buffer.consume(cnt);
          if cnt == len {
            break;
          }
          len = output_buffer.readable();
        }
      }
    }

    close!(self, output_buffer);

    HttpResult::Flushed
  }

  // fn buffer<'headers, 'buf>(&mut self, response: Response<'headers, 'buf>) -> Result<()> {
  //   unimplemented!()
  //   // TODO
  //   // match msg.to_buffer(self.output_buffer) {
  //   //   Err(Error(ErrorKind::BufferTooSmall(msg), _)) => {
  //   //     let additional = self.output_buffer.capacity();
  //   //     self.output_buffer.reserve(additional);

  //   //     self.assert_max_size(&*self.output_buffer)?;

  //   //     trace!("SonicHandler::buffer(): increased output_buffer size by {}", additional);
  //   //     self.buffer(msg)
  //   //   }
  //   //   Ok(_) => Ok(()),
  //   //   Err(e) => Err(e),
  //   // }
  // }
}

// impl<'h, 'buf: 'headers, 'headers> Handler<'h, Response<'headers, 'buf>, Result<HttpResult<'h>>> for HttpHandler<'h> {
//   fn on_next(&'h mut self, response: Response<'headers, 'buf>) -> Result<HttpResult<'h>> {
//     self.buffer(response)?;
//     // FIXME self.output_buffer.
//     Ok(et_write!(self.epfd,
//                  &mut self.is_writable,
//                  self.sockfd,
//                  self.output_buffer,
//                  &mut self.closed_write,
//                  // FIXME
//                  true,
//                  HttpResult::Incomplete,
//                  HttpResult::Flushed,
//                  HttpResult::Close,
//                  // FIXME
//                  |epfd| ()))
//   }
// }

// TODO should include fd? should http hanlder be of multiple sockfd ? should it keep state?
pub struct HttpEvent<'a, 'b> {
  pub events: EpollEventKind,
  pub input_buffer: &'b mut ByteBuffer,
  pub output_buffer: &'a mut ByteBuffer,
  pub headers: &'b mut [Header<'b>],
}

pub struct HttpResponseEvent<'a, 'b> {
  pub response: &'a [u8],
  pub output_buffer: &'b mut ByteBuffer,
}

impl<'a, 'b> Handler<HttpEvent<'a, 'b>, HttpResult<'b>> for HttpHandler<'b> {
  fn on_next(&mut self, event: HttpEvent<'a, 'b>) -> HttpResult<'b> {
    let events = event.events;
    let mut state = HttpResult::Incomplete;

    if events.contains(EPOLLHUP) {
      trace!("socket fd {}: EPOLLHUP", &self.sockfd);
      return HttpResult::Close;
    }

    if events.contains(EPOLLRDHUP) {
      trace!("socket fd {}: EPOLLRDHUP", &self.sockfd);
      self.closed_write = true;
    }

    if events.contains(EPOLLERR) {
      let err = format!("socket fd {}: EPOLLERR", &self.sockfd);
      let res: Result<()> = Err(err.into());
      ok!(self, event.output_buffer, res);
    }

    // TODO state machine, write state is not bubbled up correclty
    // use state machine diagram
    if events.contains(EPOLLOUT) {
      trace!("socket fd {}: EPOLLOUT", &self.sockfd);
      self.is_writable = true;
      trace!("self.is_writable = {:?}", &self.is_writable);
      state = self.try_write(event.output_buffer);
    }

    if self.parsing {
      if events.contains(EPOLLIN) {
        trace!("socket fd {}: EPOLLIN", &self.sockfd);
        self.is_readable = true;
        ok!(self,
            event.output_buffer,
            self.try_read(event.input_buffer, event.output_buffer, *DEFAULT_MAX_MESSAGE_SIZE));
      }

      if self.request.is_none() {
        self.request = Some(Request::new(event.headers));
        trace!("set self.request to some: {:?}", &self.request.is_some());
      }
      self.try_frame(event.input_buffer, event.output_buffer)
    } else {
      state
    }
  }
}

impl<'b> EpollHandler for HttpHandler<'b> {
  fn interests() -> EpollEventKind {
    EPOLLIN | EPOLLOUT | EPOLLET
  }

  fn with_epfd(&mut self, epfd: EpollFd) {
    self.epfd = epfd;
  }
}

impl<'b> Reset for HttpHandler<'b> {
  fn reset(&mut self) {
    self.request = None;
    self.persist_connection = false;
    self.completing = false;
  }
}
