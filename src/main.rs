use tokio::net::TcpListener;

mod prelude;
mod connection;
mod room;

const IP: &str = "0.0.0.0:42530";
const SYMM: usize = 32;

#[tokio::main]
async fn main() -> Result<()> {
    let mut listener = TcpListener::from(IP);
    // Create tasks to handle connections
    // Create tasks to handle rooms
    Ok(())
}
