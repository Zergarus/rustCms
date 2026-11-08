ALTER TABLE seo_settings
    ADD COLUMN canonical TEXT NOT NULL DEFAULT '' CHECK (char_length(canonical) <= 2000),
    ADD COLUMN robots TEXT NOT NULL DEFAULT '' CHECK (robots IN ('', 'index,follow', 'noindex,follow', 'index,nofollow', 'noindex,nofollow'));
