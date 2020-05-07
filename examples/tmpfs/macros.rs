macro_rules! io_format_err {
    ($($fmt:tt)*) => {
        ::std::io::Error::new(::std::io::ErrorKind::Other, format!($($fmt)*))
    }
}

macro_rules! io_bail {
    ($($fmt:tt)*) => { return Err(io_format_err!($($fmt)*).into()); }
}

macro_rules! io_return {
    ($errno:expr) => {
        return Err(::std::io::Error::from_raw_os_error($errno).into());
    };
}
