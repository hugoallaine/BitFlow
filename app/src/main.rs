mod api;
mod music;
mod photos;
mod player;

use bitflow_shared::{AuthConfig, Credentials, Item, Me, Prefs, Progress, ScanStatus};
use dioxus::prelude::*;
use photos::{PhotoAlbum, PhotoFolders, PhotoMap, PhotoPeople, PhotoPerson, Photos};
use player::Play;

const CSS: Asset = asset!("/assets/main.css");
const HLS: Asset = asset!("/assets/hls.min.js");

fn main() {
    dioxus::launch(App);
}

#[derive(Routable, Clone, PartialEq)]
enum Route {
    #[layout(Shell)]
    #[route("/")]
    Home {},
    #[route("/movies")]
    Movies {},
    #[route("/shows")]
    Shows {},
    #[route("/music")]
    Music {},
    #[route("/artists")]
    Artists {},
    #[route("/photos")]
    Photos {},
    #[route("/photos/folders")]
    PhotoFolders {},
    #[route("/photos/map")]
    PhotoMap {},
    #[route("/photos/people")]
    PhotoPeople {},
    #[route("/photos/people/:id")]
    PhotoPerson { id: i64 },
    #[route("/photos/album?:owner&:folder")]
    PhotoAlbum { owner: i64, folder: String },
    #[route("/item/:id")]
    Detail { id: i64 },
    #[route("/search/:q")]
    Search { q: String },
    #[route("/admin")]
    Admin {},
    #[route("/settings")]
    Settings {},
    #[end_layout]
    #[route("/play/:id")]
    Play { id: i64 },
    #[route("/login")]
    Login {},
}

#[component]
fn App() -> Element {
    use_context_provider(|| Signal::new(music::Queue::default()));
    use_context_provider(|| Signal::new(music::load_settings()));
    rsx! {
        document::Title { "BitFlow" }
        document::Stylesheet { href: CSS }
        document::Script { src: HLS }
        Router::<Route> {}
    }
}

#[component]
fn Shell() -> Element {
    let me = use_resource(|| api::get::<Me>("/api/me"));
    let mut q = use_signal(String::new);
    rsx! {
        header { class: "top",
            Link { class: "logo", to: Route::Home {}, "BitFlow" }
            nav {
                Link { to: Route::Movies {}, "Films" }
                Link { to: Route::Shows {}, "Séries" }
                Link { to: Route::Music {}, "Musiques" }
                Link { to: Route::Photos {}, "Photos" }
            }
            form {
                class: "search",
                onsubmit: move |e| {
                    e.prevent_default();
                    if !q().trim().is_empty() {
                        navigator().push(Route::Search { q: q() });
                    }
                },
                input { r#type: "search", placeholder: "Rechercher…", value: "{q}", oninput: move |e| q.set(e.value()) }
            }
            Link { to: Route::Settings {}, title: "Paramètres", "⚙" }
            if let Some(Ok(me)) = &*me.read_unchecked() {
                if me.admin {
                    Link { to: Route::Admin {}, "Admin" }
                }
                button {
                    class: "ghost",
                    title: "Se déconnecter",
                    onclick: move |_| async move {
                        let _ = api::post::<()>("/api/auth/logout", &()).await;
                        api::go("/login");
                    },
                    "{me.name} ⏻"
                }
            }
        }
        main { Outlet::<Route> {} }
        music::MusicBar {}
    }
}

#[component]
fn Home() -> Element {
    let mut status = use_resource(|| api::get::<ScanStatus>("/api/scan"));
    let empty = matches!(&*status.read(), Some(Ok(s)) if s.libraries.iter().all(|l| l.files == 0));
    if empty {
        return rsx! {
            section { class: "empty",
                h1 { "Bienvenue sur BitFlow" }
                p { class: "muted", "Aucun média pour l'instant. Vérifiez les dossiers ci-dessous puis lancez une analyse." }
                LibraryPanel { on_done: move |_| status.restart() }
            }
        };
    }
    rsx! {
        Row { title: "Reprendre", path: "/api/resume", play: true }
        Row { title: "Films récemment ajoutés", path: "/api/items?kind=movie&sort=added&limit=30" }
        Row { title: "Séries récemment ajoutées", path: "/api/items?kind=show&sort=added&limit=30" }
        Row { title: "Albums récemment ajoutés", path: "/api/items?kind=album&sort=added&limit=30" }
    }
}

#[component]
fn Movies() -> Element {
    rsx! { Grid { title: "Films", path: "/api/items?kind=movie" } }
}

#[component]
fn Shows() -> Element {
    rsx! { Grid { title: "Séries", path: "/api/items?kind=show" } }
}

#[component]
fn Music() -> Element {
    rsx! {
        div { class: "tabs",
            Link { class: "tab active", to: Route::Music {}, "Albums" }
            Link { class: "tab", to: Route::Artists {}, "Artistes" }
        }
        Grid { title: "Albums", path: "/api/items?kind=album" }
    }
}

#[component]
fn Artists() -> Element {
    rsx! {
        div { class: "tabs",
            Link { class: "tab", to: Route::Music {}, "Albums" }
            Link { class: "tab active", to: Route::Artists {}, "Artistes" }
        }
        Grid { title: "Artistes", path: "/api/items?kind=artist" }
    }
}

#[component]
fn Search(q: String) -> Element {
    rsx! { Grid { title: "Résultats pour « {q} »", path: format!("/api/search?q={}", api::enc(&q)) } }
}

#[component]
fn Row(title: String, path: String, #[props(default)] play: bool) -> Element {
    let items = use_resource(use_reactive!(|path| async move { api::get::<Vec<Item>>(&path).await }));
    match &*items.read_unchecked() {
        Some(Ok(v)) if !v.is_empty() => rsx! {
            section {
                h2 { "{title}" }
                div { class: "row",
                    for it in v.iter().cloned() {
                        Card { key: "{it.id}", item: it, play }
                    }
                }
            }
        },
        _ => rsx! {},
    }
}

#[component]
fn Grid(title: String, path: String) -> Element {
    let items = use_resource(use_reactive!(|path| async move { api::get::<Vec<Item>>(&path).await }));
    rsx! {
        h1 { "{title}" }
        match &*items.read_unchecked() {
            Some(Ok(v)) if v.is_empty() => rsx! { p { class: "muted", "Rien ici pour le moment." } },
            Some(Ok(v)) => rsx! {
                div { class: "grid",
                    for it in v.iter().cloned() {
                        Card { key: "{it.id}", item: it }
                    }
                }
            },
            Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
            None => rsx! { div { class: "loading" } },
        }
    }
}

fn clock(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

#[component]
fn Card(item: Item, #[props(default)] play: bool) -> Element {
    let to = match item.kind.as_str() {
        _ if play => Route::Play { id: item.id },
        // Une piste s'ouvre sur son album.
        "track" => Route::Detail { id: item.parent_id.unwrap_or(item.id) },
        _ => Route::Detail { id: item.id },
    };
    let wide = item.kind == "episode";
    let square = matches!(item.kind.as_str(), "album" | "artist" | "track");
    let src = if wide { api::img(&item.backdrop, "backdrop") } else { api::img(&item.poster, "poster") };
    let sub = if square && item.kind != "artist" { item.artist.clone() } else { item.year.map(|y| y.to_string()) };
    rsx! {
        Link { class: if wide { "card wide" } else if square { "card square" } else { "card" }, to,
            div { class: "thumb",
                if let Some(src) = src {
                    img { src, alt: "{item.title}", "loading": "lazy" }
                } else {
                    div { class: "noposter", "{item.title}" }
                }
                if item.watched {
                    span { class: "badge", "✓" }
                }
                if play && item.position > 0.0 {
                    span { class: "resume", "▶ {clock(item.position)}" }
                }
            }
            div { class: "meta",
                span { "{item.title}" }
                if let Some(s) = sub {
                    small { "{s}" }
                }
            }
        }
    }
}

#[component]
fn Detail(id: i64) -> Element {
    let mut item = use_resource(use_reactive!(|id| async move { api::get::<Item>(&format!("/api/items/{id}")).await }));
    let mut seasons = use_resource(use_reactive!(|id| async move { api::get::<Vec<Item>>(&format!("/api/items/{id}/children")).await }));
    let mut season = use_signal(|| None::<i64>);
    let mut episodes = use_resource(move || async move {
        match season() {
            Some(s) => api::get::<Vec<Item>>(&format!("/api/items/{s}/children")).await,
            None => Ok(vec![]),
        }
    });
    // Première saison sélectionnée par défaut.
    use_effect(move || {
        if let Some(Ok(v)) = &*seasons.read() {
            if season.peek().is_none_or(|s| !v.iter().any(|x| x.id == s)) {
                season.set(v.first().map(|x| x.id));
            }
        }
    });
    let toggle = move |target: i64, watched: bool| async move {
        let _ = api::post::<()>(&format!("/api/items/{target}/progress"), &Progress { position: 0.0, watched }).await;
        item.restart();
        seasons.restart();
        episodes.restart();
    };

    let it = match &*item.read_unchecked() {
        Some(Ok(it)) => it.clone(),
        Some(Err(e)) => return rsx! { p { class: "error", "{e}" } },
        None => return rsx! { div { class: "loading" } },
    };
    match it.kind.as_str() {
        "album" => return rsx! { music::AlbumView { item: it } },
        "artist" => return rsx! { music::ArtistView { item: it } },
        _ => {}
    }
    let eps = match &*episodes.read_unchecked() {
        Some(Ok(v)) => v.clone(),
        _ => vec![],
    };
    let show = it.kind == "show";
    let play_id = if show { eps.iter().find(|e| !e.watched).or(eps.first()).map(|e| e.id) } else { Some(it.id) };
    let backdrop = api::img(&it.backdrop, "backdrop").map(|u| format!("background-image:url({u})")).unwrap_or_default();

    rsx! {
        div { class: "hero", style: "{backdrop}",
            div { class: "hero-body",
                if let Some(p) = api::img(&it.poster, "poster") {
                    img { class: "poster", src: p, alt: "{it.title}" }
                }
                div {
                    h1 { "{it.title}" }
                    p { class: "facts",
                        if let Some(y) = it.year { span { "{y}" } }
                        if let Some(r) = it.rating { span { "★ {r:.1}" } }
                        if let (Some(s), Some(e)) = (it.season, it.episode) { span { "S{s:02}E{e:02}" } }
                        if let Some(imdb) = &it.imdb_id {
                            a { href: "https://www.imdb.com/title/{imdb}", target: "_blank", rel: "noopener", "IMDb" }
                        }
                    }
                    if let Some(o) = &it.overview { p { class: "overview", "{o}" } }
                    div { class: "actions",
                        if let Some(pid) = play_id {
                            Link { class: "btn primary", to: Route::Play { id: pid },
                                if !show && it.position > 0.0 { "▶ Reprendre ({clock(it.position)})" } else { "▶ Lecture" }
                            }
                        }
                        if !show {
                            Download { id }
                        }
                        button { class: "btn", onclick: move |_| toggle(id, !it.watched),
                            if it.watched { "Marquer non vu" } else { "Marquer vu" }
                        }
                    }
                }
            }
        }
        if show {
            if let Some(Ok(list)) = &*seasons.read_unchecked() {
                div { class: "tabs",
                    for s in list.iter().cloned() {
                        button {
                            key: "{s.id}",
                            class: if season() == Some(s.id) { "tab active" } else { "tab" },
                            onclick: move |_| season.set(Some(s.id)),
                            "{s.title}"
                        }
                    }
                }
            }
            div { class: "episodes",
                for e in eps.into_iter() {
                    div { key: "{e.id}", class: "episode",
                        Link { class: "still", to: Route::Play { id: e.id },
                            if let Some(src) = api::img(&e.backdrop, "backdrop") { img { src, "loading": "lazy" } }
                        }
                        div { class: "ep-body",
                            h3 { "{e.episode.unwrap_or(0)}. {e.title}" }
                            if let Some(o) = &e.overview { p { "{o}" } }
                        }
                        Download { id: e.id, compact: true }
                        button {
                            class: if e.watched { "check on" } else { "check" },
                            title: if e.watched { "Marquer non vu" } else { "Marquer vu" },
                            onclick: move |_| toggle(e.id, !e.watched),
                            "✓"
                        }
                    }
                }
            }
        }
    }
}

pub const QUALITY_KEY: &str = "bitflow.download_quality";
const QUALITIES: &[(&str, &str)] = &[
    ("original", "Originale (fichier source, sans conversion)"),
    ("2160", "4K (2160p)"),
    ("1080", "1080p"),
    ("720", "720p"),
    ("480", "480p"),
];

fn download_quality() -> String {
    api::setting(QUALITY_KEY).filter(|q| QUALITIES.iter().any(|(k, _)| k == q)).unwrap_or("1080".into())
}

/// Télécharge dans la qualité des paramètres. Hors « originale », le serveur convertit d'abord le fichier :
/// on interroge l'avancement toutes les 2 s, puis le navigateur télécharge le résultat.
#[component]
fn Download(id: i64, #[props(default)] compact: bool) -> Element {
    let mut status = use_signal(|| None::<String>);
    let label = status().unwrap_or(if compact { "⤓".into() } else { "⤓ Télécharger".into() });
    rsx! {
        button {
            class: if compact { "check" } else { "btn" },
            title: "Télécharger",
            disabled: status().is_some(),
            onclick: move |_| async move {
                let url = format!("/api/download/{id}/{}", download_quality());
                loop {
                    match api::get::<f64>(&format!("{url}?status=true")).await {
                        Ok(p) if p < 1.0 => {
                            status.set(Some(format!("{:.0} %", p * 100.0)));
                            gloo_timers::future::TimeoutFuture::new(2000).await;
                        }
                        Ok(_) => {
                            status.set(None);
                            api::go(&url);
                            break;
                        }
                        Err(e) => {
                            status.set(None);
                            if let Some(w) = web_sys::window() {
                                let _ = w.alert_with_message(&e);
                            }
                            break;
                        }
                    }
                }
            },
            "{label}"
        }
    }
}

const LANGS: &[(&str, &str)] = &[
    ("fr", "Français"), ("en", "English"), ("de", "Deutsch"), ("it", "Italiano"), ("es", "Español"),
    ("pt", "Português"), ("nl", "Nederlands"), ("ja", "日本語"), ("ko", "한국어"), ("zh", "中文"),
];
const SUB_MODES: &[(&str, &str)] = &[
    ("foreign", "Si l'audio est dans une autre langue"),
    ("always", "Toujours"),
    ("off", "Jamais"),
];

/// Préférences de lecture, enregistrées sur le serveur : identiques sur tous les appareils.
#[component]
fn PlaybackPrefs() -> Element {
    let mut prefs = use_resource(|| api::get::<Prefs>("/api/prefs"));
    let mut msg = use_signal(String::new);
    let Some(Ok(p)) = prefs.read_unchecked().clone() else { return rsx! { div { class: "loading" } } };
    // Rechargement après enregistrement : chaque liste repart des valeurs à jour.
    let save = move |p: Prefs| async move {
        match api::put::<()>("/api/prefs", &p).await {
            Ok(()) => {
                msg.set("Enregistré.".into());
                prefs.restart();
            }
            Err(e) => msg.set(e),
        }
    };
    let (p1, p2, p3) = (p.clone(), p.clone(), p.clone());
    rsx! {
        section { class: "panel",
            h2 { "Lecture" }
            form { onsubmit: move |e| e.prevent_default(),
                label { r#for: "audio-lang", "Langue audio préférée" }
                select {
                    id: "audio-lang",
                    onchange: move |e| save(Prefs { audio_lang: Some(e.value()).filter(|v| !v.is_empty()), ..p1.clone() }),
                    option { value: "", selected: p.audio_lang.is_none(), "Version originale" }
                    for (k, name) in LANGS {
                        option { value: *k, selected: p.audio_lang.as_deref() == Some(*k), "{name}" }
                    }
                }
                label { r#for: "sub-lang", "Langue des sous-titres" }
                select {
                    id: "sub-lang",
                    onchange: move |e| save(Prefs { sub_lang: e.value(), ..p2.clone() }),
                    for (k, name) in LANGS {
                        option { value: *k, selected: p.sub_lang == *k, "{name}" }
                    }
                }
                label { r#for: "sub-mode", "Afficher les sous-titres automatiquement" }
                select {
                    id: "sub-mode",
                    onchange: move |e| save(Prefs { sub_mode: e.value(), ..p3.clone() }),
                    for (k, name) in SUB_MODES {
                        option { value: *k, selected: p.sub_mode == *k, "{name}" }
                    }
                }
            }
            p { class: "muted",
                "En mode « autre langue », si l'audio est déjà dans votre langue, seuls les sous-titres forcés "
                "(passages en langue étrangère) sont affichés. Vous pouvez toujours changer de piste pendant la lecture."
            }
            p { class: "muted", "{msg}" }
        }
    }
}

#[component]
fn Settings() -> Element {
    let mut quality = use_signal(download_quality);
    rsx! {
        h1 { "Paramètres" }
        PlaybackPrefs {}
        music::MusicSettingsPanel {}
        section { class: "panel",
            h2 { "Téléchargements" }
            label { r#for: "quality", "Qualité des fichiers téléchargés" }
            select {
                id: "quality",
                onchange: move |e| {
                    api::set_setting(QUALITY_KEY, &e.value());
                    quality.set(e.value());
                },
                for (k, label) in QUALITIES {
                    option { value: *k, selected: quality() == *k, "{label}" }
                }
            }
            p { class: "muted",
                "Une qualité inférieure à l'original est convertie par le serveur avant le téléchargement. "
                "Si la source est déjà en H.264 et pas plus haute que la qualité choisie, elle est simplement réemballée en MP4 (quasi instantané)."
            }
        }
    }
}

#[component]
fn Admin() -> Element {
    let mut name = use_signal(String::new);
    let mut pw = use_signal(String::new);
    let mut msg = use_signal(String::new);
    rsx! {
        h1 { "Administration" }
        section { class: "panel",
            h2 { "Bibliothèque" }
            LibraryPanel {}
        }
        section { class: "panel",
            h2 { "Nouvel utilisateur" }
            form {
                onsubmit: move |e| async move {
                    e.prevent_default();
                    let c = Credentials { name: name(), password: pw() };
                    msg.set(match api::post::<()>("/api/users", &c).await {
                        Ok(_) => format!("Compte « {} » créé.", c.name),
                        Err(e) => e,
                    });
                    pw.set(String::new());
                },
                input { placeholder: "Nom", value: "{name}", oninput: move |e| name.set(e.value()), required: true }
                input { r#type: "password", placeholder: "Phrase secrète (15 caractères min.)", "autocomplete": "new-password", value: "{pw}", oninput: move |e| pw.set(e.value()), required: true }
                button { class: "btn", r#type: "submit", "Créer" }
            }
        }
        p { class: "muted", "{msg}" }
    }
}

/// Dossiers de médias, nombre de fichiers indexés, erreurs d'accès et bouton d'analyse.
#[component]
fn LibraryPanel(#[props(default)] on_done: EventHandler<()>) -> Element {
    let mut status = use_signal(|| None::<ScanStatus>);
    let mut msg = use_signal(String::new);
    // Recharge l'état toutes les 2 s tant qu'une analyse tourne.
    let refresh = move |mut seen_running: bool| async move {
        while let Ok(s) = api::get::<ScanStatus>("/api/scan").await {
            let running = s.running;
            status.set(Some(s));
            if !running {
                // Seulement après une analyse lancée ou suivie : l'accueil se recharge sans boucler.
                if seen_running {
                    on_done.call(());
                }
                break;
            }
            seen_running = true;
            gloo_timers::future::TimeoutFuture::new(2000).await;
        }
    };
    use_future(move || refresh(false));
    let Some(s) = status() else { return rsx! {} };
    rsx! {
        div { class: "libs",
            for l in s.libraries {
                div { class: "lib",
                    strong { "{l.kind}" }
                    code { "{l.path}" }
                    match l.error {
                        Some(e) => rsx! { span { class: "error", "{e}" } },
                        None => rsx! { span { class: "muted", "{l.files} fichiers" } },
                    }
                }
            }
        }
        p { class: "muted small",
            "Noms attendus : films « Titre (2020).mkv », séries « Série/Saison 1/Série S01E02.mkv », musiques selon les tags des fichiers."
        }
        button {
            class: "btn primary",
            disabled: s.running,
            onclick: move |_| async move {
                match api::post::<()>("/api/scan", &()).await {
                    Ok(_) => {
                        msg.set(String::new());
                        // Laisse le serveur démarrer l'analyse avant de suivre son état.
                        gloo_timers::future::TimeoutFuture::new(300).await;
                        refresh(true).await;
                    }
                    Err(e) => msg.set(e),
                }
            },
            if s.running { "Analyse en cours…" } else { "Analyser la bibliothèque" }
        }
        p { class: "error", "{msg}" }
    }
}

#[component]
fn Login() -> Element {
    let cfg = use_resource(|| api::get::<AuthConfig>("/api/auth/config"));
    let mut name = use_signal(String::new);
    let mut pw = use_signal(String::new);
    let mut msg = use_signal(String::new);
    let setup = matches!(&*cfg.read_unchecked(), Some(Ok(c)) if c.setup);
    let oidc = matches!(&*cfg.read_unchecked(), Some(Ok(c)) if c.oidc);
    rsx! {
        div { class: "login",
            form {
                onsubmit: move |e| async move {
                    e.prevent_default();
                    let c = Credentials { name: name(), password: pw() };
                    if setup {
                        if let Err(e) = api::post::<()>("/api/users", &c).await {
                            return msg.set(e);
                        }
                    }
                    match api::post::<String>("/api/auth/login", &c).await {
                        Ok(_) => api::go("/"),
                        Err(e) => msg.set(e),
                    }
                },
                h1 { class: "logo", "BitFlow" }
                if setup { p { "Bienvenue ! Créez le compte administrateur." } }
                input { placeholder: "Nom d'utilisateur", "autocomplete": "username", value: "{name}", oninput: move |e| name.set(e.value()), required: true }
                input {
                    r#type: "password",
                    placeholder: if setup { "Phrase secrète (15 caractères min.)" } else { "Phrase secrète" },
                    "autocomplete": if setup { "new-password" } else { "current-password" },
                    value: "{pw}",
                    oninput: move |e| pw.set(e.value()),
                    required: true,
                }
                button { class: "btn primary", r#type: "submit", if setup { "Créer et se connecter" } else { "Se connecter" } }
                if oidc && !setup {
                    a { class: "btn", href: "/api/auth/oidc", "Connexion via SSO" }
                }
                p { class: "error", "{msg}" }
            }
        }
    }
}
