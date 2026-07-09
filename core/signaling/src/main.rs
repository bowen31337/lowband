use lowband_signaling::{router, AppState};

#[tokio::main]
async fn main() {
    let addr = std::env::var("SIGNALING_BIND").unwrap_or_else(|_| "0.0.0.0:3478".into());
    let db_path = std::env::var("SIGNALING_DB").unwrap_or_else(|_| ":memory:".into());
    let state = AppState::open(&db_path).unwrap_or_else(|e| {
        eprintln!("lowband-signaling: open db {db_path}: {e}");
        std::process::exit(1);
    });
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        eprintln!("lowband-signaling: bind {addr}: {e}");
        std::process::exit(1);
    });
    eprintln!("lowband-signaling: listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
