use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users(
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, pw TEXT, oidc_sub TEXT UNIQUE,
  admin INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS sessions(
  hash BLOB PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users ON DELETE CASCADE,
  expires INTEGER NOT NULL);
-- path : fichier pour movie/episode, dossier pour show, dossier#sN pour season (clé d'upsert).
CREATE TABLE IF NOT EXISTS items(
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL, parent_id INTEGER REFERENCES items ON DELETE CASCADE,
  title TEXT NOT NULL, year INTEGER, overview TEXT, poster TEXT, backdrop TEXT,
  season INTEGER, episode INTEGER, imdb_id TEXT, tmdb_id INTEGER, rating REAL,
  path TEXT NOT NULL UNIQUE, added INTEGER NOT NULL DEFAULT (unixepoch()),
  matched INTEGER NOT NULL DEFAULT 0, orig_lang TEXT, artist TEXT, duration REAL, mbid TEXT,
  loudness REAL, peak REAL, album_loudness REAL, album_peak REAL);
CREATE INDEX IF NOT EXISTS items_parent ON items(parent_id, season, episode);
CREATE INDEX IF NOT EXISTS items_kind ON items(kind, title);
CREATE TABLE IF NOT EXISTS prefs(
  user_id INTEGER PRIMARY KEY REFERENCES users ON DELETE CASCADE,
  audio_lang TEXT, sub_lang TEXT NOT NULL, sub_mode TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS progress(
  user_id INTEGER NOT NULL REFERENCES users ON DELETE CASCADE,
  item_id INTEGER NOT NULL REFERENCES items ON DELETE CASCADE,
  position REAL NOT NULL, watched INTEGER NOT NULL DEFAULT 0,
  updated INTEGER NOT NULL DEFAULT (unixepoch()), PRIMARY KEY(user_id, item_id));
";

/// Colonnes de `bitflow_shared::Item`, progression de l'utilisateur `?1` incluse.
pub const ITEM: &str = "SELECT i.id, i.kind, i.parent_id, i.title, i.year, i.overview, i.poster, i.backdrop,
  i.season, i.episode, i.imdb_id, i.tmdb_id, i.rating, i.artist, i.duration,
  i.loudness, i.peak, i.album_loudness, i.album_peak,
  COALESCE(p.position, 0.0) AS position, COALESCE(p.watched, 0) AS watched
  FROM items i LEFT JOIN progress p ON p.item_id = i.id AND p.user_id = ?1 ";

pub async fn open(file: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(file)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    // Colonnes ajoutées après coup sur les bases existantes ; « duplicate column » = déjà fait.
    for m in [
        "ALTER TABLE items ADD COLUMN orig_lang TEXT",
        "ALTER TABLE items ADD COLUMN artist TEXT",
        "ALTER TABLE items ADD COLUMN duration REAL",
        "ALTER TABLE items ADD COLUMN mbid TEXT",
        "ALTER TABLE items ADD COLUMN loudness REAL",
        "ALTER TABLE items ADD COLUMN peak REAL",
        "ALTER TABLE items ADD COLUMN album_loudness REAL",
        "ALTER TABLE items ADD COLUMN album_peak REAL",
    ] {
        if let Err(e) = sqlx::query(m).execute(&pool).await
            && !e.to_string().contains("duplicate column")
        {
            return Err(e.into());
        }
    }
    Ok(pool)
}
