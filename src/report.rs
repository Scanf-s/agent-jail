/// Print a line to stderr as `eprintln!` does, but ignore a failed write.
///
/// `eprintln!` panics when it cannot write, for example when stderr is a pipe whose reader has
/// ended, as in `aj run ... 2>&1 | head -1`. The panic would end a runtime process before it
/// cleans up after the container.
macro_rules! report {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}
