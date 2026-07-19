//! Максимально производительный HTTP-сервер на Hyper 1.x с поддержкой PostgreSQL.
//! - /json → статический JSON (быстро)
//! - /orders → JSON-список заказов из БД

use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    os::fd::AsRawFd,
    pin::Pin,
    sync::Arc,
};

use bytes::Bytes;
use deadpool_postgres::{Config, Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use http_body_util::Full;
use hyper::{
    body::Incoming,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
    server::conn::http1::Builder,
    service::Service,
    Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use once_cell::sync::Lazy;
use serde::Serialize;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    net::TcpListener,
    signal,
    sync::oneshot,
};
use tokio_postgres::{NoTls, Row};

// -----------------------------------------------------------------------------
// Константы и статические ответы для /json
// -----------------------------------------------------------------------------

const JSON_BYTES: &[u8] = br#"{"message":"Hello, World!"}"#;
const JSON_LEN: usize = JSON_BYTES.len();
type Body = Full<Bytes>;

static JSON_RESPONSE: Lazy<Response<Body>> = Lazy::new(|| {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .header(CONTENT_LENGTH, JSON_LEN)
        .body(Full::new(Bytes::from_static(JSON_BYTES)))
        .unwrap()
});

static NOT_FOUND_RESPONSE: Lazy<Response<Body>> = Lazy::new(|| {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::new()))
        .unwrap()
});

// -----------------------------------------------------------------------------
// Модель Order и сериализация
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Order {
    id: i64,
    customer_id: i64,
    product_id: i64,
    quantity: i32,
    total_cents: i64,
}

/// Преобразует строку из PostgreSQL в Order
fn row_to_order(row: Row) -> Result<Order, tokio_postgres::Error> {
    Ok(Order {
        id: row.try_get(0)?,
        customer_id: row.try_get(1)?,
        product_id: row.try_get(2)?,
        quantity: row.try_get(3)?,
        total_cents: row.try_get(4)?,
    })
}

// -----------------------------------------------------------------------------
// Состояние приложения (пул соединений с БД)
// -----------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    pool: Pool,
}

// -----------------------------------------------------------------------------
// Кастомный Service с состоянием
// -----------------------------------------------------------------------------

#[derive(Clone)]
struct AppService {
    state: Arc<AppState>,
}

impl Service<Request<Incoming>> for AppService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let path = req.uri().path().to_owned();
        let state = self.state.clone();

        Box::pin(async move {
            match path.as_str() {
                "/json" => Ok(JSON_RESPONSE.clone()),
                "/orders" => handle_orders(state).await,
                _ => Ok(NOT_FOUND_RESPONSE.clone()),
            }
        })
    }
}

// -----------------------------------------------------------------------------
// Обработчик /orders
// -----------------------------------------------------------------------------

async fn handle_orders(state: Arc<AppState>) -> Result<Response<Body>, Infallible> {
    // Получаем соединение из пула
    let client = match state.pool.get().await {
        Ok(client) => client,
        Err(e) => {
            eprintln!("failed to get DB connection: {}", e);
            return Ok(internal_error_response());
        }
    };

    // Выполняем запрос
    let rows = match client
        .query(
            "SELECT id, customer_id, product_id, quantity, total_cents FROM orders ORDER BY id LIMIT 10",
            &[],
        )
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("DB query failed: {}", e);
            return Ok(internal_error_response());
        }
    };

    // Преобразуем строки в заказы
    let mut orders = Vec::with_capacity(rows.len());
    for row in rows {
        match row_to_order(row) {
            Ok(order) => orders.push(order),
            Err(e) => {
                eprintln!("failed to parse row: {}", e);
                return Ok(internal_error_response());
            }
        }
    }

    // Сериализуем в JSON
    let json = match serde_json::to_vec(&orders) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("failed to serialize JSON: {}", e);
            return Ok(internal_error_response());
        }
    };

    // Формируем успешный ответ
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .header(CONTENT_LENGTH, json.len())
        .body(Full::new(Bytes::from(json)))
        .unwrap())
}

/// Ответ 500 Internal Server Error
fn internal_error_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

// -----------------------------------------------------------------------------
// Создание пула соединений с БД
// -----------------------------------------------------------------------------

fn create_pool(database_url: &str) -> Result<Pool, anyhow::Error> {
    let config = database_url.parse::<tokio_postgres::Config>()?;
    let mgr_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };
    let manager = Manager::from_config(config, NoTls, mgr_config);
    let pool = Pool::builder(manager)
        .max_size(16)
        .runtime(Runtime::Tokio1)
        .build()?;
    Ok(pool)
}

// -----------------------------------------------------------------------------
// Создание сокета с SO_REUSEPORT (Unix) через libc
// -----------------------------------------------------------------------------

fn create_listener(addr: SocketAddr, reuseport: bool) -> std::io::Result<TcpListener> {
    let socket = Socket::new(
        Domain::for_address(addr),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;

    socket.set_reuse_address(true)?;

    #[cfg(unix)]
    if reuseport {
        unsafe {
            let yes: libc::c_int = 1;
            let ret = libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &yes as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    #[cfg(not(unix))]
    if reuseport {
        eprintln!("SO_REUSEPORT not supported on this platform, ignoring");
    }

    socket.set_nodelay(true)?;
    socket.bind(&addr.into())?;
    socket.listen(4096)?;
    socket.set_nonblocking(true)?;

    TcpListener::from_std(socket.into())
}

// -----------------------------------------------------------------------------
// Обработчик сигналов
// -----------------------------------------------------------------------------

async fn shutdown_signal() -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
        let _ = tx.send(());
    });
    rx
}

// -----------------------------------------------------------------------------
// Main
// -----------------------------------------------------------------------------

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // Загружаем переменные из .env файла (если есть)
    let _ = dotenvy::dotenv();

    // Читаем переменные окружения
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set");
    let bind_addr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_owned());

    let addr: SocketAddr = bind_addr.parse()?;
    let reuseport = std::env::var("SO_REUSEPORT").is_ok();

    // Создаём пул соединений с БД
    let pool = create_pool(&database_url)?;

    // Создаём слушающий сокет
    let listener = create_listener(addr, reuseport)?;
    println!("Listening on http://{} (reuseport={})", addr, reuseport);

    // Состояние приложения
    let state = Arc::new(AppState { pool });
    let service = AppService { state };

    // HTTP-билдер
    let mut builder = Builder::new();
    builder.keep_alive(true);

    let mut shutdown_rx = shutdown_signal().await;

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        if let Err(e) = stream.set_nodelay(true) {
                            eprintln!("failed to set TCP_NODELAY: {e}");
                        }

                        let io = TokioIo::new(stream);
                        let service = service.clone();
                        let builder = builder.clone();

                        tokio::spawn(async move {
                            if let Err(e) = builder.serve_connection(io, service).await {
                                eprintln!("connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("accept error: {e}");
                        break;
                    }
                }
            }
            _ = &mut shutdown_rx => {
                println!("Shutting down gracefully...");
                break;
            }
        }
    }

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    Ok(())
}

// -----------------------------------------------------------------------------
// Интеграционные тесты
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_test_server() -> String {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = create_listener(addr, false).unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{}", port);

        let database_url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set for tests");
        let pool = create_pool(&database_url).unwrap();

        let state = Arc::new(AppState { pool });
        let service = AppService { state };

        let mut builder = Builder::new();
        builder.keep_alive(true);

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let io = TokioIo::new(stream);
                        let service = service.clone();
                        let builder = builder.clone();
                        tokio::spawn(async move {
                            let _ = builder.serve_connection(io, service).await;
                        });
                    }
                    Err(e) => {
                        eprintln!("test server accept error: {e}");
                        break;
                    }
                }
            }
        });

        url
    }

    #[tokio::test]
    async fn test_json_endpoint() {
        let base_url = spawn_test_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/json", base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["message"], "Hello, World!");
    }

    #[tokio::test]
    async fn test_orders_endpoint() {
        let base_url = spawn_test_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/orders", base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let orders: Vec<Order> = resp.json().await.unwrap();
        // Проверяем, что вернулся список (может быть пустым, но это нормально)
        assert!(orders.len() <= 10);
    }

    #[tokio::test]
    async fn test_not_found() {
        let base_url = spawn_test_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/unknown", base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }
}
