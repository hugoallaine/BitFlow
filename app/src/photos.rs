//! Photos : chronologie par mois, dossiers (avec partage) et visionneuse plein écran.
use bitflow_shared::{Me, Photo, PhotoFolder, Share, UserName};
use dioxus::prelude::*;

use crate::{Route, api};

const MONTHS: [&str; 12] = ["janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre", "octobre", "novembre", "décembre"];
const PAGE: usize = 300;
const LEAFLET_JS: Asset = asset!("/assets/leaflet.js");
const LEAFLET_CSS: Asset = asset!("/assets/leaflet.css");

/// Carte Leaflet + regroupement des marqueurs ; reçoit [[index, id, lat, lon]…], renvoie l'index cliqué.
const MAP_JS: &str = r#"
await new Promise(r => { const t = setInterval(() => { if (window.L && L.markerClusterGroup && document.getElementById('bf-map')) { clearInterval(t); r(); } }, 50); });
const points = await dioxus.recv();
const map = L.map('bf-map', { worldCopyJump: true });
L.tileLayer('/api/tiles/{z}/{x}/{y}', { maxZoom: 19, attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>' }).addTo(map);
const icon = (id, count) => L.divIcon({ className: 'map-ph', iconSize: [56, 56], html: `<img src="/api/photos/${id}/thumb">` + (count ? `<b>${count}</b>` : '') });
const group = L.markerClusterGroup({ showCoverageOnHover: false, maxClusterRadius: 60,
  iconCreateFunction: c => icon(c.getAllChildMarkers()[0].options.pid, c.getChildCount()) });
for (const [i, id, lat, lon] of points) {
  const m = L.marker([lat, lon], { pid: id, icon: icon(id) });
  m.on('click', () => dioxus.send(i));
  group.addLayer(m);
}
map.addLayer(group);
map.fitBounds(group.getBounds(), { maxZoom: 14, padding: [40, 40] });
"#;

/// (année, mois 1-12, jour) depuis des secondes Unix (algorithme de Howard Hinnant).
fn civil(ts: i64) -> (i64, usize, i64) {
    let z = ts.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m as usize, d)
}

fn month_label(ts: i64) -> String {
    let (y, m, _) = civil(ts);
    format!("{} {y}", MONTHS[m - 1])
}

fn date_label(ts: i64) -> String {
    let (y, m, d) = civil(ts);
    format!("{d} {} {y} · {:02}:{:02}", MONTHS[m - 1], ts.rem_euclid(86400) / 3600, ts.rem_euclid(3600) / 60)
}

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Timeline,
    Folders,
    Map,
}

#[component]
fn Tabs(active: Tab) -> Element {
    let class = |t: Tab| if t == active { "tab active" } else { "tab" };
    rsx! {
        div { class: "tabs",
            Link { class: class(Tab::Timeline), to: Route::Photos {}, "Chronologie" }
            Link { class: class(Tab::Folders), to: Route::PhotoFolders {}, "Dossiers" }
            Link { class: class(Tab::Map), to: Route::PhotoMap {}, "Carte" }
        }
    }
}

/// Toutes les photos visibles, les plus récentes d'abord, groupées par mois.
#[component]
pub fn Photos() -> Element {
    rsx! {
        Tabs { active: Tab::Timeline }
        PhotoList { query: String::new() }
    }
}

#[component]
pub fn PhotoAlbum(owner: i64, folder: String) -> Element {
    let title = if folder.is_empty() { "Racine".to_string() } else { folder.clone() };
    rsx! {
        Tabs { active: Tab::Folders }
        h1 { "{title}" }
        PhotoList { query: format!("owner={owner}&folder={}", api::enc(&folder)) }
    }
}

/// Grille paginée (PAGE photos par appel) avec intertitres de mois et visionneuse.
#[component]
fn PhotoList(query: String) -> Element {
    let mut photos = use_signal(Vec::<Photo>::new);
    let mut more = use_signal(|| false);
    let mut open = use_signal(|| None::<usize>);
    let load = move |query: String, offset: usize| async move {
        if let Ok(page) = api::get::<Vec<Photo>>(&format!("/api/photos?{query}&limit={PAGE}&offset={offset}")).await {
            more.set(page.len() == PAGE);
            if offset == 0 {
                photos.set(page);
            } else {
                photos.write().extend(page);
            }
        }
    };
    use_effect(use_reactive!(|query| {
        spawn(load(query, 0));
    }));
    let list = photos.read();
    if list.is_empty() {
        return rsx! { p { class: "muted", "Aucune photo pour le moment." } };
    }
    rsx! {
        for (i, p) in list.iter().enumerate() {
            Fragment { key: "{p.id}",
                if i == 0 || month_label(list[i - 1].taken) != month_label(p.taken) {
                    h2 { class: "month", "{month_label(p.taken)}" }
                }
                button { class: "ph", onclick: move |_| open.set(Some(i)),
                    img { src: "/api/photos/{p.id}/thumb", loading: "lazy", alt: "{p.name}" }
                    if p.video {
                        span { class: "ph-video", "▶ {crate::clock(p.duration.unwrap_or(0.0))}" }
                    }
                }
            }
        }
        if more() {
            button { class: "btn more", onclick: move |_| { spawn(load(query.clone(), photos.read().len())); }, "Afficher plus" }
        }
        if let Some(i) = open() {
            Viewer { photos: list.clone(), index: i, on_close: move |_| open.set(None), on_move: move |j| open.set(Some(j)) }
        }
    }
}

/// Plein écran : ←/→ pour naviguer, Échap pour fermer.
#[component]
fn Viewer(photos: Vec<Photo>, index: usize, on_close: EventHandler<()>, on_move: EventHandler<usize>) -> Element {
    let p = &photos[index];
    let last = photos.len() - 1;
    let place = p.lat.zip(p.lon).map(|(a, b)| format!("{a:.4}, {b:.4}"));
    rsx! {
        div {
            class: "viewer",
            tabindex: 0,
            autofocus: true,
            onmounted: move |e| async move { let _ = e.set_focus(true).await; },
            onkeydown: move |e| match e.key() {
                Key::Escape => on_close.call(()),
                Key::ArrowLeft if index > 0 => on_move.call(index - 1),
                Key::ArrowRight if index < last => on_move.call(index + 1),
                _ => {}
            },
            if p.video {
                video { key: "{p.id}", src: "/api/photos/{p.id}/original", controls: true, autoplay: true, "playsinline": "true" }
            } else {
                img { key: "{p.id}", src: "/api/photos/{p.id}/preview", alt: "{p.name}" }
            }
            div { class: "viewer-bar",
                div {
                    strong { "{date_label(p.taken)}" }
                    span { class: "muted", " · {p.name}" }
                    if let Some(pl) = place { span { class: "muted", " · 📍 {pl}" } }
                }
                div {
                    a { class: "btn", href: "/api/photos/{p.id}/original", download: "{p.name}", "Télécharger" }
                    button { class: "btn", onclick: move |_| on_close.call(()), "Fermer" }
                }
            }
            if index > 0 {
                button { class: "nav prev", title: "Précédente", onclick: move |_| on_move.call(index - 1), "‹" }
            }
            if index < last {
                button { class: "nav next", title: "Suivante", onclick: move |_| on_move.call(index + 1), "›" }
            }
        }
    }
}

/// Photos géolocalisées sur une carte ; un clic ouvre la visionneuse.
#[component]
pub fn PhotoMap() -> Element {
    let photos = use_resource(|| api::get::<Vec<Photo>>("/api/photos?geo=true&limit=20000"));
    let mut open = use_signal(|| None::<usize>);
    use_effect(move || {
        let Some(Ok(list)) = &*photos.read() else { return };
        if list.is_empty() {
            return;
        }
        let points: Vec<_> = list.iter().enumerate().map(|(i, p)| (i, p.id, p.lat, p.lon)).collect();
        let mut map = document::eval(MAP_JS);
        let _ = map.send(points);
        spawn(async move {
            while let Ok(i) = map.recv::<usize>().await {
                open.set(Some(i));
            }
        });
    });
    rsx! {
        document::Stylesheet { href: LEAFLET_CSS }
        document::Script { src: LEAFLET_JS }
        Tabs { active: Tab::Map }
        match &*photos.read() {
            Some(Ok(list)) if list.is_empty() => rsx! { p { class: "muted", "Aucune photo géolocalisée." } },
            Some(Ok(list)) => rsx! {
                div { id: "bf-map", class: "map" }
                if let Some(i) = open() {
                    Viewer { photos: list.clone(), index: i, on_close: move |_| open.set(None), on_move: move |j| open.set(Some(j)) }
                }
            },
            _ => rsx! {},
        }
    }
}

/// Dossiers à soi puis dossiers partagés par les autres.
#[component]
pub fn PhotoFolders() -> Element {
    let mut folders = use_resource(|| api::get::<Vec<PhotoFolder>>("/api/photos/folders"));
    let me = use_resource(|| api::get::<Me>("/api/me"));
    let users = use_resource(|| api::get::<Vec<UserName>>("/api/users"));
    let mut sharing = use_signal(|| None::<PhotoFolder>);
    let my_id = me.read().as_ref().and_then(|m| m.as_ref().ok()).map(|m| m.id);
    rsx! {
        Tabs { active: Tab::Folders }
        match &*folders.read() {
            Some(Ok(list)) if list.is_empty() => rsx! {
                p { class: "muted", "Aucune photo. Les photos de chaque utilisateur se trouvent dans un sous-dossier à son nom (BITFLOW_PHOTOS/<nom>)." }
            },
            Some(Ok(list)) => rsx! {
                div { class: "grid",
                    for f in list.iter().cloned() {
                        div { key: "{f.owner}/{f.folder}", class: "card",
                            Link { to: Route::PhotoAlbum { owner: f.owner, folder: f.folder.clone() },
                                div { class: "thumb square", img { src: "/api/photos/{f.cover}/thumb", loading: "lazy" } }
                            }
                            div { class: "meta",
                                span { if f.folder.is_empty() { "Racine" } else { "{f.folder}" } }
                                small { "{f.count}" }
                            }
                            if Some(f.owner) == my_id {
                                button { class: "ghost small", onclick: move |_| sharing.set(Some(f.clone())),
                                    if f.shared_with.is_empty() { "Partager" } else { "Partagé ({f.shared_with.len()})" }
                                }
                            } else {
                                small { class: "muted", "Partagé par {f.owner_name}" }
                            }
                        }
                    }
                }
            },
            _ => rsx! {},
        }
        if let (Some(f), Some(Ok(users))) = (sharing(), &*users.read()) {
            ShareDialog {
                folder: f,
                users: users.iter().filter(|u| Some(u.id) != my_id).cloned().collect::<Vec<_>>(),
                on_close: move |changed: bool| {
                    sharing.set(None);
                    if changed {
                        folders.restart();
                    }
                },
            }
        }
    }
}

#[component]
fn ShareDialog(folder: PhotoFolder, users: Vec<UserName>, on_close: EventHandler<bool>) -> Element {
    let mut picked = use_signal(|| folder.shared_with.clone());
    let mut msg = use_signal(String::new);
    let name = if folder.folder.is_empty() { "toutes vos photos".to_string() } else { format!("« {} »", folder.folder) };
    let target = folder.folder.clone();
    rsx! {
        div { class: "dialog-bg", onclick: move |_| on_close.call(false),
            div { class: "panel dialog", onclick: move |e| e.stop_propagation(),
                h2 { "Partager {name}" }
                p { class: "muted small", "Les sous-dossiers sont partagés aussi. Les personnes choisies peuvent voir et télécharger ces photos." }
                if users.is_empty() {
                    p { class: "muted", "Aucun autre utilisateur." }
                }
                for u in users {
                    label { key: "{u.id}", class: "check-row",
                        input {
                            r#type: "checkbox",
                            checked: picked.read().contains(&u.id),
                            onchange: move |e| {
                                if e.checked() { picked.write().push(u.id) } else { picked.write().retain(|x| *x != u.id) }
                            },
                        }
                        "{u.name}"
                    }
                }
                div { class: "actions",
                    button {
                        class: "btn primary",
                        onclick: move |_| {
                            let share = Share { folder: target.clone(), users: picked() };
                            async move {
                                match api::put("/api/photos/shares", &share).await {
                                    Ok(_) => on_close.call(true),
                                    Err(e) => msg.set(e),
                                }
                            }
                        },
                        "Enregistrer"
                    }
                    button { class: "btn", onclick: move |_| on_close.call(false), "Annuler" }
                }
                p { class: "error", "{msg}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(1689358921), (2023, 7, 14));
        assert_eq!(civil(1709164800), (2024, 2, 29));
        assert_eq!(date_label(1689358921), "14 juillet 2023 · 18:22");
    }
}
