# BitFlow

Serveur multimédia auto-hébergé en Rust : films, séries et musique ; photos et applications mobiles à venir.

- **Serveur** : axum + SQLite (WAL), un seul binaire.
- **Client** : Dioxus (Rust → WebAssembly). Le même code ciblera Android et iOS.
- **Métadonnées** : TMDB (affiches, résumés, notes, identifiant IMDb). Les images sont téléchargées une fois par le serveur et servies depuis `/data/img` (une seule taille par type pour limiter le stockage) : les navigateurs ne contactent jamais TMDB.
- **Lecture** : directe (HTTP Range) quand le navigateur sait lire le fichier, sinon transcodage HLS à la volée par ffmpeg, avec accélération matérielle (VAAPI, NVENC ou QSV).
- **Lecteur** : reprise de lecture, choix de la piste audio, sous-titres intégrés (texte et image incrustée) ou en fichiers à côté de la vidéo (`Film.fr.srt`), sélection automatique selon les préférences de l'utilisateur (langue audio, langue des sous-titres, affichage jamais / si l'audio est dans une autre langue / toujours), qualité 1080p/720p/480p, vitesse, image dans l'image, plein écran, enchaînement des épisodes, raccourcis clavier (espace/k, ←/→, ↑/↓, f, m).
- **Musique** : tags ID3/FLAC/MP4/Vorbis (lofty, repli ffprobe pour WMA…), compilations regroupées sous « Artistes divers », pochettes (`cover.jpg` du dossier, image intégrée, sinon Cover Art Archive) stockées en local, MusicBrainz pour l'identifiant et l'année. Lecture directe (MP3, AAC, FLAC, Opus, Vorbis, WAV) ou conversion MP3 (WMA, APE, ALAC). Barre de lecture persistante : file d'attente, aléatoire, répétition, minuterie, touches multimédia et écran verrouillé (Media Session).
- **Audio (inspiré de Plexamp)** : normalisation du volume par piste/album/automatique (tags ReplayGain ou analyse EBU R128 par le serveur, cible -18 LUFS, gain plafonné au pic), enchaînement sans blanc, fondu enchaîné 0-12 s avec fondu intelligent (aucun fondu entre pistes consécutives d'un album), égaliseur 10 bandes avec préréglages et pré-ampli, limiteur. Réglages propres à chaque appareil.
- **Images d'artistes** : `artist.jpg` / `artist-poster.jpg` dans le dossier de l'artiste (convention Plex), sinon Fanart.tv avec une clé personnelle gratuite.
- **Téléchargement** : qualité choisie dans les paramètres (originale, 4K, 1080p, 720p, 480p). Hors originale, le serveur convertit en MP4 (une conversion à la fois, copies supprimées après 24 h) ; une source H.264 déjà assez petite est simplement réemballée.
- **Authentification** : phrase secrète (Argon2id, NIST SP 800-63B-4 : 15 caractères minimum, contrôle Have I Been Pwned, verrouillage après 10 échecs) ou SSO OIDC (Authorization Code + PKCE, nonce, `at_hash`, RFC 9700).

## Démarrage (Docker)

1. Créer une clé API sur [themoviedb.org](https://www.themoviedb.org/settings/api) et copier le **jeton d'accès en lecture (v4)**.
2. Adapter les chemins des médias dans `docker-compose.yml`.
3. Lancer :

```sh
TMDB_TOKEN=eyJ... docker compose up -d
```

L'image `ghcr.io/hugoallaine/bitflow:latest` est publiée par la CI à chaque push sur `main` (tags `v1.2.3` → `:1.2.3`). Pour compiler localement : `docker compose up -d --build`.

4. Ouvrir http://localhost:8080 : le premier compte créé est administrateur.

### Organisation des fichiers

```
films/   Heat (1995)/Heat.mkv          ou  The.Matrix.1999.1080p.mkv
musique/ Artiste/Album/01 Titre.flac  (organisation libre : ce sont les tags qui comptent)
series/  Breaking Bad/Saison 1/Breaking.Bad.S01E02.mkv   (ou 1x02)
```

## Configuration

| Variable | Défaut | Rôle |
|---|---|---|
| `BITFLOW_URL` | `http://localhost:8080` | URL publique. En `https://`, le cookie devient `__Host-` + `Secure`. Sert aussi à l'URL de retour OIDC. |
| `BITFLOW_MOVIES` / `BITFLOW_SHOWS` | – | Dossiers des médias, séparés par des virgules. |
| `BITFLOW_MUSIC` | – | Dossiers de musique, séparés par des virgules. |
| `BITFLOW_CONTACT` | – | Contact ajouté au User-Agent MusicBrainz (URL ou e-mail), recommandé par leurs règles. |
| `FANART_KEY` | – | Clé personnelle Fanart.tv (gratuite) pour les images d'artistes. |
| `TMDB_TOKEN` | – | Jeton TMDB v4. Sans lui, pas de métadonnées. |
| `BITFLOW_LANG` | `fr-FR` | Langue des métadonnées. |
| `BITFLOW_HWACCEL` | `none` | `vaapi`, `nvenc` ou `qsv`. |
| `OIDC_ISSUER`, `OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET` | – | SSO (Authentik, Authelia, Keycloak, Entra ID…). URI de redirection à déclarer : `{BITFLOW_URL}/api/auth/oidc/callback`. |
| `BITFLOW_IMG_POSTER` | `w342` | Taille stockée des affiches (`w92` à `w780`, `original`). |
| `BITFLOW_IMG_BACKDROP` | `w780` | Taille stockée des fonds et vignettes d'épisodes (`w300`, `w780`, `w1280`, `original`). |
| `BITFLOW_DATA` | `/data` | Base SQLite, images, cache de transcodage et téléchargements. |

Mettez BitFlow derrière un reverse proxy HTTPS (Caddy, Traefik) dès qu'il est exposé hors du réseau local.

## Développement

```sh
cargo test -p bitflow-server
cargo run -p bitflow-server                       # API sur :8080
cd app && dx serve --web                          # interface (dx = dioxus-cli 0.7.10)
cd app && dx bundle --web --release --out-dir ../target/web-out   # puis BITFLOW_WEB=target/web-out/public
```

## Feuille de route

1. **Phase 1 (livrée)** : films, séries, TMDB, authentification, lecteur web, Docker.
2. **Phase 2 (livrée)** : musique (MusicBrainz + Cover Art Archive, tags, file de lecture).
3. **Phase 3** : photos (EXIF, miniatures, chronologie).
4. **Phase 4** : applications Android et iOS (Dioxus mobile, jeton Bearer, OIDC par navigateur système, téléchargement hors ligne).

Métadonnées fournies par TMDB. Ce produit utilise l'API TMDB mais n'est ni approuvé ni certifié par TMDB.
