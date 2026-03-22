use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("ping");

    let socket_path = slop_proto::socket_path();

    let stream = UnixStream::connect(&socket_path).await.unwrap_or_else(|e| {
        eprintln!("Failed to connect to {}: {}", socket_path.display(), e);
        std::process::exit(1);
    });

    let (reader, mut writer) = stream.into_split();

    let body = match command {
        "status" => slop_proto::RequestBody::Status,
        "ping" => slop_proto::RequestBody::Ping,
        other => {
            eprintln!("Unknown command: {}", other);
            std::process::exit(1);
        }
    };

    let request = slop_proto::Request { id: 1, body };
    let mut json = serde_json::to_string(&request).unwrap();
    json.push('\n');
    writer.write_all(json.as_bytes()).await.unwrap();

    let mut lines = BufReader::new(reader).lines();
    if let Ok(Some(line)) = lines.next_line().await {
        let response: slop_proto::Response = serde_json::from_str(&line).unwrap();
        println!("{:?}", response);
    }
}
