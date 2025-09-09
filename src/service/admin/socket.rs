use std::sync::Arc;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{UnixListener, UnixStream}};
use crate::{Result, services};
use super::command::AdminCommand;
use clap::Parser;

pub struct Service {
    listener: UnixListener,
}

impl Service {
    pub fn build(config: &crate::Config) -> Result<Arc<Self>> {
        let path = config.unix_socket_path.clone();
        let listener = UnixListener::bind(path)?;
        Ok(Arc::new(Self {
            listener,
        }))
    }

    pub fn start(self: &Arc<Self>) {
        let self2 = Arc::clone(self);
        tokio::spawn(async move {
            self2.run().await;
        });
    }

    async fn run(&self) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    self.handle_connection(stream).await;
                }
                Err(e) => {
                    tracing::error!("Failed to accept admin socket connection: {}", e);
                }
            }
        }
    }

    async fn handle_connection(&self, mut stream: UnixStream) {
        let mut buffer = Vec::new();
        if let Err(e) = stream.read_to_end(&mut buffer).await {
            tracing::error!("Failed to read from admin socket: {}", e);
            return;
        }

        let command_str = String::from_utf8_lossy(&buffer);
        let mut lines = command_str.lines();
        let command_line = lines.next().unwrap_or("");
        let body = lines.collect::<Vec<&str>>();

        let mut argv = match shell_words::split(command_line) {
            Ok(argv) => argv,
            Err(e) => {
                tracing::error!("Failed to parse admin command: {}", e);
                return;
            }
        };
        argv.insert(0, "conduit-admin".to_string());

        let admin_command = match AdminCommand::try_parse_from(&argv) {
            Ok(command) => command,
            Err(e) => {
                tracing::error!("Failed to parse admin command: {}", e);
                return;
            }
        };

        let result = services().admin.process_admin_command(admin_command, body).await;

        let response = match result {
            Ok(message) => {
                format!("{message:?}")
            }
            Err(e) => {
                format!("Error: {e}")
            }
        };

        if let Err(e) = stream.write_all(response.as_bytes()).await {
            // This can happen if the client disconnects early
            tracing::debug!("Failed to write to admin socket: {}", e);
        }
    }
}
