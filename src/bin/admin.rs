use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::env;
use std::net::Shutdown;
use figment::{Figment, providers::{Format, Toml, Env}};
use serde::Deserialize;

use tracing::{error, info};

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
    tracing_subscriber::fmt::init();

    let config: Config = Figment::new()
        .merge(Toml::file(Env::var("CONDUIT_CONFIG").unwrap_or_else(| | {
            error!("CONDUIT_CONFIG env var not set");
            std::process::exit(1);
        })))
        .extract()
        .unwrap_or_else(|e| {
            error!("Could not parse config: {}", e);
            std::process::exit(1);
        });

    let mut stream = UnixStream::connect(config.global.unix_socket_path).unwrap_or_else(|e| {
        error!("Could not connect to admin socket: {}", e);
        std::process::exit(1);
    });
    let args: Vec<String> = env::args().collect();
    let command = args[1..].join(" ");

    let mut body = String::new();
    if command.contains(" - ") {
        info!("Reading from stdin...");
        std::io::stdin().read_to_string(&mut body).unwrap_or_else(|e| {
            error!("Could not read from stdin: {}", e);
            std::process::exit(1);
        });
    }

    let full_command = format!("{}\n{}", command, body);

    stream.write_all(full_command.as_bytes()).unwrap_or_else(|e| {
        error!("Could not write to admin socket: {}", e);
        std::process::exit(1);
    });
    stream.shutdown(Shutdown::Write).unwrap_or_else(|e| {
        error!("Could not shutdown admin socket: {}", e);
        std::process::exit(1);
    });

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap_or_else(|e| {
        error!("Could not read from admin socket: {}", e);
        std::process::exit(1);
    });

    // TODO: Find a better way to parse this
    let body = response.split("body: \"").collect::<Vec<&str>>()[1].split("\", formatted:").collect::<Vec<&str>>()[0];
    println!("{}", body.replace("\\n", "\n"));
}
