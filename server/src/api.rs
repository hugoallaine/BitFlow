use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use bitflow_shared::{AuthConfig, Credentials, Item, Library, Me, Prefs, Progress, ScanStatus};
use serde::Deserialize;

use crate::{App, R, auth, auth::User, db::ITEM, err, scan, stream, tmdb};

pub fn routes() -> Router<App> {
    Router::new()
        .route("/auth/config", get(auth_config))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/oidc", get(auth::oidc_start))
        .route("/auth/oidc/callback", get(auth::oidc_callback))
        .route("/me", get(me))
        .route("/prefs", get(get_prefs).put(put_prefs))
        .route("/users", get(crate::photos::users).post(create_user))
        .route("/scan", get(scan_status).post(rescan))
        .merge(crate::photos::routes())
        .merge(crate::playlists::routes())
        .route("/items", get(list))
        .route("/resume", get(resume))
        .route("/search", get(search))
        .route("/items/{id}", get(item))
        .route("/items/{id}/children", get(children))
        .route("/items/{id}/progress", post(progress))
        .route("/play/{id}/info", get(stream::info))
        .route("/play/{id}/direct", get(stream::direct))
        .route("/play/{id}/audio", get(stream::audio))
        .route("/play/{id}/hls", post(stream::hls_start))
        .route("/play/{id}/sub/{n}", get(stream::subtitle))
        .route("/download/{id}/{quality}", get(stream::download))
        .route("/hls", axum::routing::delete(stream::hls_stop))
        .route("/hls/{sid}/{file}", get(stream::hls_file))
        .route("/img/{kind}/{file}", get(tmdb::image))
}

async fn has_users(app: &App) -> R<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users)").fetch_one(&app.db).await?)
}

async fn auth_config(State(app): State<App>) -> R<Json<AuthConfig>> {
    Ok(Json(AuthConfig { oidc: app.cfg.oidc.is_some(), setup: !has_users(&app).await? }))
}

/// Renvoie aussi le jeton : les applications mobiles l'utilisent en `Authorization: Bearer`.
async fn login(State(app): State<App>, Json(c): Json<Credentials>) -> R<impl IntoResponse> {
    let id = auth::login(&app, &c.name, &c.password).await?;
    let token = auth::new_session(&app, id).await?;
    Ok(([auth::set_cookie(&app, &token, auth::session_max_age())], Json(token)))
}

async fn me(State(app): State<App>, u: User) -> R<Json<Me>> {
    let name = sqlx::query_scalar("SELECT name FROM users WHERE id = ?").bind(u.id).fetch_one(&app.db).await?;
    Ok(Json(Me { id: u.id, name, admin: u.admin }))
}

/// Préférences enregistrées, sinon valeurs par défaut (sous-titres dans la langue du serveur si l'audio diffère).
pub async fn prefs_of(app: &App, user_id: i64) -> R<Prefs> {
    let saved = sqlx::query_as("SELECT audio_lang, sub_lang, sub_mode FROM prefs WHERE user_id = ?").bind(user_id).fetch_optional(&app.db).await?;
    Ok(saved.unwrap_or_else(|| Prefs { audio_lang: None, sub_lang: app.cfg.lang.chars().take(2).collect(), sub_mode: "foreign".into() }))
}

async fn get_prefs(State(app): State<App>, u: User) -> R<Json<Prefs>> {
    Ok(Json(prefs_of(&app, u.id).await?))
}

async fn put_prefs(State(app): State<App>, u: User, Json(p): Json<Prefs>) -> R<StatusCode> {
    let lang_ok = |l: &str| (2..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase());
    if !matches!(p.sub_mode.as_str(), "off" | "foreign" | "always") || !lang_ok(&p.sub_lang) || p.audio_lang.as_deref().is_some_and(|l| !lang_ok(l)) {
        return err(StatusCode::BAD_REQUEST, "préférences invalides");
    }
    sqlx::query(
        "INSERT INTO prefs(user_id, audio_lang, sub_lang, sub_mode) VALUES (?, ?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET audio_lang = excluded.audio_lang, sub_lang = excluded.sub_lang, sub_mode = excluded.sub_mode",
    )
    .bind(u.id)
    .bind(&p.audio_lang)
    .bind(&p.sub_lang)
    .bind(&p.sub_mode)
    .execute(&app.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Admin uniquement, sauf pour le tout premier compte qui devient administrateur.
async fn create_user(State(app): State<App>, user: Option<User>, Json(c): Json<Credentials>) -> R<StatusCode> {
    let first = !has_users(&app).await?;
    if !first && !user.is_some_and(|u| u.admin) {
        return err(StatusCode::FORBIDDEN, "réservé aux administrateurs");
    }
    let name = c.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return err(StatusCode::BAD_REQUEST, "Nom d'utilisateur invalide.");
    }
    auth::check_policy(&app, &c.password).await?;
    let pw = c.password.clone();
    let hash = tokio::task::spawn_blocking(move || auth::hash(&pw)).await?;
    // `first` revérifié dans la requête : deux créations simultanées ne font pas deux admins.
    let res = sqlx::query("INSERT INTO users(name, pw, admin) VALUES (?, ?, NOT EXISTS(SELECT 1 FROM users))")
        .bind(name)
        .bind(hash)
        .execute(&app.db)
        .await;
    match res {
        Ok(_) => Ok(StatusCode::CREATED),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => err(StatusCode::CONFLICT, "Ce nom est déjà utilisé."),
        Err(e) => Err(e.into()),
    }
}

/// Dossiers configurés, fichiers indexés et erreurs d'accès.
async fn scan_status(State(app): State<App>, _u: User) -> R<Json<ScanStatus>> {
    let mut libraries = vec![];
    for (kind, var, roots, item) in [
        ("Films", "BITFLOW_MOVIES", &app.cfg.movies, "movie"),
        ("Séries", "BITFLOW_SHOWS", &app.cfg.shows, "episode"),
        ("Musiques", "BITFLOW_MUSIC", &app.cfg.music, "track"),
        ("Photos", "BITFLOW_PHOTOS", &app.cfg.photos, "photo"),
    ] {
        if roots.is_empty() {
            libraries.push(Library { kind: kind.into(), path: String::new(), files: 0, error: Some(format!("non configuré ({var})")) });
        }
        for root in roots {
            let path = root.to_string_lossy().into_owned();
            let sql = if item == "photo" {
                "SELECT COUNT(*) FROM photos WHERE ?1 = 'photo' AND substr(path, 1, length(?2)) = ?2"
            } else {
                "SELECT COUNT(*) FROM items WHERE kind = ?1 AND substr(path, 1, length(?2)) = ?2"
            };
            let files = sqlx::query_scalar(sql)
                .bind(item).bind(&path).fetch_one(&app.db).await?;
            libraries.push(Library { kind: kind.into(), error: scan::root_error(root), path, files });
        }
    }
    Ok(Json(ScanStatus { running: scan::running(), libraries }))
}

async fn rescan(State(app): State<App>, u: User) -> R<StatusCode> {
    if !u.admin {
        return err(StatusCode::FORBIDDEN, "réservé aux administrateurs");
    }
    tokio::spawn(async move { scan::run(&app).await });
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct ListQuery {
    kind: String,
    /// "added" (récents d'abord) ou titre par défaut.
    sort: Option<String>,
    limit: Option<i64>,
}

async fn list(State(app): State<App>, u: User, Query(q): Query<ListQuery>) -> R<Json<Vec<Item>>> {
    if !matches!(q.kind.as_str(), "movie" | "show" | "album" | "artist") {
        return err(StatusCode::BAD_REQUEST, "kind invalide");
    }
    let order = if q.sort.as_deref() == Some("added") { "i.added DESC" } else { "i.title COLLATE NOCASE" };
    let sql = format!("{ITEM} WHERE i.kind = ?2 ORDER BY {order} LIMIT ?3");
    Ok(Json(sqlx::query_as(&sql).bind(u.id).bind(&q.kind).bind(q.limit.unwrap_or(-1)).fetch_all(&app.db).await?))
}

async fn resume(State(app): State<App>, u: User) -> R<Json<Vec<Item>>> {
    let sql = format!("{ITEM} WHERE p.position > 0 AND p.watched = 0 ORDER BY p.updated DESC LIMIT 20");
    Ok(Json(sqlx::query_as(&sql).bind(u.id).fetch_all(&app.db).await?))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

// shortcut: LIKE parcourt toute la table, passer à FTS5 au-delà de ~100k éléments.
async fn search(State(app): State<App>, u: User, Query(q): Query<SearchQuery>) -> R<Json<Vec<Item>>> {
    let pattern = format!("%{}%", q.q.replace(['%', '_'], ""));
    let sql = format!("{ITEM} WHERE i.kind IN ('movie', 'show', 'episode', 'artist', 'album', 'track') AND i.title LIKE ?2 ORDER BY i.kind != 'movie', i.title LIMIT 80");
    Ok(Json(sqlx::query_as(&sql).bind(u.id).bind(pattern).fetch_all(&app.db).await?))
}

async fn item(State(app): State<App>, u: User, Path(id): Path<i64>) -> R<Json<Item>> {
    let sql = format!("{ITEM} WHERE i.id = ?2");
    match sqlx::query_as(&sql).bind(u.id).bind(id).fetch_optional(&app.db).await? {
        Some(i) => Ok(Json(i)),
        None => err(StatusCode::NOT_FOUND, "introuvable"),
    }
}

async fn children(State(app): State<App>, u: User, Path(id): Path<i64>) -> R<Json<Vec<Item>>> {
    // Saisons/épisodes/pistes : numéros ; albums d'un artiste : année.
    let sql = format!("{ITEM} WHERE i.parent_id = ?2 ORDER BY i.season, i.episode, i.year, i.title");
    Ok(Json(sqlx::query_as(&sql).bind(u.id).bind(id).fetch_all(&app.db).await?))
}

/// Pour une série ou une saison, « vu / non vu » s'applique à tous ses épisodes.
async fn progress(State(app): State<App>, u: User, Path(id): Path<i64>, Json(p): Json<Progress>) -> R<StatusCode> {
    sqlx::query(
        "WITH RECURSIVE t(id) AS (SELECT ?2 UNION ALL SELECT i.id FROM items i JOIN t ON i.parent_id = t.id)
         INSERT INTO progress(user_id, item_id, position, watched)
         SELECT ?1, t.id, ?3, ?4 FROM t JOIN items i ON i.id = t.id WHERE i.kind IN ('movie', 'episode')
         ON CONFLICT(user_id, item_id) DO UPDATE SET position = excluded.position, watched = excluded.watched, updated = unixepoch()",
    )
    .bind(u.id)
    .bind(id)
    .bind(if p.watched { 0.0 } else { p.position.max(0.0) })
    .bind(p.watched)
    .execute(&app.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
