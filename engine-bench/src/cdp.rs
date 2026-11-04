//! A minimal Chrome DevTools Protocol client. It sends only what the harness
//! needs, because lighter engines implement part of the protocol.

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

type Reply = std::result::Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;

pub struct Cdp {
    outgoing: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: AtomicU64,
    call_timeout: Duration,
}

impl Cdp {
    pub async fn connect(ws_url: &str, call_timeout: Duration) -> Result<Self> {
        let (socket, _) = timeout(call_timeout, connect_async(ws_url))
            .await
            .context("timed out connecting to CDP")?
            .context("connecting to CDP")?;
        let (mut write, mut read) = socket.split();
        let (outgoing, mut queue) = mpsc::unbounded_channel::<Message>();
        let pending = Pending::default();

        tokio::spawn(async move {
            while let Some(message) = queue.recv().await {
                if write.send(message).await.is_err() {
                    break;
                }
            }
        });

        let waiting = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = read.next().await {
                if let Some((id, reply)) = parse_reply(&message)
                    && let Some(caller) = waiting.lock().unwrap().remove(&id)
                {
                    let _ = caller.send(reply);
                }
            }
            // Dropping the senders fails every call still waiting.
            waiting.lock().unwrap().clear();
        });

        Ok(Self {
            outgoing,
            pending,
            next_id: AtomicU64::new(1),
            call_timeout,
        })
    }

    pub async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session_id) = session_id {
            request["sessionId"] = json!(session_id);
        }

        let (caller, answer) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, caller);
        self.outgoing
            .send(Message::Text(request.to_string().into()))
            .ok()
            .with_context(|| format!("connection closed before {method}"))?;

        match timeout(self.call_timeout, answer).await {
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("{method} timed out")
            }
            Ok(Err(_)) => bail!("connection closed during {method}"),
            Ok(Ok(Err(error))) => bail!("{method} failed: {error}"),
            Ok(Ok(Ok(result))) => Ok(result),
        }
    }
}

/// Returns the call a message answers. Events have no id and are skipped.
fn parse_reply(message: &Message) -> Option<(u64, Reply)> {
    let reply: Value = serde_json::from_str(message.to_text().ok()?).ok()?;
    let id = reply["id"].as_u64()?;
    match reply.get("error") {
        Some(error) => Some((id, Err(error["message"].to_string()))),
        None => Some((id, Ok(reply["result"].clone()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{WebSocketStream, accept_async};

    type Socket = WebSocketStream<tokio::net::TcpStream>;

    const TIMEOUT: Duration = Duration::from_secs(2);

    /// Runs `script` as the server for one connection and returns its URL.
    async fn fake_engine<F, Fut>(script: F) -> String
    where
        F: FnOnce(Socket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            script(accept_async(stream).await.unwrap()).await;
        });
        format!("ws://127.0.0.1:{port}/")
    }

    async fn next_request(socket: &mut Socket) -> Value {
        let message = socket.next().await.unwrap().unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    async fn send(socket: &mut Socket, reply: Value) {
        socket
            .send(Message::Text(reply.to_string().into()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn call_returns_the_result_for_its_id() {
        let url = fake_engine(|mut socket| async move {
            let request = next_request(&mut socket).await;
            send(
                &mut socket,
                json!({"id": request["id"], "result": {"ok": true}}),
            )
            .await;
        })
        .await;

        let cdp = Cdp::connect(&url, TIMEOUT).await.unwrap();
        let result = cdp
            .call("Browser.getVersion", json!({}), None)
            .await
            .unwrap();

        assert_eq!(result, json!({"ok": true}));
    }

    #[tokio::test]
    async fn call_reports_protocol_errors() {
        let url = fake_engine(|mut socket| async move {
            let request = next_request(&mut socket).await;
            let error = json!({"code": -32601, "message": "'Page.navigate' wasn't found"});
            send(&mut socket, json!({"id": request["id"], "error": error})).await;
        })
        .await;

        let cdp = Cdp::connect(&url, TIMEOUT).await.unwrap();
        let error = cdp
            .call("Page.navigate", json!({}), None)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("wasn't found"), "{error}");
    }

    #[tokio::test]
    async fn out_of_order_replies_reach_the_right_caller() {
        let url = fake_engine(|mut socket| async move {
            let first = next_request(&mut socket).await;
            let second = next_request(&mut socket).await;
            send(
                &mut socket,
                json!({"id": second["id"], "result": {"method": second["method"]}}),
            )
            .await;
            send(
                &mut socket,
                json!({"id": first["id"], "result": {"method": first["method"]}}),
            )
            .await;
        })
        .await;

        let cdp = Cdp::connect(&url, TIMEOUT).await.unwrap();
        let (first, second) = tokio::join!(
            cdp.call("Page.enable", json!({}), None),
            cdp.call("Network.enable", json!({}), None)
        );

        assert_eq!(first.unwrap(), json!({"method": "Page.enable"}));
        assert_eq!(second.unwrap(), json!({"method": "Network.enable"}));
    }

    #[tokio::test]
    async fn session_id_is_sent_when_given() {
        let url = fake_engine(|mut socket| async move {
            let request = next_request(&mut socket).await;
            send(
                &mut socket,
                json!({"id": request["id"], "result": {"session": request["sessionId"]}}),
            )
            .await;
        })
        .await;

        let cdp = Cdp::connect(&url, TIMEOUT).await.unwrap();
        let result = cdp
            .call("Page.enable", json!({}), Some("abc"))
            .await
            .unwrap();

        assert_eq!(result, json!({"session": "abc"}));
    }

    #[tokio::test]
    async fn events_between_replies_are_ignored() {
        let url = fake_engine(|mut socket| async move {
            let request = next_request(&mut socket).await;
            send(
                &mut socket,
                json!({"method": "Page.loadEventFired", "params": {}}),
            )
            .await;
            send(
                &mut socket,
                json!({"id": request["id"], "result": {"ok": true}}),
            )
            .await;
        })
        .await;

        let cdp = Cdp::connect(&url, TIMEOUT).await.unwrap();
        let result = cdp.call("Page.enable", json!({}), None).await.unwrap();

        assert_eq!(result, json!({"ok": true}));
    }

    #[tokio::test]
    async fn a_closed_connection_fails_the_waiting_call() {
        let url = fake_engine(|mut socket| async move {
            next_request(&mut socket).await;
            socket.close(None).await.unwrap();
        })
        .await;

        let cdp = Cdp::connect(&url, TIMEOUT).await.unwrap();

        assert!(cdp.call("Page.enable", json!({}), None).await.is_err());
    }

    #[tokio::test]
    async fn a_silent_engine_times_out() {
        let url = fake_engine(|mut socket| async move {
            next_request(&mut socket).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;

        let cdp = Cdp::connect(&url, Duration::from_millis(100))
            .await
            .unwrap();
        let error = cdp.call("Page.enable", json!({}), None).await.unwrap_err();

        assert!(error.to_string().contains("timed out"), "{error}");
    }
}
