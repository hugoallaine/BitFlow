//! Photos et vidéos personnelles : un dossier par utilisateur (`BITFLOW_PHOTOS/<nom>/…`), partage par dossier.
//! Date et position : EXIF (images) ou ffprobe (vidéos). Miniatures : ImageMagick (HEIC, RAW…) et ffmpeg (vidéos).
use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::LazyLock,
    time::UNIX_EPOCH,
};

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use bitflow_shared::{Photo, PhotoFolder, Share, UserName};
use futures_util::{StreamExt, stream};
use regex::Regex;
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use walkdir::WalkDir;

use crate::{App, R, auth::User, err};

const IMAGE: &[&str] = &["jpg", "jpeg", "png", "webp", "heic", "heif", "avif", "gif", "tif", "tiff"];
const RAW: &[&str] = &["dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf", "pef", "srw"];
const VIDEO: &[&str] = &["mp4", "mov", "m4v", "3gp"];
const THUMB: u32 = 480;
const PREVIEW: u32 = 2048;

/// Photo visible par l'utilisateur ?1 : la sienne, ou dans un dossier qu'on lui a partagé (sous-dossiers compris).
/// substr plutôt que LIKE : « _ » et « % » dans un nom de dossier ne doivent rien ouvrir de plus.
const VISIBLE: &str = "(p.owner = ?1 OR EXISTS (SELECT 1 FROM photo_shares s WHERE s.owner = p.owner AND s.target = ?1
  AND (s.folder = '' OR p.folder = s.folder OR substr(p.folder, 1, length(s.folder) + 1) = s.folder || '/')))";

pub fn routes() -> Router<App> {
    Router::new()
        .route("/photos", get(list))
        .route("/photos/folders", get(folders))
        .route("/photos/shares", put(set_share))
        .route("/photos/{id}/{size}", get(file))
}

// ---------- Analyse ----------

struct Found {
    owner: i64,
    folder: String,
    name: String,
    path: String,
    mtime: i64,
    video: bool,
}

pub async fn scan(app: &App) -> anyhow::Result<()> {
    if app.cfg.photos.is_empty() {
        return Ok(());
    }
    let users: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM users").fetch_all(&app.db).await?;
    let known: HashMap<String, i64> = sqlx::query_as("SELECT path, mtime FROM photos").fetch_all(&app.db).await?.into_iter().collect();
    let roots = app.cfg.photos.clone();
    let broken: Vec<String> = roots.iter().filter(|r| crate::scan::root_error(r).is_some()).map(|r| r.to_string_lossy().into_owned()).collect();

    let (found, metas) = tokio::task::spawn_blocking(move || {
        let found = walk(&roots, &users);
        let metas: HashMap<String, Meta> = found
            .iter()
            .filter(|f| known.get(&f.path) != Some(&f.mtime))
            .map(|f| (f.path.clone(), if f.video { video_meta(&f.path) } else { image_meta(&f.path) }))
            .collect();
        (found, metas)
    })
    .await?;

    let mut tx = app.db.begin().await?;
    let present: std::collections::HashSet<&str> = found.iter().map(|f| f.path.as_str()).collect();
    let gone: Vec<(i64, String)> = sqlx::query_as("SELECT id, path FROM photos").fetch_all(&mut *tx).await?;
    for (id, path) in gone.iter().filter(|(_, p)| !present.contains(p.as_str()) && !broken.iter().any(|b| p.starts_with(b.as_str()))) {
        sqlx::query("DELETE FROM photos WHERE id = ?").bind(id).execute(&mut *tx).await?;
        remove_cache(app, *id);
        tracing::debug!("photo retirée : {path}");
    }
    for f in found.iter() {
        let Some(m) = metas.get(&f.path) else { continue };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO photos(owner, path, folder, name, video, taken, lat, lon, duration, mtime) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET owner = excluded.owner, folder = excluded.folder, name = excluded.name, taken = excluded.taken,
             lat = excluded.lat, lon = excluded.lon, duration = excluded.duration, mtime = excluded.mtime, thumb = 0 RETURNING id",
        )
        .bind(f.owner).bind(&f.path).bind(&f.folder).bind(&f.name).bind(f.video)
        .bind(m.taken.unwrap_or(f.mtime)).bind(m.lat).bind(m.lon).bind(m.duration).bind(f.mtime)
        .fetch_one(&mut *tx)
        .await?;
        // Fichier modifié : anciennes miniatures périmées.
        remove_cache(app, id);
    }
    tx.commit().await?;
    tracing::info!("photos : {} fichiers, {} nouveaux ou modifiés", found.len(), metas.len());
    Ok(())
}

/// `<racine>/<nom d'utilisateur>/…` : les dossiers qui ne correspondent à aucun compte sont ignorés.
fn walk(roots: &[PathBuf], users: &[(i64, String)]) -> Vec<Found> {
    let mut out = vec![];
    for root in roots {
        let Ok(dirs) = std::fs::read_dir(root) else { continue };
        for dir in dirs.filter_map(Result::ok).filter(|d| d.path().is_dir()) {
            let dir_name = dir.file_name().to_string_lossy().into_owned();
            let Some((owner, _)) = users.iter().find(|(_, n)| n.eq_ignore_ascii_case(&dir_name)) else {
                tracing::warn!("photos : « {dir_name} » ne correspond à aucun utilisateur, ignoré");
                continue;
            };
            let base = dir.path();
            for e in WalkDir::new(&base).follow_links(true).into_iter().filter_map(Result::ok).filter(|e| e.file_type().is_file()) {
                let p = e.path();
                let Some(ext) = p.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase) else { continue };
                let video = VIDEO.contains(&ext.as_str());
                if !video && !IMAGE.contains(&ext.as_str()) && !RAW.contains(&ext.as_str()) {
                    continue;
                }
                // Fichiers cachés (._IMG.jpg de macOS, .thumbnails…) : ignorés.
                if p.strip_prefix(&base).is_ok_and(|r| r.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'))) {
                    continue;
                }
                let folder = p.parent().and_then(|d| d.strip_prefix(&base).ok()).map(|d| d.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                let mtime = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
                out.push(Found {
                    owner: *owner,
                    folder,
                    name: p.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                    path: p.to_string_lossy().into_owned(),
                    mtime,
                    video,
                });
            }
        }
    }
    out
}

#[derive(Default)]
struct Meta {
    taken: Option<i64>,
    lat: Option<f64>,
    lon: Option<f64>,
    duration: Option<f64>,
}

fn image_meta(path: &str) -> Meta {
    let Ok(file) = std::fs::File::open(path) else { return Meta::default() };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)) else { return Meta::default() };
    let text = |tag| exif.get_field(tag, exif::In::PRIMARY).map(|f| f.display_value().to_string());
    let coord = |tag, reference| {
        let exif::Value::Rational(v) = &exif.get_field(tag, exif::In::PRIMARY)?.value else { return None };
        let deg = v.first()?.to_f64() + v.get(1).map_or(0.0, |m| m.to_f64() / 60.0) + v.get(2).map_or(0.0, |s| s.to_f64() / 3600.0);
        let negative = text(reference).is_some_and(|r| r.contains('S') || r.contains('W'));
        Some(if negative { -deg } else { deg })
    };
    let (lat, lon) = (coord(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef), coord(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef));
    Meta {
        taken: text(exif::Tag::DateTimeOriginal).or_else(|| text(exif::Tag::DateTime)).and_then(|d| parse_date(&d)),
        // 0/0 = GPS présent mais sans position.
        lat: lat.zip(lon).filter(|(a, b)| *a != 0.0 || *b != 0.0).map(|p| p.0),
        lon: lat.zip(lon).filter(|(a, b)| *a != 0.0 || *b != 0.0).map(|p| p.1),
        duration: None,
    }
}

fn video_meta(path: &str) -> Meta {
    let Ok(out) = std::process::Command::new("ffprobe").args(["-v", "error", "-print_format", "json", "-show_format", path]).output() else {
        return Meta::default();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return Meta::default() };
    let tags = &v["format"]["tags"];
    let tag = |k: &str| tags.as_object().and_then(|t| t.iter().find(|(name, _)| name.eq_ignore_ascii_case(k))).and_then(|(_, v)| v.as_str());
    let (lat, lon) = tag("location").or(tag("com.apple.quicktime.location.ISO6709")).and_then(parse_iso6709).unzip();
    Meta {
        // Date de création Apple (heure locale) plus fiable que creation_time (UTC, parfois date d'export).
        taken: tag("com.apple.quicktime.creationdate").or(tag("creation_time")).and_then(parse_date),
        lat,
        lon,
        duration: v["format"]["duration"].as_str().and_then(|d| d.parse().ok()),
    }
}

/// « 2023:07:14 18:22:01 », « 2023-07-14T16:22:01.000000Z »… -> secondes Unix (heure de la prise, sans fuseau).
fn parse_date(s: &str) -> Option<i64> {
    static NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
    let n: Vec<i64> = NUM.find_iter(s).take(6).filter_map(|m| m.as_str().parse().ok()).collect();
    let [y, mo, d, rest @ ..] = n.as_slice() else { return None };
    if !(1900..=2200).contains(y) || !(1..=12).contains(mo) || !(1..=31).contains(d) {
        return None; // « 0000:00:00 00:00:00 » des appareils sans horloge
    }
    let hms = rest.iter().chain([0, 0, 0].iter()).take(3).fold(0, |acc, v| acc * 60 + v);
    Some(days_from_civil(*y, *mo, *d) * 86400 + hms)
}

/// Jours depuis le 1970-01-01 (algorithme de Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// ISO 6709 des vidéos : « +46.7785+006.6411+430.000/ ».
fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([+-]\d+(?:\.\d+)?)([+-]\d+(?:\.\d+)?)").unwrap());
    let c = RE.captures(s)?;
    Some((c[1].parse().ok()?, c[2].parse().ok()?))
}

// ---------- Miniatures ----------

/// ImageMagick 7 (`magick`) ou 6 (`convert`).
static MAGICK: LazyLock<&str> = LazyLock::new(|| {
    if std::process::Command::new("magick").arg("-version").output().is_ok_and(|o| o.status.success()) { "magick" } else { "convert" }
});

fn cache(app: &App, size: &str, id: i64) -> PathBuf {
    app.cfg.data.join("photos").join(size).join(format!("{id}.jpg"))
}

fn remove_cache(app: &App, id: i64) {
    for size in ["thumb", "preview"] {
        let _ = std::fs::remove_file(cache(app, size, id));
    }
}

/// JPEG de `max` px de côté au plus, orienté selon l'EXIF. Écrit à côté puis renommé : jamais de fichier à moitié écrit servi.
async fn render(path: &str, video: bool, max: u32, out: &FsPath) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(out.parent().unwrap()).await?;
    let tmp = out.with_extension("tmp.jpg");
    let ok = if video {
        let scale = format!("scale={max}:{max}:force_original_aspect_ratio=decrease");
        let mut ok = false;
        // Image à 1 s, sinon la première (vidéos très courtes).
        for at in ["1", "0"] {
            let st = tokio::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-ss", at, "-i", path, "-frames:v", "1", "-vf", &scale, "-q:v", "4"])
                .arg(&tmp)
                .status()
                .await?;
            ok = st.success() && tmp.exists();
            if ok {
                break;
            }
        }
        ok
    } else {
        // [0] : première image (HEIC multi-images, GIF animé, TIFF multipage).
        tokio::process::Command::new(*MAGICK)
            .arg(format!("{path}[0]"))
            .args(["-auto-orient", "-thumbnail", &format!("{max}x{max}>"), "-quality", "82", "-strip"])
            .arg(&tmp)
            .status()
            .await?
            .success()
    };
    anyhow::ensure!(ok, "conversion impossible");
    tokio::fs::rename(&tmp, out).await?;
    Ok(())
}

/// Miniatures manquantes, 2 à la fois (ImageMagick est gourmand), après chaque analyse.
pub async fn thumbs(app: &App) -> anyhow::Result<()> {
    let todo: Vec<(i64, String, bool)> = sqlx::query_as("SELECT id, path, video FROM photos WHERE thumb = 0").fetch_all(&app.db).await?;
    if !todo.is_empty() {
        tracing::info!("miniatures : {} photos", todo.len());
    }
    stream::iter(todo)
        .for_each_concurrent(2, |(id, path, video)| async move {
            let res = render(&path, video, THUMB, &cache(app, "thumb", id)).await;
            if let Err(e) = &res {
                tracing::warn!("miniature {path} : {e:#}");
            }
            let _ = sqlx::query("UPDATE photos SET thumb = ? WHERE id = ?").bind(if res.is_ok() { 1 } else { 2 }).bind(id).execute(&app.db).await;
        })
        .await;
    Ok(())
}

// ---------- API ----------

#[derive(Deserialize)]
struct ListQuery {
    owner: Option<i64>,
    folder: Option<String>,
    #[serde(default)]
    offset: i64,
    limit: Option<i64>,
}

async fn list(State(app): State<App>, u: User, Query(q): Query<ListQuery>) -> R<Json<Vec<Photo>>> {
    let sql = format!(
        "SELECT p.id, p.owner, p.folder, p.name, p.video, p.taken, p.lat, p.lon, p.duration FROM photos p
         WHERE {VISIBLE} AND (?2 IS NULL OR p.owner = ?2) AND (?3 IS NULL OR p.folder = ?3)
         ORDER BY p.taken DESC, p.id DESC LIMIT ?4 OFFSET ?5"
    );
    let photos = sqlx::query_as(&sql)
        .bind(u.id).bind(q.owner).bind(q.folder).bind(q.limit.unwrap_or(300).clamp(1, 5000)).bind(q.offset.max(0))
        .fetch_all(&app.db)
        .await?;
    Ok(Json(photos))
}

async fn folders(State(app): State<App>, u: User) -> R<Json<Vec<PhotoFolder>>> {
    let sql = format!(
        "SELECT p.owner, u.name, p.folder, COUNT(*), MAX(p.id) FROM photos p JOIN users u ON u.id = p.owner
         WHERE {VISIBLE} GROUP BY p.owner, p.folder ORDER BY p.owner <> ?1, u.name, p.folder"
    );
    let rows: Vec<(i64, String, String, i64, i64)> = sqlx::query_as(&sql).bind(u.id).fetch_all(&app.db).await?;
    let shares: Vec<(String, i64)> = sqlx::query_as("SELECT folder, target FROM photo_shares WHERE owner = ?").bind(u.id).fetch_all(&app.db).await?;
    let out = rows
        .into_iter()
        .map(|(owner, owner_name, folder, count, cover)| PhotoFolder {
            shared_with: if owner == u.id { shares.iter().filter(|(f, _)| *f == folder).map(|(_, t)| *t).collect() } else { vec![] },
            owner,
            owner_name,
            folder,
            count,
            cover,
        })
        .collect();
    Ok(Json(out))
}

/// Remplace la liste des utilisateurs avec qui un de ses dossiers est partagé.
async fn set_share(State(app): State<App>, u: User, Json(s): Json<Share>) -> R<StatusCode> {
    let mut tx = app.db.begin().await?;
    sqlx::query("DELETE FROM photo_shares WHERE owner = ? AND folder = ?").bind(u.id).bind(&s.folder).execute(&mut *tx).await?;
    for target in s.users.iter().filter(|t| **t != u.id) {
        // Utilisateur inexistant : refusé par la clé étrangère.
        sqlx::query("INSERT OR IGNORE INTO photo_shares(owner, folder, target) VALUES (?, ?, ?)")
            .bind(u.id).bind(&s.folder).bind(target).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Noms des comptes, pour choisir avec qui partager.
pub async fn users(State(app): State<App>, _u: User) -> R<Json<Vec<UserName>>> {
    Ok(Json(sqlx::query_as("SELECT id, name FROM users ORDER BY name").fetch_all(&app.db).await?))
}

/// `thumb` (480 px), `preview` (2048 px, images seulement) ou `original` (fichier tel quel, Range pour les vidéos).
async fn file(State(app): State<App>, u: User, Path((id, size)): Path<(i64, String)>, req: Request) -> R<Response> {
    let sql = format!("SELECT p.path, p.video FROM photos p WHERE p.id = ?2 AND {VISIBLE}");
    let Some((path, video)): Option<(String, bool)> = sqlx::query_as(&sql).bind(u.id).bind(id).fetch_optional(&app.db).await? else {
        return err(StatusCode::NOT_FOUND, "photo introuvable");
    };
    let (out, max) = match size.as_str() {
        "original" => return Ok(ServeFile::new(path).oneshot(req).await?.into_response()),
        "thumb" => (cache(&app, "thumb", id), THUMB),
        "preview" if !video => (cache(&app, "preview", id), PREVIEW),
        _ => return err(StatusCode::NOT_FOUND, "taille inconnue"),
    };
    if !out.exists() {
        let _slot = app.photo_slot.acquire().await?;
        if !out.exists() && let Err(e) = render(&path, video, max, &out).await {
            tracing::warn!("{size} {path} : {e:#}");
            return err(StatusCode::UNPROCESSABLE_ENTITY, "format non pris en charge");
        }
    }
    let mut res = ServeFile::new(out).oneshot(req).await?.into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("private, max-age=86400"));
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(parse_date("1970:01:02 00:00:10"), Some(86410));
        assert_eq!(parse_date("2023:07:14 18:22:01"), Some(1689358921));
        assert_eq!(parse_date("2023-07-14T18:22:01.000000Z"), Some(1689358921));
        assert_eq!(parse_date("2024-02-29"), Some(1709164800));
        assert_eq!(parse_date("0000:00:00 00:00:00"), None);
    }

    #[test]
    fn iso6709() {
        assert_eq!(parse_iso6709("+46.7785+006.6411+430.000/"), Some((46.7785, 6.6411)));
        assert_eq!(parse_iso6709("-33.8688+151.2093/"), Some((-33.8688, 151.2093)));
        assert_eq!(parse_iso6709("n/a"), None);
    }
}
