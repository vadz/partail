# partail

`partail` ("parallel tail") follows several log files at once, each in its own
window of a terminal UI, colouring their lines using regular expressions.

It is inspired by [multitail](https://github.com/folkertvanheusden/multitail),
but is written in safe Rust and designed to cope with untrusted input: invalid
UTF-8, terminal escape sequences and other control characters in the log files
are shown visibly instead of being passed to the terminal.

**Status:** under development, not usable yet.

## Building

Requires Rust 1.88 or later.

```sh
cargo build --release
```
