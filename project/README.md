# Модуль проекта

Код конкретного сайта, который не нужен остальным: настройки API bxapi (ширины
картинок, характеристики, источник картинки товара, алиасы, правила хлебных
крошек и редиректов), декораторы ответов и группы поиска — аналог `bxapi.*`
в `/bitrix/.settings_extra.php` и регистрации классов в `init.php`.

Одна установка CMS — один проект. Файлы этого каталога, кроме README, в репозиторий
CMS не попадают — храните их в репозитории проекта.

## Как подключить

1. Положите `project/mod.rs` с функцией:

   ```rust
   use crate::bxapi::project::Project;

   pub fn project() -> Project {
       Project {
           image_widths: vec![120, 320, 800],
           ..Project::default()
       }
   }
   ```

   Код можно разбить на файлы: `mod decorators;` в `mod.rs` подключит
   `project/decorators.rs`.

2. Соберите CMS — `build.rs` подключит модуль автоматически. Без `project/mod.rs`
   действуют настройки по умолчанию.

Декоратор — тип с `impl crate::bxapi::project::Decorator`, группа поиска —
`impl crate::bxapi::project::SearchGroup`; регистрируются в полях
`detail_decorators`, `list_decorators`, `search_groups` структуры `Project`.
Корзина и оформление заказа (аналог `bxapi.basket` и `bxapi.order`) — поля `cart`
(`CartConfig`) и `order` (`OrderConfig`; ограничение доставок по корзине — `impl
crate::bxapi::project::DeliveryFilter` в `order.delivery_filter`).
