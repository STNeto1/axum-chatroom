use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing, Router,
};
use futures::{lock::Mutex, sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tower_http::services::ServeDir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Our shared state
struct AppState {
    messages: Mutex<Vec<ChatMessage>>,
    // Channel used to send messages to all connected clients.
    tx: broadcast::Sender<ChatMessage>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ChatMessage {
    username: String,
    text: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "chat=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let (tx, _rx) = broadcast::channel(100);

    let app_state = Arc::new(AppState {
        tx,
        messages: Mutex::new(vec![]),
    });

    let app = Router::new()
        .route("/websocket", routing::get(websocket_handler))
        .with_state(app_state)
        .nest_service("/", ServeDir::new("assets"));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, state))
}

async fn websocket(stream: WebSocket, state: Arc<AppState>) {
    // By splitting, we can send and receive at the same time.
    let (mut sender, mut receiver) = stream.split();

    // Send all messages in the queue to the client.
    for msg in state.messages.lock().await.iter() {
        let _ = sender
            .send(Message::Text(serde_json::to_string(&msg).unwrap()))
            .await;
    }

    // We subscribe *before* sending the "joined" message, so that we will also
    // display it to our client.
    let mut rx = state.tx.subscribe();

    // Spawn the first task that will receive broadcast messages and send text
    // messages over the websocket to our client.
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            // In any websocket error, break loop.
            if sender
                .send(Message::Text(serde_json::to_string(&msg).unwrap()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Clone things we want to pass (move) to the receiving task.
    let tx = state.tx.clone();

    // Spawn a task that takes messages from the websocket, prepends the user
    // name, and sends them to all broadcast subscribers.
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = receiver.next().await {
            let chat_message = serde_json::from_str::<ChatMessage>(&text).unwrap();
            state.messages.lock().await.push(chat_message.clone());

            let _ = tx.send(chat_message);
        }
    });

    // If any one of the tasks run to completion, we abort the other.
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };
}
