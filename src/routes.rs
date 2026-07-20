use actix_web::{get, web, HttpResponse, Responder};
use deadpool_postgres::Pool;
use serde_json::json;
use std::sync::Arc;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::models::Order;

/// Статический JSON
#[get("/json")]
async fn json_handler() -> impl Responder {
    HttpResponse::Ok().json(json!({ "message": "Hello, World!" }))
}

/// Список заказов из БД
#[get("/orders")]
async fn orders_handler(pool: web::Data<Arc<Pool>>) -> impl Responder {
    let client = match pool.get().await {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Failed to get DB connection: {}", e);
            return HttpResponse::InternalServerError().body("DB connection error");
        }
    };

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
            return HttpResponse::InternalServerError().body("Query error");
        }
    };

    let mut orders = Vec::with_capacity(rows.len());
    for row in rows {
        let order = Order {
            id: row.try_get(0).unwrap_or_default(),
            customer_id: row.try_get(1).unwrap_or_default(),
            product_id: row.try_get(2).unwrap_or_default(),
            quantity: row.try_get(3).unwrap_or_default(),
            total_cents: row.try_get(4).unwrap_or_default(),
        };
        orders.push(order);
    }

    HttpResponse::Ok().json(orders)
}

/// Метрики CPU и RAM
#[get("/metrics")]
async fn metrics_handler() -> impl Responder {
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::new().with_cpu_usage())
            .with_memory(MemoryRefreshKind::new().with_ram()),
    );
    sys.refresh_all();

    // CPU usage (среднее по всем ядрам)
    let cpu_usage = sys.global_cpu_info().cpu_usage();

    // RAM usage
    let total_memory = sys.total_memory();
    let used_memory = sys.used_memory();
    let memory_usage_percent = if total_memory > 0 {
        (used_memory as f64 / total_memory as f64) * 100.0
    } else {
        0.0
    };

    let metrics = json!({
        "cpu_usage_percent": cpu_usage,
        "memory_usage_percent": memory_usage_percent,
        "memory_used_bytes": used_memory,
        "memory_total_bytes": total_memory,
    });

    HttpResponse::Ok().json(metrics)
}

