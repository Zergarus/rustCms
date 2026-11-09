-- Подтверждение почты отдельно от активности: active=false значит и «не подтвердил»,
-- и «заблокирован админом». Импортированные пользователи уже подтверждены.
ALTER TABLE users ADD COLUMN confirmed BOOLEAN NOT NULL DEFAULT TRUE;
