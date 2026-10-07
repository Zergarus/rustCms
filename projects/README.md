# Модули проектов

Код конкретного сайта, который не нужен остальным: настройки API bxapi (ширины
картинок, характеристики, источник картинки товара, алиасы, правила хлебных
крошек и редиректов), декораторы ответов и группы поиска — аналог `bxapi.*`
в `/bitrix/.settings_extra.php` и регистрации классов в `init.php`.

Файлы этого каталога, кроме README, в репозиторий CMS не попадают — храните их
в репозитории проекта.

## Как подключить

1. Положите `projects/<имя>.rs` (имя — `[a-z][a-z0-9_]*`) с функцией:

   ```rust
   use crate::bxapi::project::Project;

   pub fn project() -> Project {
       Project {
           image_widths: vec![120, 320, 800],
           ..Project::default()
       }
   }
   ```

2. Соберите CMS — `build.rs` подключит модуль автоматически.
3. Включите его: `BXAPI_PROJECT=<имя>` в `.env`.

Декоратор — тип с `impl crate::bxapi::project::Decorator`, группа поиска —
`impl crate::bxapi::project::SearchGroup`; регистрируются в полях
`detail_decorators`, `list_decorators`, `search_groups` структуры `Project`.
