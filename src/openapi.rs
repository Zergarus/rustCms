//! OpenAPI 3.0 публичного API: обёртка bxapi, ключевые маршруты и схемы.

use axum::{Json, extract::State, response::IntoResponse};
use serde_json::{Value, json};

use crate::state::AppState;

fn envelope_ref() -> Value {
    json!({ "description": "Ответ в обёртке bxapi", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Envelope" } } } })
}

fn error_ref(status: &str, description: &str) -> Value {
    json!({ status: { "description": description, "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Envelope" } } } } })
}

fn json_body(schema: Value) -> Value {
    json!({ "required": true, "content": { "application/json": { "schema": schema } } })
}

fn csrf_param() -> Value {
    json!({
        "name": "X-Bitrix-Csrf-Token", "in": "header", "required": true,
        "schema": { "type": "string" },
        "description": "Токен из GET /api/v1/base/csrf/token; должен совпасть с cookie CMS_CSRF"
    })
}

fn post(tag: &str, summary: &str, schema: Option<Value>) -> Value {
    let mut op = json!({
        "tags": [tag], "summary": summary,
        "parameters": [csrf_param()],
        "responses": {
            "200": envelope_ref(),
            "403": error_ref("403", "Ошибка прикладного уровня или invalid_csrf"),
            "429": error_ref("429", "Превышен rate-limit")
        }
    });
    if let Some(schema) = schema {
        op["requestBody"] = json_body(schema);
    }
    op
}

fn get(tag: &str, summary: &str, params: Vec<Value>) -> Value {
    json!({
        "tags": [tag], "summary": summary,
        "parameters": params,
        "responses": {
            "200": envelope_ref(),
            "403": error_ref("403", "Прикладная ошибка"),
            "429": error_ref("429", "Превышен rate-limit")
        }
    })
}

/// Документ OpenAPI.
pub async fn document(State(_state): State<AppState>) -> impl IntoResponse {
    let api_code = json!({
        "name": "apiCode", "in": "path", "required": true,
        "schema": { "type": "string" }, "description": "apiCode коллекции"
    });
    let id = json!({
        "name": "id", "in": "path", "required": true,
        "schema": { "type": "integer" }, "description": "ID записи/заказа"
    });
    let doc = json!({
        "openapi": "3.0.3",
        "info": {
            "title": "CMS public API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Публичный API CMS: обёртка bxapi {status, data, errors}; POST требует CSRF-токен. Полный список маршрутов — в README."
        },
        "servers": [{ "url": "/" }],
        "tags": [
            { "name": "base" }, { "name": "auth" }, { "name": "iblock" },
            { "name": "cart" }, { "name": "order" }, { "name": "profile" }, { "name": "service" }
        ],
        "paths": {
            "/api/health": { "get": get("service", "Живость сервиса и БД", vec![]) },
            "/metrics": { "get": get("service", "Метрики Prometheus (text/plain)", vec![]) },
            "/api/v1/base/csrf/token": { "get": get("base", "CSRF-токен и cookie CMS_CSRF", vec![]) },
            "/api/v1/auth/login": { "post": post("auth", "Вход", Some(json!({ "$ref": "#/components/schemas/LoginRequest" }))) },
            "/api/v1/auth/session": { "get": get("auth", "Текущая сессия", vec![]) },
            "/api/v1/auth/logout": { "post": post("auth", "Выход", None) },
            "/api/v1/iblock/list": { "get": get("iblock", "Коллекции с apiEnabled", vec![]) },
            "/api/v1/iblock/info/{apiCode}": { "get": get("iblock", "Настройки коллекции", vec![api_code]) },
            "/api/v1/iblock/{apiCode}/element/list": { "post": post("iblock", "Список записей", Some(json!({ "$ref": "#/components/schemas/ElementListRequest" }))) },
            "/api/v1/iblock/{apiCode}/element/{id}": { "post": post("iblock", "Запись по id", Some(json!({ "$ref": "#/components/schemas/ElementRequest" }))) },
            "/api/v1/iblock/{apiCode}/element/slug/{slug}": { "post": post("iblock", "Запись по символьному коду", Some(json!({ "$ref": "#/components/schemas/ElementRequest" }))) },
            "/api/v1/iblock/{apiCode}/section/list": { "post": post("iblock", "Разделы", Some(json!({ "$ref": "#/components/schemas/SectionListRequest" }))) },
            "/api/v1/iblock/{apiCode}/search": { "post": post("iblock", "Полнотекстовый поиск", Some(json!({ "$ref": "#/components/schemas/SearchRequest" }))) },
            "/api/v1/form/{code}": { "post": post("iblock", "Публичная форма", Some(json!({ "type": "object", "additionalProperties": true }))) },
            "/api/v1/cart": { "get": get("cart", "Корзина покупателя", vec![]) },
            "/api/v1/cart/items": { "post": post("cart", "Добавить товар", Some(json!({ "$ref": "#/components/schemas/CartItemRequest" }))) },
            "/api/v1/cart/items/{id}/quantity": { "post": post("cart", "Изменить количество", Some(json!({ "type": "object", "properties": { "quantity": { "type": "number" } }, "required": ["quantity"] }))) },
            "/api/v1/cart/items/{id}/remove": { "post": post("cart", "Удалить позицию", None) },
            "/api/v1/order/submit": { "post": post("order", "Оформить заказ", Some(json!({ "type": "object", "additionalProperties": true }))) },
            "/api/v1/order/pay": { "post": post("order", "Запустить оплату", Some(json!({ "type": "object", "properties": { "id": { "type": "integer" } }, "required": ["id"] }))) },
            "/api/v1/order/{id}/payment-channel": { "get": get("order", "Состояние оплаты заказа", vec![id.clone()]) },
            "/api/v1/profile/orders": { "get": get("profile", "История заказов", vec![]) },
            "/api/v1/profile/orders/{id}/repeat": { "post": post("profile", "Повторить заказ", None) }
        },
        "components": {
            "securitySchemes": {
                "session": { "type": "apiKey", "in": "cookie", "name": "CMS_SID" }
            },
            "schemas": {
                "Envelope": {
                    "type": "object",
                    "properties": {
                        "status": { "type": "string", "enum": ["success", "error"] },
                        "data": {},
                        "errors": { "type": "array", "items": { "$ref": "#/components/schemas/Error" } }
                    },
                    "required": ["status", "errors"]
                },
                "Error": {
                    "type": "object",
                    "properties": {
                        "code": { "type": "string" },
                        "message": { "type": "string" },
                        "customData": {}
                    }
                },
                "LoginRequest": {
                    "type": "object",
                    "properties": {
                        "login": { "type": "string" },
                        "password": { "type": "string" },
                        "remember": { "type": "boolean" }
                    },
                    "required": ["login", "password"]
                },
                "ElementListRequest": {
                    "type": "object",
                    "properties": {
                        "select": { "type": "array", "items": { "type": "string" } },
                        "filter": { "type": "object", "additionalProperties": true },
                        "order": { "type": "object", "additionalProperties": true },
                        "limit": { "type": "integer" },
                        "offset": { "type": "integer" }
                    }
                },
                "ElementRequest": {
                    "type": "object",
                    "properties": {
                        "select": { "type": "array", "items": { "type": "string" } },
                        "itemDescription": { "type": "array", "items": { "type": "string" } }
                    }
                },
                "SectionListRequest": {
                    "type": "object",
                    "properties": {
                        "select": { "type": "array", "items": { "type": "string" } },
                        "filter": { "type": "object", "additionalProperties": true },
                        "hasElements": { "type": "object", "additionalProperties": true }
                    }
                },
                "SearchRequest": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer" },
                        "select": { "type": "array", "items": { "type": "string" } },
                        "filter": { "type": "object", "additionalProperties": true }
                    },
                    "required": ["query"]
                },
                "CartItemRequest": {
                    "type": "object",
                    "properties": {
                        "productId": { "type": "integer" },
                        "quantity": { "type": "number" },
                        "storeId": { "type": "integer" }
                    },
                    "required": ["productId"]
                }
            }
        }
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        Json(doc),
    )
}
