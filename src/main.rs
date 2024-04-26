use std::time::Duration;

use env_logger::Env;
use log::{info, warn};
use tokio::{io::{AsyncReadExt, AsyncWriteExt, Interest}, net::{TcpListener, TcpStream}};

mod router;

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    let bind_addr = "0.0.0.0:17677";
    warn!("Starting server on: {}", bind_addr);
    listen(bind_addr).await;
}

async fn listen(bind_addr: &str) {
    let listener = TcpListener::bind(bind_addr).await.unwrap();
    info!("Listening on: {}", bind_addr);
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            run_connection(stream).await;
        });
    }
}

async fn run_connection(mut stream: TcpStream) {
    info!("New connection from {:?}", stream.peer_addr().unwrap());
    let r = stream.ready(Interest::READABLE | Interest::WRITABLE).await.unwrap();
    info!("Ready: {:?}", r);
    stream.write_u32_le(1234).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    stream.write_u32_le(22).await.unwrap();
    stream.write_u32_le(222).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    stream.write_u32_le(33).await.unwrap();
    stream.write_u32_le(333).await.unwrap();
    stream.write_u32_le(333).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stream.write_u32_le(333).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stream.write_u32_le(333).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stream.write_u32_le(333).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    stream.write_u32_le(44).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    stream.write_u32_le(55).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    stream.write_u32_le(66).await.unwrap();
    while stream.readable().await.is_ok() {
        match stream.read_u32_le().await {
            Ok(u) => {
                info!("Received: {}", u);
                if u == 6 {
                    break;
                }
            }
            Err(e) => {
                info!("Error: {:?}", e);
                let r = stream.ready(Interest::READABLE | Interest::WRITABLE).await.unwrap();
                info!("Ready: {:?}", r);
                break;
            }
        }
    }
    info!("closing Connection");
    stream.shutdown().await.unwrap();
    // stream.
}
