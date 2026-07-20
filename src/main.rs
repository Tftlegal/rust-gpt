mod models;
mod routes;

use actix_web::{web, App, HttpServer};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use dotenvy::dotenv;
use std::env;
use std::sync::Arc;

use routes::{json_handler, metrics_handler, orders_handler};

/// Создаёт пул соединений с PostgreSQL
fn create_pool(database_url: &str) -> Result<Pool, anyhow::Error> {
    let config = database_url.parse::<tokio_postgres::Config>()?;
    let mgr_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };
    let manager = Manager::from_config(config, tokio_postgres::NoTls, mgr_config);
    let pool = Pool::builder(manager)
        .max_size(16)
        .runtime(Runtime::Tokio1)
        .build()?;
    Ok(pool)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let _ = dotenv();

    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set (in .env or environment)");

    let pool = match create_pool(&database_url) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("Failed to create DB pool: {}", e);
            std::process::exit(1);
        }
    };

    let bind_addr = env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_owned());

    println!("Starting server on http://{}", bind_addr);

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .service(json_handler)
            .service(orders_handler)
            .service(metrics_handler)
    })
    .workers(num_cpus::get()) // используем все ядра
    .bind(&bind_addr)?
    .run()
    .await
}
