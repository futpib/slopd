//! Ephemeral SSH forwarder using the Android app's exact Iroh dependency graph.
fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .expect("usage: test-server SSH_PORT")
        .parse()
        .expect("invalid SSH port");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime")
        .block_on(async {
            let server = iroh_ssh::IrohSsh::builder()
                .accept_incoming(true)
                .accept_port(port)
                .build()
                .await
                .expect("start Iroh test server");
            println!("endpoint={}", server.endpoint_id());
            std::io::Write::flush(&mut std::io::stdout()).expect("flush endpoint ID");
            std::future::pending::<()>().await;
            drop(server);
        });
}
