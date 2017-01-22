error_chain! {
    types {
        Error, ErrorKind, ResultExt, Result;
    }

    links { 
        RuxError(::rux::error::Error, ::rux::error::ErrorKind);
    }

    foreign_links {
        IoError(::std::io::Error);
        ParseAddr(::std::net::AddrParseError);
        FromUtf8Error(::std::string::FromUtf8Error);
        Utf8Error(::std::str::Utf8Error);
        HttpParseError(::httparse::Error);
        NixError(::rux::error::NixError);
    }

    errors {
      MessageTooBig(max_size: usize) {
        description("message bigger than max message size")
        display("message bigger than max message size of {:?}", max_size)
      }
    }
}
