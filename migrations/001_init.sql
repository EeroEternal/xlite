-- Public model alias → one or more upstream channels (new-api style).
-- Weight is used for random pick; higher weight = more traffic.
-- Failover tries the next channel only before the first response byte.

CREATE TABLE providers (
    id            BIGSERIAL PRIMARY KEY,
    name          TEXT NOT NULL,
    base_url      TEXT NOT NULL,
    api_key       TEXT NOT NULL DEFAULT '',
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE models (
    id            BIGSERIAL PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE channels (
    id              BIGSERIAL PRIMARY KEY,
    model_id        BIGINT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    provider_id     BIGINT NOT NULL REFERENCES providers(id) ON DELETE RESTRICT,
    upstream_model  TEXT NOT NULL,
    weight          INT NOT NULL DEFAULT 1 CHECK (weight >= 1),
    priority        INT NOT NULL DEFAULT 0,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE INDEX idx_channels_model ON channels(model_id) WHERE enabled;

CREATE TABLE api_keys (
    id            BIGSERIAL PRIMARY KEY,
    name          TEXT NOT NULL,
    key_hash      TEXT NOT NULL UNIQUE,
    key_prefix    TEXT NOT NULL,
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE api_key_models (
    api_key_id    BIGINT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model_id      BIGINT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    PRIMARY KEY (api_key_id, model_id)
);
