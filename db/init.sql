CREATE EXTENSION IF NOT EXISTS unaccent;

CREATE OR REPLACE FUNCTION immutable_unaccent(text)
RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE STRICT
AS $$ SELECT public.unaccent('public.unaccent', $1) $$;

CREATE TABLE IF NOT EXISTS judikatura (
    id                   BIGSERIAL PRIMARY KEY,
    spisova_znacka       TEXT NOT NULL UNIQUE,
    -- lookup key: no accents, spaces or dots, lower case
    spisova_znacka_norm  TEXT GENERATED ALWAYS AS (
        lower(replace(replace(immutable_unaccent(spisova_znacka), ' ', ''), '.', ''))
    ) STORED,
    datum_rozhodnuti     DATE,
    soud                 TEXT,
    popularni_nazev      TEXT,
    pravni_veta          TEXT,
    text_dokumentu       TEXT,
    abstrakt             TEXT,
    ecli                 TEXT,
    kategorie            TEXT,
    created_at           TIMESTAMP(0) NOT NULL DEFAULT CURRENT_TIMESTAMP(0),
    updated_at           TIMESTAMP(0) NOT NULL DEFAULT CURRENT_TIMESTAMP(0)
);

CREATE INDEX IF NOT EXISTS judikatura_norm_prefix_idx
    ON judikatura (spisova_znacka_norm text_pattern_ops);
