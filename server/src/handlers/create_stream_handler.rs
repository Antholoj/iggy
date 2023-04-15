use crate::handlers::STATUS_OK;
use anyhow::Result;
use std::net::SocketAddr;
use std::str::from_utf8;
use streaming::error::Error;
use streaming::system::System;
use tokio::net::UdpSocket;

pub const COMMAND: &[u8] = &[11];
const LENGTH: usize = 5;

pub async fn handle(
    input: &[u8],
    socket: &UdpSocket,
    address: SocketAddr,
    system: &mut System,
) -> Result<(), Error> {
    if input.len() < LENGTH {
        return Err(Error::InvalidCommand);
    }

    let stream = u32::from_le_bytes(input[..4].try_into().unwrap());
    let name = from_utf8(&input[4..]).unwrap();
    if name.len() > 100 {
        return Err(Error::InvalidStreamName);
    }

    system.create_stream(stream, name).await?;
    socket.send_to(STATUS_OK, address).await?;
    Ok(())
}
