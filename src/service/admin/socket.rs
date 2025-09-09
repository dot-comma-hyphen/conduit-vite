use std::sync::Arc;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{UnixListener, UnixStream}};
use crate::{Result, services};
use super::command::AdminCommand;
use clap::Parser;

pub struct Service {
    listener: UnixListener,
}

impl Service {
    pub fn build() -> Result<Arc<Self>> {
        let path = services().globals.config.unix_socket_path.clone();
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
                    // Handle error
                }
            }
        }
    }

    async fn handle_connection(&self, mut stream: UnixStream) {
        let mut buffer = Vec::new();
        stream.read_to_end(&mut buffer).await.unwrap();

        let command_str = String::from_utf8_lossy(&buffer);
        let mut argv = shell_words::split(&command_str).unwrap();
        argv.insert(0, "conduit-admin".to_string());

        let admin_command = AdminCommand::try_parse_from(&argv).unwrap();

        let result = services().admin.process_admin_command(admin_command, Vec::new()).await;

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
