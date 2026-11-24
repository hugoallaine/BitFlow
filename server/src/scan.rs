//! Analyse des bibliothèques : films (`Titre (2020).mkv`), séries (`Série/Saison 1/Série S01E02.mkv`)
//! et musique (tags ID3/FLAC/MP4/Vorbis lus par lofty).
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{LazyLock, atomic::{AtomicBool, Ordering}},
};

use lofty::{picture::MimeType, prelude::*, tag::Tag};
use regex::Regex;
use walkdir::WalkDir;

use crate::App;

const VIDEO: &[&str] = &["mkv", "mp4", "m4v", "avi", "mov", "webm", "ts", "m2ts", "wmv", "mpg"];
const AUDIO: &[&str] = &["mp3", "flac", "m4a", "aac", "ogg", "oga", "opus", "wav", "wma", "ape", "aiff", "aif", "wv", "alac"];
const FOLDER_ART: &[&str] = &["cover.jpg", "folder.jpg", "front.jpg", "cover.png", "folder.png", "front.png"];
pub const VARIOUS: &str = "Artistes divers";
static RUNNING: AtomicBool = AtomicBool::new(false);

pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Dossier absent ou illisible, avec la cause probable en Docker.
pub fn root_error(root: &Path) -> Option<String> {
    let e = std::fs::read_dir(root).err()?;
    Some(match e.kind() {
        std::io::ErrorKind::NotFound => "dossier introuvable : volume Docker monté ?".into(),
        std::io::ErrorKind::PermissionDenied => "accès refusé : droits de lecture pour l'utilisateur 1000 ?".into(),
        _ => e.to_string(),
    })
}

/// Scan incrémental puis récupération des métadonnées. Un seul scan à la fois.
pub async fn run(app: &App) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Err(e) = scan(app).await {
        tracing::error!("scan : {e:#}");
    }
    if let Err(e) = crate::photos::scan(app).await {
        tracing::error!("photos : {e:#}");
    }
    if let Err(e) = crate::tmdb::enrich(app).await {
        tracing::error!("TMDB : {e:#}");
    }
    if let Err(e) = crate::musicbrainz::enrich(app).await {
        tracing::error!("MusicBrainz : {e:#}");
    }
    if let Err(e) = crate::loudness::analyze(app).await {
        tracing::error!("analyse du volume : {e:#}");
    }
    if let Err(e) = crate::photos::thumbs(app).await {
        tracing::error!("miniatures : {e:#}");
    }
    if let Err(e) = crate::faces::run(app).await {
        tracing::error!("visages : {e:#}");
    }
    RUNNING.store(false, Ordering::SeqCst);
}

async fn scan(app: &App) -> anyhow::Result<()> {
    let known: Vec<String> = sqlx::query_scalar("SELECT path FROM items WHERE kind IN ('movie', 'episode', 'track')").fetch_all(&app.db).await?;
    let (movies, shows, music) = (app.cfg.movies.clone(), app.cfg.shows.clone(), app.cfg.music.clone());
    // Dossier non monté ou illisible : on garde ses éléments au lieu de tout effacer (progression, métadonnées).
    let broken: Vec<String> = movies.iter().chain(&shows).chain(&music)
        .filter(|r| root_error(r).inspect(|e| tracing::warn!("bibliothèque {} : {e}", r.display())).is_some())
        .map(|r| r.to_string_lossy().into_owned())
        .collect();
    let known_set: HashSet<String> = known.iter().cloned().collect();
    // Parcours et lecture des tags (E/S disque bloquantes) hors du runtime async.
    let (movies, shows, music_files, tracks) = tokio::task::spawn_blocking(move || {
        let music = walk(&music, AUDIO);
        let new: Vec<PathBuf> = music.iter().map(|(_, p)| p.clone()).filter(|p| !known_set.contains(p.to_str().unwrap_or_default())).collect();
        // Dossier de séries inclus dans celui des films (BITFLOW_MOVIES=/media, BITFLOW_SHOWS=/media/series) : épisodes, pas films.
        let shows = walk(&shows, VIDEO);
        let in_shows: HashSet<&PathBuf> = shows.iter().map(|(_, p)| p).collect();
        let movies: Vec<_> = walk(&movies, VIDEO).into_iter().filter(|(_, p)| !in_shows.contains(p)).collect();
        (movies, shows, music, read_tracks(new))
    })
    .await?;

    // Fichiers disparus : suppression, puis des conteneurs devenus vides.
    let present: HashSet<&str> = movies.iter().chain(&shows).chain(&music_files).filter_map(|(_, p)| p.to_str()).collect();
    let mut tx = app.db.begin().await?;
    for p in known.iter().filter(|p| !present.contains(p.as_str()) && !broken.iter().any(|b| p.starts_with(b.as_str()))) {
        sqlx::query("DELETE FROM items WHERE path = ?").bind(p).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM items WHERE kind = 'season' AND id NOT IN (SELECT parent_id FROM items WHERE kind = 'episode')").execute(&mut *tx).await?;
    sqlx::query("DELETE FROM items WHERE kind = 'show' AND id NOT IN (SELECT parent_id FROM items WHERE kind = 'season')").execute(&mut *tx).await?;
    sqlx::query("DELETE FROM items WHERE kind = 'album' AND id NOT IN (SELECT parent_id FROM items WHERE kind = 'track')").execute(&mut *tx).await?;
    sqlx::query("DELETE FROM items WHERE kind = 'artist' AND id NOT IN (SELECT parent_id FROM items WHERE kind = 'album')").execute(&mut *tx).await?;

    let known: HashSet<String> = known.into_iter().collect();
    let mut added = 0;
    for (_, file) in movies.iter().filter(|(_, p)| !known.contains(p.to_str().unwrap_or_default())) {
        let (title, year) = movie_name(file);
        sqlx::query("INSERT INTO items(kind, title, year, path) VALUES ('movie', ?, ?, ?) ON CONFLICT(path) DO NOTHING")
            .bind(title).bind(year).bind(file.to_str()).execute(&mut *tx).await?;
        added += 1;
    }
    for (root, file) in shows.iter().filter(|(_, p)| !known.contains(p.to_str().unwrap_or_default())) {
        let Some((s, e)) = episode_num(&file.to_string_lossy()) else { continue };
        let Some(dir) = file.strip_prefix(root).ok().and_then(|r| r.components().next()) else { continue };
        let show_dir = root.join(dir);
        let (title, year) = clean_title(&dir.as_os_str().to_string_lossy());
        let show: i64 = sqlx::query_scalar(
            "INSERT INTO items(kind, title, year, path) VALUES ('show', ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET kind = kind RETURNING id",
        ).bind(title).bind(year).bind(show_dir.to_str()).fetch_one(&mut *tx).await?;
        let season: i64 = sqlx::query_scalar(
            "INSERT INTO items(kind, parent_id, title, season, path) VALUES ('season', ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET kind = kind RETURNING id",
        ).bind(show).bind(format!("Saison {s}")).bind(s).bind(format!("{}#s{s}", show_dir.display())).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO items(kind, parent_id, title, season, episode, path) VALUES ('episode', ?, ?, ?, ?, ?) ON CONFLICT(path) DO NOTHING")
            .bind(season).bind(format!("Épisode {e}")).bind(s).bind(e).bind(file.to_str()).execute(&mut *tx).await?;
        added += 1;
    }
    for t in tracks {
        let artist_key = t.album_artist.to_lowercase();
        let artist: i64 = sqlx::query_scalar(
            "INSERT INTO items(kind, title, mbid, path) VALUES ('artist', ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET mbid = COALESCE(items.mbid, excluded.mbid) RETURNING id",
        ).bind(&t.album_artist).bind(&t.artist_mbid).bind(format!("music-artist:{artist_key}")).fetch_one(&mut *tx).await?;
        let (album, cover): (i64, Option<String>) = sqlx::query_as(
            "INSERT INTO items(kind, parent_id, title, artist, year, mbid, path) VALUES ('album', ?, ?, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET year = COALESCE(items.year, excluded.year), mbid = COALESCE(items.mbid, excluded.mbid)
             RETURNING id, poster",
        ).bind(artist).bind(&t.album).bind(&t.album_artist).bind(t.year).bind(&t.mbid)
            .bind(format!("music-album:{artist_key}/{}", t.album.to_lowercase())).fetch_one(&mut *tx).await?;
        if let (None, Some((bytes, ext))) = (cover, &t.cover) {
            let name = crate::musicbrainz::save_image(app, format!("c{album}.{ext}"), bytes).await?;
            sqlx::query("UPDATE items SET poster = ? WHERE id = ?").bind(name).bind(album).execute(&mut *tx).await?;
        }
        // ReplayGain présent : pas besoin de mesurer (matched = 1).
        sqlx::query(
            "INSERT INTO items(kind, parent_id, title, artist, season, episode, duration, loudness, peak, album_loudness, album_peak, matched, path)
             VALUES ('track', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(path) DO NOTHING",
        )
        .bind(album).bind(&t.title).bind(&t.artist).bind(t.disc).bind(t.track).bind(t.duration)
        .bind(t.rg.0).bind(t.rg.1).bind(t.rg.2).bind(t.rg.3).bind(t.rg.0.is_some()).bind(t.path.to_str())
        .execute(&mut *tx).await?;
        added += 1;
    }
    tx.commit().await?;
    crate::musicbrainz::propagate_covers(app).await?;
    tracing::info!("scan terminé : {added} nouveaux fichiers");
    Ok(())
}

/// (racine, fichier) pour chaque fichier d'une des extensions sous les racines.
/// Un fichier n'apparaît qu'une fois même si les racines se recoupent (dossier listé deux fois ou inclus dans un autre).
fn walk(roots: &[PathBuf], exts: &'static [&'static str]) -> Vec<(PathBuf, PathBuf)> {
    let mut seen = HashSet::new();
    roots
        .iter()
        .flat_map(|root| {
            WalkDir::new(root).follow_links(true).into_iter().filter_map(Result::ok).filter_map(move |e| {
                let ext = e.path().extension()?.to_str()?.to_ascii_lowercase();
                (e.file_type().is_file() && exts.contains(&ext.as_str())).then(|| (root.clone(), e.into_path()))
            })
        })
        .filter(|(_, p)| seen.insert(p.clone()))
        .collect()
}

struct TrackTags {
    path: PathBuf,
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    track: Option<u32>,
    disc: Option<u32>,
    year: Option<i64>,
    duration: f64,
    mbid: Option<String>,
    artist_mbid: Option<String>,
    /// ReplayGain converti : (volume piste LUFS, pic piste, volume album, pic album).
    rg: (Option<f64>, Option<f64>, Option<f64>, Option<f64>),
    /// Pochette (octets, extension) : seulement pour la première piste de chaque dossier qui en a une.
    cover: Option<(Vec<u8>, &'static str)>,
}

/// Lit les tags. Valeurs manquantes : nom du fichier, nom du dossier, « Artiste inconnu ».
fn read_tracks(files: Vec<PathBuf>) -> Vec<TrackTags> {
    let mut covered: HashSet<PathBuf> = HashSet::new();
    let mut tracks: Vec<(TrackTags, bool)> = files
        .into_iter()
        .map(|path| {
            let file = lofty::read_from_path(&path).ok();
            // Formats que lofty ne lit pas (WMA/ASF…) : tags et durée via ffprobe.
            let probed = if file.is_none() { ffprobe_tags(&path) } else { None };
            let tag = file.as_ref().and_then(|f| f.primary_tag().or(f.first_tag())).or(probed.as_ref().map(|(t, _)| t));
            let text = |v: Option<std::borrow::Cow<'_, str>>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
            let key = |k: ItemKey| tag.and_then(|t| t.get_string(k)).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
            let dir = path.parent().unwrap_or(&path).to_path_buf();
            let artist = text(tag.and_then(|t| t.artist()));
            let tagged_album_artist = key(ItemKey::AlbumArtist);
            let cover = if covered.contains(&dir) { None } else { folder_art(&dir).or_else(|| tag.and_then(embedded_art)) };
            if cover.is_some() {
                covered.insert(dir.clone());
            }
            let t = TrackTags {
                title: text(tag.and_then(|t| t.title())).unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into()),
                album: text(tag.and_then(|t| t.album())).unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into()),
                album_artist: tagged_album_artist.clone().or(artist.clone()).unwrap_or_else(|| "Artiste inconnu".into()),
                artist: artist.or(tagged_album_artist.clone()).unwrap_or_else(|| "Artiste inconnu".into()),
                track: tag.and_then(|t| t.track()),
                disc: tag.and_then(|t| t.disk()),
                year: tag.and_then(|t| t.date()).map(|d| d.year as i64),
                duration: file.as_ref().map(|f| f.properties().duration().as_secs_f64()).or(probed.as_ref().map(|(_, d)| *d)).unwrap_or(0.0),
                mbid: key(ItemKey::MusicBrainzReleaseId),
                artist_mbid: key(ItemKey::MusicBrainzReleaseArtistId),
                rg: (
                    key(ItemKey::ReplayGainTrackGain).and_then(|g| crate::loudness::loudness_from_gain(&g)),
                    key(ItemKey::ReplayGainTrackPeak).and_then(|p| p.trim().parse().ok()),
                    key(ItemKey::ReplayGainAlbumGain).and_then(|g| crate::loudness::loudness_from_gain(&g)),
                    key(ItemKey::ReplayGainAlbumPeak).and_then(|p| p.trim().parse().ok()),
                ),
                cover,
                path,
            };
            (t, tagged_album_artist.is_some())
        })
        .collect();
    // Compilation sans « album artist » : plusieurs artistes dans un même album du même dossier.
    let mut artists: HashMap<(PathBuf, String), HashSet<String>> = HashMap::new();
    for (t, tagged) in &tracks {
        if !tagged {
            artists.entry((t.path.parent().unwrap_or(&t.path).into(), t.album.clone())).or_default().insert(t.artist.clone());
        }
    }
    for (t, tagged) in &mut tracks {
        if !*tagged && artists.get(&(t.path.parent().unwrap_or(&t.path).into(), t.album.clone())).is_some_and(|a| a.len() > 1) {
            t.album_artist = VARIOUS.into();
        }
    }
    tracks.into_iter().map(|(t, _)| t).collect()
}

/// Tags ffprobe convertis en `Tag` lofty, pour réutiliser la même lecture que les autres formats.
fn ffprobe_tags(path: &Path) -> Option<(Tag, f64)> {
    let out = std::process::Command::new("ffprobe").args(["-v", "error", "-print_format", "json", "-show_format"]).arg(path).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
    for (k, val) in v["format"]["tags"].as_object()? {
        let Some(val) = val.as_str() else { continue };
        let key = match k.to_ascii_lowercase().as_str() {
            "title" => ItemKey::TrackTitle,
            "artist" | "author" => ItemKey::TrackArtist,
            "album" => ItemKey::AlbumTitle,
            "album_artist" | "albumartist" | "wm/albumartist" => ItemKey::AlbumArtist,
            "track" | "wm/tracknumber" => ItemKey::TrackNumber,
            "disc" | "wm/partofset" => ItemKey::DiscNumber,
            "date" | "year" | "wm/year" => ItemKey::Year,
            _ => continue,
        };
        // « 3/12 » -> « 3 » pour les numéros.
        tag.insert_text(key, val.split('/').next().unwrap_or(val).trim().to_owned());
    }
    let duration = v["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0);
    Some((tag, duration))
}

fn folder_art(dir: &Path) -> Option<(Vec<u8>, &'static str)> {
    FOLDER_ART.iter().find_map(|f| Some((std::fs::read(dir.join(f)).ok()?, if f.ends_with("png") { "png" } else { "jpg" })))
}

fn embedded_art(tag: &Tag) -> Option<(Vec<u8>, &'static str)> {
    let pics = tag.pictures();
    let p = pics.iter().find(|p| p.pic_type() == lofty::picture::PictureType::CoverFront).or(pics.first())?;
    Some((p.data().to_vec(), if p.mime_type() == Some(&MimeType::Png) { "png" } else { "jpg" }))
}

static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?)[\s._(\[-]+((?:19|20)\d{2})(?:[\s._)\]-]|$)").unwrap());
static EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:s(\d{1,2})[ ._-]?e(\d{1,3})|\b(\d{1,2})x(\d{2,3})\b)").unwrap());

/// "The.Matrix.1999.1080p" -> ("The Matrix", Some(1999)).
fn clean_title(s: &str) -> (String, Option<i64>) {
    let tidy = |t: &str| t.replace(['.', '_'], " ").split_whitespace().collect::<Vec<_>>().join(" ");
    match YEAR.captures(s) {
        Some(c) => (tidy(&c[1]), c[2].parse().ok()),
        None => (tidy(s), None),
    }
}

/// Nom du fichier, ou du dossier parent si seul celui-ci contient l'année.
fn movie_name(file: &Path) -> (String, Option<i64>) {
    let stem = file.file_stem().unwrap_or_default().to_string_lossy();
    let from_file = clean_title(&stem);
    if from_file.1.is_some() {
        return from_file;
    }
    match file.parent().and_then(Path::file_name).map(|d| clean_title(&d.to_string_lossy())) {
        Some(from_dir) if from_dir.1.is_some() => from_dir,
        _ => from_file,
    }
}

fn episode_num(s: &str) -> Option<(i64, i64)> {
    let c = EPISODE.captures_iter(s).last()?;
    let s = c.get(1).or(c.get(3))?.as_str().parse().ok()?;
    let e = c.get(2).or(c.get(4))?.as_str().parse().ok()?;
    Some((s, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(clean_title("The.Matrix.1999.1080p.BluRay"), ("The Matrix".into(), Some(1999)));
        assert_eq!(clean_title("2001 A Space Odyssey (1968)"), ("2001 A Space Odyssey".into(), Some(1968)));
        assert_eq!(clean_title("Amélie"), ("Amélie".into(), None));
        assert_eq!(movie_name(Path::new("/m/Heat (1995)/heat.mkv")), ("Heat".into(), Some(1995)));
    }

    #[test]
    fn untagged_tracks() {
        let dir = std::env::temp_dir().join(format!("bitflow-music-{}", std::process::id())).join("Mon Album");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("01 Intro.mp3"), b"").unwrap();
        let t = read_tracks(vec![dir.join("01 Intro.mp3")]);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
        assert_eq!((t[0].title.as_str(), t[0].album.as_str(), t[0].album_artist.as_str()), ("01 Intro", "Mon Album", "Artiste inconnu"));
    }

    #[test]
    fn episodes() {
        assert_eq!(episode_num("/tv/Breaking Bad/Season 1/Breaking.Bad.S01E02.mkv"), Some((1, 2)));
        assert_eq!(episode_num("/tv/Show/show - 3x07.mkv"), Some((3, 7)));
        assert_eq!(episode_num("/tv/Show/extras.mkv"), None);
    }
}
