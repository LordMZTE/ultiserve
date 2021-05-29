# ultiserve
A file server that serves local files over http with some fancy bits.

## Features
- File index with icons
- Syntax highlighting
- (Coming soon) markdown rendering
- Completely JS-free

## Compiling
```sh
git clone https://github.com/lordmzte/ultiserve.git
cd ultiserve
cargo build --release
```

## Usage
```
Serve your files over http!

USAGE:
    ultiserve [OPTIONS] [dir]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
    -a, --addr <addr>    The address to bind the server to [default: 127.0.0.1:8080]

ARGS:
    <dir>    The directory to serve [default: .]
```
