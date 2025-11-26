-- まだテーブルがなければ作成する
CREATE TABLE IF NOT EXISTS tickets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(), -- 一意なID
    number INT NOT NULL,                           -- 整理番号 (1, 2, 3...)
    group_size INT NOT NULL,                       -- 人数
    status TEXT NOT NULL DEFAULT 'waiting',        -- waiting, called, completed
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()  -- 発行日時
);
