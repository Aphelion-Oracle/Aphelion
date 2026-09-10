//! A stand-in for the venues a node polls.
//!
//! The node's collector is real and runs in the node process, so it has to be
//! given something to collect. Pointing it at Binance would make a test of
//! what several nodes do to each other depend on an exchange being up, on rate
//! limits, and on the price of Bitcoin -- none of which the harness is trying
//! to find out about.
//!
//! Each venue is served in its own response shape, because the parsers are
//! real too: Kraken's nested `result` object, Coinbase's RFC 3339 `time`,
//! Binance's flat bid/ask strings. A fixture that served one shape to all
//! three would skip the parsing the node actually does.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

/// What each venue is currently quoting, and whether it is answering at all.
#[derive(Default)]
struct Book {
    /// Mid price per venue, as a decimal string. Absent means "use the
    /// default", so a test only has to name the venue it wants to move.
    quotes: HashMap<String, String>,
    /// Venues that fail every request, as an exchange having an outage does.
    down: HashMap<String, bool>,
    default_price: String,
}

/// A running fake exchange, serving every venue from one socket.
pub struct Exchange {
    book: Arc<Mutex<Book>>,
    addr: SocketAddr,
}

impl Exchange {
    pub async fn start(default_price: &str) -> std::io::Result<Self> {
        let book = Arc::new(Mutex::new(Book {
            default_price: default_price.to_string(),
            ..Default::default()
        }));

        let app = Router::new()
            .route("/binance", get(binance))
            .route("/kraken", get(kraken))
            .route("/coinbase/{product}/ticker", get(coinbase))
            .with_state(Arc::clone(&book));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Ok(Self { book, addr })
    }

    /// The `APHELION_SOURCE_URL_*` overrides that point a node here.
    pub fn env(&self) -> Vec<(String, String)> {
        let base = format!("http://{}", self.addr);
        vec![
            (
                "APHELION_SOURCE_URL_BINANCE".into(),
                format!("{base}/binance"),
            ),
            (
                "APHELION_SOURCE_URL_KRAKEN".into(),
                format!("{base}/kraken"),
            ),
            (
                "APHELION_SOURCE_URL_COINBASE".into(),
                format!("{base}/coinbase"),
            ),
        ]
    }

    /// Move one venue's quote. Others keep quoting the default.
    pub fn quote(&self, venue: &str, price: &str) {
        self.book
            .lock()
            .unwrap()
            .quotes
            .insert(venue.to_string(), price.to_string());
    }

    /// Move every venue's quote at once.
    pub fn quote_all(&self, price: &str) {
        let mut book = self.book.lock().unwrap();
        book.default_price = price.to_string();
        book.quotes.clear();
    }

    /// Take a venue down, or bring it back.
    pub fn set_down(&self, venue: &str, down: bool) {
        self.book
            .lock()
            .unwrap()
            .down
            .insert(venue.to_string(), down);
    }
}

/// `None` when the venue is down, so the handler can answer the way that venue
/// fails rather than with a generic 500.
fn quote_for(book: &Arc<Mutex<Book>>, venue: &str) -> Option<(String, String)> {
    let book = book.lock().unwrap();
    if *book.down.get(venue).unwrap_or(&false) {
        return None;
    }
    let mid: f64 = book
        .quotes
        .get(venue)
        .unwrap_or(&book.default_price)
        .parse()
        .unwrap_or(0.0);
    // One basis point either side: wide enough to be a real two-sided book,
    // tight enough that the mid is the number the test asked for.
    Some((
        format!("{:.8}", mid * 0.9999),
        format!("{:.8}", mid * 1.0001),
    ))
}

async fn binance(
    State(book): State<Arc<Mutex<Book>>>,
    Query(_q): Query<HashMap<String, String>>,
) -> Json<Value> {
    match quote_for(&book, "binance") {
        Some((bid, ask)) => Json(json!({ "bidPrice": bid, "askPrice": ask })),
        // Binance reports an error as a code/msg object with no prices, which
        // the node's parser rejects for the missing field.
        None => Json(json!({ "code": -1121, "msg": "Invalid symbol." })),
    }
}

async fn kraken(
    State(book): State<Arc<Mutex<Book>>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let pair = q.get("pair").cloned().unwrap_or_else(|| "XBTUSD".into());
    match quote_for(&book, "kraken") {
        Some((bid, ask)) => Json(json!({
            "error": [],
            "result": { pair: { "b": [bid, "1", "1.000"], "a": [ask, "1", "1.000"] } }
        })),
        // Kraken reports failure inside a 200 response; the node checks the
        // error array explicitly, and this is that path.
        None => Json(json!({ "error": ["EService:Unavailable"], "result": {} })),
    }
}

async fn coinbase(
    State(book): State<Arc<Mutex<Book>>>,
    Path(_product): Path<String>,
) -> Json<Value> {
    match quote_for(&book, "coinbase") {
        Some((bid, ask)) => Json(json!({
            "bid": bid,
            "ask": ask,
            "time": chrono::Utc::now().to_rfc3339(),
        })),
        None => Json(json!({ "message": "NotFound" })),
    }
}
