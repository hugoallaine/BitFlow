//! Playlists personnelles : chaque utilisateur ne voit et ne modifie que les siennes.
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use bitflow_shared::{Playlist, PlaylistEdit, PlaylistTracks};
use sqlx::SqliteConnection;

use crate::{App, R, auth::User, db::ITEM, err};

pub fn routes() -> Router<App> {
    Router::new()
        .route("/playlists", get(list).post(create))
        .route("/playlists/{id}", get(tracks).put(replace).delete(remove))
        .route("/playlists/{id}/tracks", post(append))
}

const NAME_MAX: usize = 200;

async fn list(State(app): State<App>, u: User) -> R<Json<Vec<Playlist>>> {
    Ok(Json(
        sqlx::query_as(
            "SELECT p.id, p.name, COUNT(i.id) AS count, COALESCE(SUM(i.duration), 0.0) AS duration,
               (SELECT i2.poster FROM playlist_tracks t2 JOIN items i2 ON i2.id = t2.item_id
                WHERE t2.playlist_id = p.id AND i2.poster IS NOT NULL ORDER BY t2.pos LIMIT 1) AS cover
             FROM playlists p LEFT JOIN playlist_tracks t ON t.playlist_id = p.id LEFT JOIN items i ON i.id = t.item_id
             WHERE p.owner = ? GROUP BY p.id ORDER BY p.name COLLATE NOCASE",
        )
        .bind(u.id)
        .fetch_all(&app.db)
        .await?,
    ))
}

fn clean(name: &str) -> R<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return err(StatusCode::BAD_REQUEST, "nom de playlist vide ou trop long");
    }
    Ok(name.into())
}

/// La playlist appartient-elle à `u` ?
async fn owned(db: &mut SqliteConnection, u: &User, id: i64) -> R<()> {
    match sqlx::query_scalar::<_, i64>("SELECT id FROM playlists WHERE id = ? AND owner = ?").bind(id).bind(u.id).fetch_optional(db).await? {
        Some(_) => Ok(()),
        None => err(StatusCode::NOT_FOUND, "playlist introuvable"),
    }
}

/// Ajoute les pistes à la suite ; les identifiants qui ne sont pas des pistes sont ignorés.
async fn push(db: &mut SqliteConnection, id: i64, items: &[i64]) -> R<()> {
    let mut pos: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(pos) + 1, 0) FROM playlist_tracks WHERE playlist_id = ?").bind(id).fetch_one(&mut *db).await?;
    for item in items {
        let r = sqlx::query("INSERT INTO playlist_tracks(playlist_id, pos, item_id) SELECT ?, ?, id FROM items WHERE id = ? AND kind = 'track'")
            .bind(id).bind(pos).bind(item).execute(&mut *db).await?;
        pos += r.rows_affected() as i64;
    }
    Ok(())
}

async fn create(State(app): State<App>, u: User, Json(p): Json<PlaylistEdit>) -> R<Json<i64>> {
    let name = clean(&p.name)?;
    let mut tx = app.db.begin().await?;
    let id: i64 = sqlx::query_scalar("INSERT INTO playlists(owner, name) VALUES (?, ?) RETURNING id").bind(u.id).bind(name).fetch_one(&mut *tx).await?;
    push(&mut tx, id, &p.items).await?;
    tx.commit().await?;
    Ok(Json(id))
}

async fn tracks(State(app): State<App>, u: User, Path(id): Path<i64>) -> R<Json<PlaylistTracks>> {
    let Some(name) = sqlx::query_scalar("SELECT name FROM playlists WHERE id = ? AND owner = ?").bind(id).bind(u.id).fetch_optional(&app.db).await? else {
        return err(StatusCode::NOT_FOUND, "playlist introuvable");
    };
    let sql = format!("{ITEM} JOIN playlist_tracks t ON t.item_id = i.id WHERE t.playlist_id = ?2 ORDER BY t.pos");
    let tracks = sqlx::query_as(&sql).bind(u.id).bind(id).fetch_all(&app.db).await?;
    Ok(Json(PlaylistTracks { name, tracks }))
}

/// Remplace le nom et toute la liste : couvre renommage, retrait et réordonnancement.
async fn replace(State(app): State<App>, u: User, Path(id): Path<i64>, Json(p): Json<PlaylistEdit>) -> R<StatusCode> {
    let name = clean(&p.name)?;
    let mut tx = app.db.begin().await?;
    owned(&mut tx, &u, id).await?;
    sqlx::query("UPDATE playlists SET name = ? WHERE id = ?").bind(name).bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM playlist_tracks WHERE playlist_id = ?").bind(id).execute(&mut *tx).await?;
    push(&mut tx, id, &p.items).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn append(State(app): State<App>, u: User, Path(id): Path<i64>, Json(items): Json<Vec<i64>>) -> R<StatusCode> {
    let mut tx = app.db.begin().await?;
    owned(&mut tx, &u, id).await?;
    push(&mut tx, id, &items).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove(State(app): State<App>, u: User, Path(id): Path<i64>) -> R<StatusCode> {
    let r = sqlx::query("DELETE FROM playlists WHERE id = ? AND owner = ?").bind(id).bind(u.id).execute(&app.db).await?;
    Ok(if r.rows_affected() == 0 { StatusCode::NOT_FOUND } else { StatusCode::NO_CONTENT })
}
