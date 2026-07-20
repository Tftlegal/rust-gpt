use serde::Serialize;

/// Модель заказа (соответствует таблице orders)
#[derive(Debug, Serialize)]
pub struct Order {
    pub id: i64,
    pub customer_id: i64,
    pub product_id: i64,
    pub quantity: i32,
    pub total_cents: i64,
}
