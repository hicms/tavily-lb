use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Socks {
    pub url: String,
    pub destinations: Arc<Mutex<Vec<(String, u16)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Socks {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Socks {
    pub async fn start(upstream_url: &str) -> Self {
        let target = upstream_url.trim_start_matches("http://").to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("socks5h://{}", listener.local_addr().unwrap());
        let destinations = Arc::new(Mutex::new(Vec::new()));
        let recorded = destinations.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let target = target.clone();
                let recorded = recorded.clone();
                connections.spawn(async move {
                    let mut greeting = [0; 2];
                    socket.read_exact(&mut greeting).await.unwrap();
                    assert_eq!(greeting[0], 5);
                    let mut methods = vec![0; greeting[1] as usize];
                    socket.read_exact(&mut methods).await.unwrap();
                    assert!(methods.contains(&0));
                    socket.write_all(&[5, 0]).await.unwrap();
                    let mut connect = [0; 4];
                    socket.read_exact(&mut connect).await.unwrap();
                    assert_eq!(&connect, &[5, 1, 0, 3], "socks5h must resolve at the proxy");
                    let length = socket.read_u8().await.unwrap() as usize;
                    let mut domain = vec![0; length];
                    socket.read_exact(&mut domain).await.unwrap();
                    let port = socket.read_u16().await.unwrap();
                    recorded
                        .lock()
                        .unwrap()
                        .push((String::from_utf8(domain).unwrap(), port));
                    let mut target = TcpStream::connect(target).await.unwrap();
                    socket
                        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut target).await;
                });
                while let Some(result) = connections.try_join_next() {
                    result.unwrap();
                }
            }
        });
        Self {
            url,
            destinations,
            task,
        }
    }
}
