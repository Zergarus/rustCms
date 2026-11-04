//! Действия над заказом, общие для карточки и массовых операций: проверка, изменение и
//! решение, нужно ли письмо (отправляет вызывающий).

use sqlx::PgPool;

use super::repo;
use crate::stock::StockError;

/// Письмо, которое вызывающий должен отправить после изменения заказа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailEvent {
    /// Смена статуса (код нового статуса).
    StatusChanged(String),
    Paid,
    Canceled,
}

impl MailEvent {
    /// Код почтового события.
    pub fn event_name(&self) -> String {
        match self {
            MailEvent::StatusChanged(code) => format!("SALE_STATUS_CHANGED_{code}"),
            MailEvent::Paid => "SALE_ORDER_PAID".into(),
            MailEvent::Canceled => "SALE_ORDER_CANCEL".into(),
        }
    }
}

#[derive(Debug)]
pub enum ActionError {
    NotFound,
    Rejected(String),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ActionError {
    fn from(e: sqlx::Error) -> Self {
        ActionError::Db(e)
    }
}

impl From<StockError> for ActionError {
    fn from(e: StockError) -> Self {
        match e {
            StockError::Rejected(m) => ActionError::Rejected(m.join("; ")),
            StockError::Db(e) => ActionError::Db(e),
        }
    }
}

async fn ensure_exists(db: &PgPool, id: i64) -> Result<(), ActionError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM orders WHERE id = $1)")
        .bind(id)
        .fetch_one(db)
        .await?;
    if exists {
        Ok(())
    } else {
        Err(ActionError::NotFound)
    }
}

/// Смена статуса. Письмо — только если статус действительно изменился и у него `notify`.
pub async fn change_status(
    db: &PgPool,
    id: i64,
    status: &str,
    user_id: Option<i64>,
    comment: &str,
) -> Result<Option<MailEvent>, ActionError> {
    let notify: Option<bool> =
        sqlx::query_scalar("SELECT notify FROM order_statuses WHERE code = $1")
            .bind(status)
            .fetch_optional(db)
            .await?;
    let Some(notify) = notify else {
        return Err(ActionError::Rejected("Нет такого статуса".into()));
    };
    ensure_exists(db, id).await?;
    let changed = repo::set_status(db, id, status, user_id, comment).await?;
    Ok((changed && notify).then(|| MailEvent::StatusChanged(status.to_string())))
}

/// Оплата заказа. Письмо — только при изменении и установке «оплачен».
pub async fn change_paid(
    db: &PgPool,
    id: i64,
    paid: bool,
) -> Result<Option<MailEvent>, ActionError> {
    ensure_exists(db, id).await?;
    let changed = repo::set_paid(db, id, paid).await?;
    Ok((changed && paid).then_some(MailEvent::Paid))
}

/// Отмена заказа. Письмо — только при изменении и установке «отменён».
pub async fn change_canceled(
    db: &PgPool,
    id: i64,
    cancel: bool,
    reason: &str,
) -> Result<Option<MailEvent>, ActionError> {
    ensure_exists(db, id).await?;
    let changed = repo::set_canceled(db, id, cancel, reason).await?;
    Ok((changed && cancel).then_some(MailEvent::Canceled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{order_fixture, place_order};

    #[sqlx::test]
    async fn status_mail_only_when_changed_and_notify(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        sqlx::query("INSERT INTO order_statuses (code, name, notify) VALUES ('P', 'Оплачен', TRUE), ('Q', 'Тихий', FALSE)")
            .execute(&db)
            .await
            .unwrap();
        let ev = change_status(&db, id, "P", None, "").await.unwrap();
        assert_eq!(ev, Some(MailEvent::StatusChanged("P".into())));
        assert_eq!(ev.unwrap().event_name(), "SALE_STATUS_CHANGED_P");
        assert_eq!(change_status(&db, id, "P", None, "").await.unwrap(), None);
        assert_eq!(change_status(&db, id, "Q", None, "").await.unwrap(), None);
        match change_status(&db, id, "nope", None, "").await {
            Err(ActionError::Rejected(m)) => assert_eq!(m, "Нет такого статуса"),
            other => panic!("{other:?}"),
        }
    }

    #[sqlx::test]
    async fn paid_mail_only_on_set(db: PgPool) {
        let f = order_fixture(&db).await;
        let id = place_order(&db, &f, None).await;
        assert_eq!(
            change_paid(&db, id, true).await.unwrap(),
            Some(MailEvent::Paid)
        );
        assert_eq!(change_paid(&db, id, true).await.unwrap(), None);
        assert_eq!(change_paid(&db, id, false).await.unwrap(), None);
        assert_eq!(MailEvent::Paid.event_name(), "SALE_ORDER_PAID");
    }

    #[sqlx::test]
    async fn cancel_rejected_when_shipped(db: PgPool) {
        let f = order_fixture(&db).await;
        let shipped = place_order(&db, &f, None).await;
        sqlx::query("UPDATE shipments SET deducted = TRUE WHERE order_id = $1")
            .bind(shipped)
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            change_canceled(&db, shipped, true, "").await,
            Err(ActionError::Rejected(_))
        ));
        let id = place_order(&db, &f, None).await;
        assert_eq!(
            change_canceled(&db, id, true, "причина").await.unwrap(),
            Some(MailEvent::Canceled)
        );
        assert_eq!(change_canceled(&db, id, true, "").await.unwrap(), None);
        assert_eq!(MailEvent::Canceled.event_name(), "SALE_ORDER_CANCEL");
    }

    #[sqlx::test]
    async fn missing_order(db: PgPool) {
        let f = order_fixture(&db).await;
        let _ = f;
        assert!(matches!(
            change_status(&db, 999, "N", None, "").await,
            Err(ActionError::NotFound)
        ));
        assert!(matches!(
            change_paid(&db, 999, true).await,
            Err(ActionError::NotFound)
        ));
        assert!(matches!(
            change_canceled(&db, 999, true, "").await,
            Err(ActionError::NotFound)
        ));
    }
}
