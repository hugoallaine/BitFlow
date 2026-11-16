//! Types échangés entre le serveur et les clients (JSON).
use serde::{Deserialize, Serialize};

/// Film, série, saison ou épisode, avec la progression de l'utilisateur courant.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Item {
    pub id: i64,
    /// movie | show | season | episode
    pub kind: String,
    pub parent_id: Option<i64>,
    pub title: String,
    pub year: Option<i64>,
    pub overview: Option<String>,
    /// Chemins d'images TMDB (ex. "/abc.jpg").
    pub poster: Option<String>,
    pub backdrop: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub imdb_id: Option<String>,
    pub tmdb_id: Option<i64>,
    pub rating: Option<f64>,
    /// Musique : artiste de la piste, ou de l'album.
    pub artist: Option<String>,
    /// Musique : durée de la piste en secondes.
    pub duration: Option<f64>,
    /// Musique : volume intégré (LUFS) et pic vrai (linéaire) de la piste et de son album, pour la normalisation.
    pub loudness: Option<f64>,
    pub peak: Option<f64>,
    pub album_loudness: Option<f64>,
    pub album_peak: Option<f64>,
    pub position: f64,
    pub watched: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Track {
    /// Index relatif au type de flux (0:a:N ou 0:s:N pour ffmpeg).
    pub index: usize,
    pub label: String,
    pub default: bool,
    /// Sous-titre image (PGS/VobSub) : doit être incrusté via transcodage.
    pub burn: bool,
    /// Code ISO 639-1 ("fr", "en"…), si connu.
    pub lang: Option<String>,
    /// Sous-titres forcés : seulement les passages en langue étrangère.
    pub forced: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct MediaInfo {
    pub duration: f64,
    /// Le navigateur peut lire le fichier tel quel.
    pub direct: bool,
    pub audio: Vec<Track>,
    pub subs: Vec<Track>,
    /// Épisode suivant, pour l'enchaînement automatique.
    pub next: Option<i64>,
    /// Pistes choisies par le serveur selon les préférences de l'utilisateur.
    pub audio_pick: usize,
    pub sub_pick: Option<usize>,
}

/// Préférences de lecture de l'utilisateur (communes à tous ses appareils).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Prefs {
    /// Langue audio préférée ; None = piste par défaut du fichier.
    pub audio_lang: Option<String>,
    pub sub_lang: String,
    /// off | foreign (audio dans une autre langue) | always
    pub sub_mode: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Me {
    pub name: String,
    pub admin: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct AuthConfig {
    pub oidc: bool,
    /// Aucun utilisateur : le premier compte créé sera administrateur.
    pub setup: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Credentials {
    pub name: String,
    pub password: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Progress {
    pub position: f64,
    pub watched: bool,
}
