use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::env;
use std::net::Shutdown;

fn main() {
    let mut stream = UnixStream::connect("/tmp/conduit_db_test/admin.sock").unwrap();
    let args: Vec<String> = env::args().collect();
    let command = args[1..].join(" ");
    stream.write_all(command.as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    println!("{}", response);
}