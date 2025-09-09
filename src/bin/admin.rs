use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::env;
use std::net::Shutdown;
use figment::{Figment, providers::{Format, Toml, Env}};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    global: Global,
}

#[derive(Deserialize, Default)]
struct Global {
    unix_socket_path: String,
}

fn main() {
    let config: Config = Figment::new()
        .merge(Toml::file(Env::var("CONDUIT_CONFIG").expect("CONDUIT_CONFIG env var not set")))
        .extract()
        .expect("Could not parse config");

    let mut stream = UnixStream::connect(config.global.unix_socket_path).expect("Could not connect to admin socket");
    let args: Vec<String> = env::args().collect();
    let command = args[1..].join(" ");
    stream.write_all(command.as_bytes()).expect("Could not write to admin socket");
    stream.shutdown(Shutdown::Write).expect("Could not shutdown admin socket");

    let mut response = String::new();
    stream.read_to_string(&mut response).expect("Could not read from admin socket");

    // TODO: Find a better way to parse this
    let body = response.split("body: \"").collect::<Vec<&str>>()[1].split("\", formatted:").collect::<Vec<&str>>()[0];
    println!("{}", body.replace("\\n", "\n"));
}
