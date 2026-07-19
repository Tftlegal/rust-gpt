//! Axum + tokio-postgres сервер с кэшированием подготовленных запросов.
//! Эндпоинты:
//! - GET /json → возвращает `{"message":"Hello, World!"}` (оптимизирован)
//! - GET /orders → возвращает JSON-список первых 10 заказов из таблицы `orders`.

use std::{collections::HashMap, env, error::Error, io, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{
        header::{CONTENT_LENGTH, CONTENT_TYPE},
        HeaderValue, Response, StatusCode,
    },
    response::{IntoResponse, Response as AxumResponse},
    routing::get,
};
use bytes::Bytes;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_postgres::{Client, NoTls, Statement};

// Константы
const SELECT_ORDERS: &str =
    "SELECT id, customer_id, product_id, quantity, total_cents FROM orders ORDER BY id LIMIT 10";

// -----------------------------------------------------------------------------
// Состояние приложения
// -----------------------------------------------------------------------------

struct AppState {
    client: MyClient,
    select_orders: Statement,
}

// -----------------------------------------------------------------------------
// Модель заказа
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Order {
    id: i64,
    customer_id: i64,
    product_id: i64,
    quantity: i32,
    total_cents: i64,
}

// -----------------------------------------------------------------------------
// Обработчик ошибок базы данных
// -----------------------------------------------------------------------------

struct DatabaseError(tokio_postgres::Error);

impl IntoResponse for DatabaseError {
    fn into_response(self) -> AxumResponse {
        eprintln!("postgres request failed: {}", self.0);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

impl From<tokio_postgres::Error> for DatabaseError {
    fn from(err: tokio_postgres::Error) -> Self {
        DatabaseError(err)
    }
}

// -----------------------------------------------------------------------------
// Клиент с кэшем подготовленных запросов
// -----------------------------------------------------------------------------

struct MyClient {
    client: Client,
    cache: HashMap<String, Statement>,
}

impl MyClient {
    async fn prepare_cached(&mut self, sql: &str) -> Result<Statement, tokio_postgres::Error> {
        if let Some(stmt) = self.cache.get(sql) {
            Ok(stmt.clone())
        } else {
            let stmt = self.client.prepare(sql).await?;
            self.cache.insert(sql.to_string(), stmt.clone());
            Ok(stmt)
        }
    }

    async fn query(
        &self,
        stmt: &Statement,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
        self.client.query(stmt, params).await
    }
}

// -----------------------------------------------------------------------------
// Обработчики Axum
// -----------------------------------------------------------------------------

/// Оптимизированный эндпоинт /json – без аллокаций, статический байтовый буфер.
#[inline(always)]
async fn json() -> Response<Body> {
    const JSON_BYTES: &[u8] = br#"{"message":"Hello, World!"}"#;
    let mut resp = Response::new(Body::from(Bytes::from_static(JSON_BYTES)));
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(CONTENT_LENGTH, HeaderValue::from_static("27"));
    resp
}

/// Возвращает список заказов из БД.
async fn orders(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Order>>, DatabaseError> {
    let rows = state
        .client
        .query(&state.select_orders, &[])
        .await?;

    let mut orders: Vec<Order> = Vec::with_capacity(rows.len());
    for row in rows {
        orders.push(Order {
            id: row.try_get(0)?,
            customer_id: row.try_get(1)?,
            product_id: row.try_get(2)?,
            quantity: row.try_get(3)?,
            total_cents: row.try_get(4)?,
        });
    }

    Ok(Json(orders))
}

// -----------------------------------------------------------------------------
// Обработка сигналов завершения (Unix)
// -----------------------------------------------------------------------------

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            eprintln!("failed to install SIGTERM handler: {error}");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                eprintln!("failed to install Ctrl+C handler: {error}");
            }
        }
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// -----------------------------------------------------------------------------
// Точка входа
// -----------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _ = dotenvy::dotenv();

    let database_url = env::var("DATABASE_URL").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "DATABASE_URL must contain a PostgreSQL connection string",
        )
    })?;

    let bind_addr = env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_owned());

    let mut postgres_config: tokio_postgres::Config = database_url.parse()?;
    postgres_config.application_name("axum-battle");

    let (client, connection) = postgres_config.connect(NoTls).await?;

    let mut my_client = MyClient {
        client,
        cache: HashMap::with_capacity(1000),
    };

    let connection_task = tokio::spawn(connection);
    let select_orders = my_client.prepare_cached(SELECT_ORDERS).await?;

    let app = Router::new()
        .route("/json", get(json))        // оптимизированный обработчик
        .route("/orders", get(orders))
        .with_state(Arc::new(AppState {
            client: my_client,
            select_orders,
        }));

    let listener = TcpListener::bind(&bind_addr).await?;
    println!("axum-battle listening on http://{bind_addr}");

    let server_future = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());

    let outcome: Result<(), Box<dyn Error>> = tokio::select! {
        result = async { server_future.await } => result.map_err(Into::into),
        result = connection_task => {
            let message = match result {
                Ok(Ok(())) => "PostgreSQL connection closed".to_owned(),
                Ok(Err(error)) => format!("PostgreSQL connection failed: {error}"),
                Err(error) => format!("PostgreSQL connection task failed: {error}"),
            };
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, message).into())
        }
    };

    outcome
}

// -----------------------------------------------------------------------------
// Тесты
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;
    use std::net::SocketAddr;

    async fn spawn_test_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let database_url = env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set for integration tests");
        let mut postgres_config: tokio_postgres::Config = database_url.parse().unwrap();
        postgres_config.application_name("axum-battle-test");

        let (client, connection) = postgres_config.connect(NoTls).await.unwrap();
        let connection_task = tokio::spawn(connection);

        let mut my_client = MyClient {
            client,
            cache: HashMap::with_capacity(1000),
        };
        let select_orders = my_client.prepare_cached(SELECT_ORDERS).await.unwrap();

        let app = Router::new()
            .route("/json", get(json))
            .route("/orders", get(orders))
            .with_state(Arc::new(AppState {
                client: my_client,
                select_orders,
            }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async {
            axum::serve(listener, app)
                .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.ok() })
                .await
                .unwrap();
        });

        (addr, server_handle)
    }

    #[tokio::test]
    async fn test_json_endpoint() {
        let (addr, handle) = spawn_test_server().await;
        let client = Client::new();
        let url = format!("http://{}/json", addr);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["message"], "Hello, World!");
        handle.abort();
    }

    #[tokio::test]
    async fn test_orders_endpoint() {
        let (addr, handle) = spawn_test_server().await;
        let client = Client::new();
        let url = format!("http://{}/orders", addr);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let orders: Vec<Order> = resp.json().await.unwrap();
        assert!(orders.len() <= 10);
        handle.abort();
    }
}

