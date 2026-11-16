//! Métadonnées TMDB (films, séries, saisons, épisodes). L'identifiant IMDb est récupéré via TMDB.
use axum::{
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, stream};
use serde_json::Value;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{App, R, auth::User, err};

fn valid_image(file: &str) -> bool {
    let Some((stem, ext)) = file.rsplit_once('.') else { return false };
    !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') && matches!(ext, "jpg" | "png" | "webp")
}

/// Image TMDB téléchargée une seule fois par le serveur, puis servie depuis `/data/img`.
/// Les navigateurs ne contactent jamais TMDB. Une seule taille stockée par type (`poster` ou `backdrop`),
/// fixée par BITFLOW_IMG_POSTER / BITFLOW_IMG_BACKDROP ; changer la taille crée un nouveau dossier.
pub async fn image(State(app): State<App>, _: User, Path((kind, file)): Path<(String, String)>, req: Request) -> R<Response> {
    if !valid_image(&file) {
        return err(StatusCode::NOT_FOUND, "image inconnue");
    }
    let size = match kind.as_str() {
        "poster" => &app.cfg.img_poster,
        "backdrop" => &app.cfg.img_backdrop,
        // Pochettes d'albums : écrites par le scan, jamais téléchargées ici.
        "cover" => return Ok(ServeFile::new(app.cfg.data.join("img").join("cover").join(&file)).oneshot(req).await?.into_response()),
        _ => return err(StatusCode::NOT_FOUND, "image inconnue"),
    };
    let path = app.cfg.data.join("img").join(format!("{kind}-{size}")).join(&file);
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        let bytes = app.http.get(format!("https://image.tmdb.org/t/p/{size}/{file}")).send().await?.error_for_status()?.bytes().await?;
        tokio::fs::create_dir_all(path.parent().unwrap_or(&path)).await?;
        // Écriture puis renommage : jamais de fichier partiel servi en cas de requêtes simultanées.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
    }
    let mut res = ServeFile::new(path).oneshot(req).await?.into_response();
    // Les noms TMDB changent quand l'image change : cache navigateur permanent.
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    Ok(res)
}

#[cfg(test)]
mod tests {
    #[test]
    fn image_names() {
        assert!(super::valid_image("kqjL17yufvn9OVLyXYpvtyrFfak.jpg"));
        assert!(!super::valid_image("../bitflow.db"));
        assert!(!super::valid_image(".jpg"));
        assert!(!super::valid_image("a.svg"));
    }
}

/// matched : 0 = à traiter, 1 = trouvé, 2 = introuvable (on ne réessaie pas).
pub async fn enrich(app: &App) -> anyhow::Result<()> {
    if app.cfg.tmdb_token.is_none() {
        tracing::warn!("TMDB_TOKEN absent : pas de métadonnées");
        return Ok(());
    }
    // matched = 1 sans langue originale : éléments analysés avant l'ajout de la colonne, à compléter.
    let todo: Vec<(i64, String, String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT id, kind, title, year, tmdb_id FROM items
         WHERE kind IN ('movie', 'show') AND (matched = 0 OR (matched = 1 AND orig_lang IS NULL))",
    )
    .fetch_all(&app.db)
    .await?;
    // TMDB tolère ~50 req/s : 8 en parallèle reste très en dessous.
    stream::iter(todo)
        .for_each_concurrent(8, |(id, kind, title, year, tmdb_id)| async move {
            if let Err(e) = match_one(app, id, &kind, &title, year, tmdb_id).await {
                tracing::warn!("TMDB « {title} » : {e:#}");
            }
        })
        .await;

    let seasons: Vec<(i64, i64, i64)> = sqlx::query_as(
        "SELECT s.id, sh.tmdb_id, s.season FROM items s JOIN items sh ON sh.id = s.parent_id
         WHERE s.kind = 'season' AND sh.tmdb_id IS NOT NULL AND (s.matched = 0 OR EXISTS
           (SELECT 1 FROM items e WHERE e.parent_id = s.id AND e.matched = 0))",
    )
    .fetch_all(&app.db)
    .await?;
    stream::iter(seasons)
        .for_each_concurrent(8, |(id, tv, n)| async move {
            if let Err(e) = season(app, id, tv, n).await {
                tracing::warn!("TMDB saison {tv}/{n} : {e:#}");
            }
        })
        .await;
    tracing::info!("métadonnées à jour");
    Ok(())
}

async fn get(app: &App, path: &str, query: &[(&str, String)]) -> anyhow::Result<Value> {
    let token = app.cfg.tmdb_token.as_deref().unwrap_or_default();
    Ok(app
        .http
        .get(format!("https://api.themoviedb.org/3{path}"))
        .bearer_auth(token)
        .query(&[("language", app.cfg.lang.as_str())])
        .query(query)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

fn s(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn year(v: &Value, k: &str) -> Option<i64> {
    v[k].as_str()?.get(..4)?.parse().ok()
}

async fn match_one(app: &App, id: i64, kind: &str, title: &str, y: Option<i64>, known: Option<i64>) -> anyhow::Result<()> {
    let (search, year_key, details) = if kind == "movie" {
        ("/search/movie", "year", "/movie")
    } else {
        ("/search/tv", "first_air_date_year", "/tv")
    };
    let mut q = vec![("query", title.to_owned())];
    if let Some(y) = y {
        q.push((year_key, y.to_string()));
    }
    let tmdb_id = match known {
        Some(id) => Some(id),
        None => get(app, search, &q).await?["results"][0]["id"].as_i64(),
    };
    let Some(tmdb_id) = tmdb_id else {
        sqlx::query("UPDATE items SET matched = 2 WHERE id = ?").bind(id).execute(&app.db).await?;
        return Ok(());
    };
    let d = get(app, &format!("{details}/{tmdb_id}"), &[("append_to_response", "external_ids".into())]).await?;
    sqlx::query(
        "UPDATE items SET title = ?, year = COALESCE(?, year), overview = ?, poster = ?, backdrop = ?,
         rating = ?, tmdb_id = ?, imdb_id = ?, orig_lang = ?, matched = 1 WHERE id = ?",
    )
    .bind(s(&d, "title").or(s(&d, "name")).unwrap_or(title.to_owned()))
    .bind(year(&d, "release_date").or(year(&d, "first_air_date")))
    .bind(s(&d, "overview"))
    .bind(s(&d, "poster_path"))
    .bind(s(&d, "backdrop_path"))
    .bind(d["vote_average"].as_f64())
    .bind(tmdb_id)
    .bind(s(&d, "imdb_id").or(s(&d["external_ids"], "imdb_id")))
    .bind(s(&d, "original_language").unwrap_or_default()) // "" = inconnu, évite de redemander à chaque scan
    .bind(id)
    .execute(&app.db)
    .await?;
    Ok(())
}

async fn season(app: &App, id: i64, tv: i64, n: i64) -> anyhow::Result<()> {
    let d = get(app, &format!("/tv/{tv}/season/{n}"), &[]).await?;
    let mut tx = app.db.begin().await?;
    sqlx::query("UPDATE items SET overview = ?, poster = ?, year = ?, matched = 1 WHERE id = ?")
        .bind(s(&d, "overview")).bind(s(&d, "poster_path")).bind(year(&d, "air_date")).bind(id)
        .execute(&mut *tx).await?;
    for ep in d["episodes"].as_array().into_iter().flatten() {
        sqlx::query(
            "UPDATE items SET title = COALESCE(?, title), overview = ?, backdrop = ?, rating = ?, year = ?, matched = 1
             WHERE parent_id = ? AND episode = ?",
        )
        .bind(s(ep, "name")).bind(s(ep, "overview")).bind(s(ep, "still_path"))
        .bind(ep["vote_average"].as_f64()).bind(year(ep, "air_date"))
        .bind(id).bind(ep["episode_number"].as_i64())
        .execute(&mut *tx).await?;
    }
    // Épisodes absents de TMDB : ne pas réinterroger à chaque scan.
    sqlx::query("UPDATE items SET matched = 2 WHERE parent_id = ? AND matched = 0").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
