//! Playlists : liste, page d'une playlist (lecture, renommage, ordre) et fenêtre « Ajouter à une playlist ».
use bitflow_shared::{Playlist, PlaylistEdit, PlaylistTracks};
use dioxus::prelude::*;

use crate::{
    MusicTabs, Route, api, clock,
    icons::{self as ic, icon},
    music::{Queue, shuffled},
};

#[component]
pub fn Playlists() -> Element {
    let list = use_resource(|| api::get::<Vec<Playlist>>("/api/playlists"));
    rsx! {
        MusicTabs { active: 2 }
        h1 { "Playlists" }
        NewPlaylist { items: Vec::new(), on_done: move |id| { navigator().push(Route::PlaylistView { id }); } }
        match &*list.read_unchecked() {
            Some(Ok(v)) if v.is_empty() => rsx! {
                p { class: "muted", "Aucune playlist pour l'instant. Créez-en une ci-dessus, ou depuis un album avec le bouton « Ajouter à une playlist »." }
            },
            Some(Ok(v)) => rsx! {
                div { class: "grid",
                    for p in v.iter().cloned() {
                        Link { key: "{p.id}", class: "card square", to: Route::PlaylistView { id: p.id },
                            div { class: "thumb",
                                if let Some(src) = api::img(&p.cover, "poster") {
                                    img { src, alt: "" }
                                } else {
                                    div { class: "noposter", {icon(ic::QUEUE)} }
                                }
                            }
                            div { class: "meta",
                                span { "{p.name}" }
                                small { "{p.count} titres · {clock(p.duration)}" }
                            }
                        }
                    }
                }
            },
            Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
            None => rsx! { div { class: "loading" } },
        }
    }
}

/// Champ « nouvelle playlist » : la crée avec `items` et renvoie son identifiant.
#[component]
fn NewPlaylist(items: Vec<i64>, on_done: EventHandler<i64>) -> Element {
    let mut name = use_signal(String::new);
    let mut error = use_signal(String::new);
    rsx! {
        form {
            class: "inline-form",
            onsubmit: move |e| {
                e.prevent_default();
                let body = PlaylistEdit { name: name(), items: items.clone() };
                async move {
                    match api::post::<i64>("/api/playlists", &body).await {
                        Ok(id) => on_done.call(id),
                        Err(e) => error.set(e),
                    }
                }
            },
            input { placeholder: "Nom de la nouvelle playlist", value: "{name}", maxlength: 200, required: true, oninput: move |e| name.set(e.value()) }
            button { class: "btn primary", r#type: "submit", {icon(ic::ADD)} "Créer" }
        }
        if !error().is_empty() {
            p { class: "error", "{error}" }
        }
    }
}

#[component]
pub fn PlaylistView(id: i64) -> Element {
    let mut queue = use_context::<Signal<Queue>>();
    let loaded = use_resource(use_reactive!(|id| async move { api::get::<PlaylistTracks>(&format!("/api/playlists/{id}")).await }));
    // Copie locale modifiée tout de suite ; le serveur reçoit la liste complète à chaque changement.
    let mut pl = use_signal(|| None::<PlaylistTracks>);
    use_effect(move || {
        if let Some(Ok(p)) = &*loaded.read() {
            pl.set(Some(p.clone()));
        }
    });
    let mut renaming = use_signal(|| None::<String>);
    let mut error = use_signal(String::new);
    let mut commit = move |next: PlaylistTracks| {
        let body = PlaylistEdit { name: next.name.clone(), items: next.tracks.iter().map(|t| t.id).collect() };
        pl.set(Some(next));
        spawn(async move {
            if let Err(e) = api::put::<()>(&format!("/api/playlists/{id}"), &body).await {
                error.set(e);
            }
        });
    };
    let mut edit = move |f: &dyn Fn(&mut PlaylistTracks)| {
        if let Some(mut p) = pl() {
            f(&mut p);
            commit(p);
        }
    };

    let Some(p) = pl() else {
        return match &*loaded.read_unchecked() {
            Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
            _ => rsx! { div { class: "loading" } },
        };
    };
    let total: f64 = p.tracks.iter().filter_map(|t| t.duration).sum();
    let cover = p.tracks.iter().find_map(|t| api::img(&t.poster, "poster"));
    let playing = queue.read().current().map(|t| t.id);
    let n = p.tracks.len();
    let (all, all2, name) = (p.tracks.clone(), p.tracks.clone(), p.name.clone());
    rsx! {
        MusicTabs { active: 2 }
        div { class: "album-head",
            if let Some(src) = cover {
                img { class: "cover", src, alt: "" }
            } else {
                div { class: "cover noposter", {icon(ic::QUEUE)} }
            }
            div {
                p { class: "muted", "Playlist" }
                if let Some(v) = renaming() {
                    form {
                        class: "inline-form",
                        onsubmit: move |e| {
                            e.prevent_default();
                            let v = renaming().unwrap_or_default();
                            if !v.trim().is_empty() {
                                edit(&|p| p.name = v.trim().into());
                            }
                            renaming.set(None);
                        },
                        input { value: "{v}", maxlength: 200, autofocus: true, oninput: move |e| renaming.set(Some(e.value())) }
                        button { class: "btn primary", r#type: "submit", "Enregistrer" }
                        button { class: "btn", r#type: "button", onclick: move |_| renaming.set(None), "Annuler" }
                    }
                } else {
                    h1 { "{p.name}" }
                }
                p { class: "facts", span { "{n} titres · {clock(total)}" } }
                div { class: "actions",
                    button { class: "btn primary", disabled: n == 0, onclick: move |_| queue.set(Queue { tracks: all.clone(), index: 0 }), {icon(ic::PLAY)} "Lecture" }
                    button { class: "btn", disabled: n == 0, onclick: move |_| queue.set(Queue { tracks: shuffled(all2.clone()), index: 0 }), {icon(ic::SHUFFLE)} "Aléatoire" }
                    button { class: "btn", onclick: move |_| renaming.set(Some(name.clone())), {icon(ic::EDIT)} "Renommer" }
                    button {
                        class: "btn",
                        onclick: move |_| async move {
                            let ok = web_sys::window().and_then(|w| w.confirm_with_message("Supprimer cette playlist ? Les titres restent dans la bibliothèque.").ok());
                            if ok == Some(true) {
                                match api::delete(&format!("/api/playlists/{id}")).await {
                                    Ok(()) => { navigator().replace(Route::Playlists {}); }
                                    Err(e) => error.set(e),
                                }
                            }
                        },
                        {icon(ic::DELETE)}
                        "Supprimer"
                    }
                }
            }
        }
        if !error().is_empty() {
            p { class: "error", "{error}" }
        }
        if n == 0 {
            p { class: "muted", "Playlist vide : ajoutez des titres depuis un album avec le bouton « Ajouter à une playlist »." }
        }
        ol { class: "tracklist",
            for (i, t) in p.tracks.iter().cloned().enumerate() {
                li {
                    key: "{i}-{t.id}",
                    class: if playing == Some(t.id) { "track pl playing" } else { "track pl" },
                    ondoubleclick: {
                        let all = p.tracks.clone();
                        move |_| queue.set(Queue { tracks: all.clone(), index: i })
                    },
                    button {
                        class: "num",
                        title: "Lire",
                        onclick: {
                            let all = p.tracks.clone();
                            move |_| queue.set(Queue { tracks: all.clone(), index: i })
                        },
                        span { class: "n", "{i + 1}" }
                        span { class: "p", "▶" }
                    }
                    div { class: "t-title",
                        span { "{t.title}" }
                        small { "{t.artist.clone().unwrap_or_default()}" }
                    }
                    div { class: "t-actions",
                        button { class: "ghost", title: "Monter", disabled: i == 0, onclick: move |_| edit(&|p| p.tracks.swap(i, i - 1)), {icon(ic::UP)} }
                        button { class: "ghost", title: "Descendre", disabled: i + 1 == n, onclick: move |_| edit(&|p| p.tracks.swap(i, i + 1)), {icon(ic::DOWN)} }
                        button { class: "ghost", title: "Retirer de la playlist", onclick: move |_| edit(&|p| { p.tracks.remove(i); }), {icon(ic::CLOSE)} }
                    }
                    span { class: "dur", "{clock(t.duration.unwrap_or(0.0))}" }
                }
            }
        }
    }
}

/// Fenêtre : choisir une playlist existante ou en créer une avec `items`.
#[component]
pub fn AddToPlaylist(items: Vec<i64>, on_close: EventHandler<()>) -> Element {
    let list = use_resource(|| api::get::<Vec<Playlist>>("/api/playlists"));
    let mut error = use_signal(String::new);
    let n = items.len();
    let items2 = items.clone();
    rsx! {
        div { class: "dialog-bg", onclick: move |_| on_close.call(()),
            section { class: "panel dialog", onclick: move |e| e.stop_propagation(),
                h2 {
                    if n == 1 { "Ajouter à une playlist" } else { "Ajouter {n} titres à une playlist" }
                }
                div { class: "pl-pick",
                    match &*list.read_unchecked() {
                        Some(Ok(v)) => rsx! {
                            for p in v.iter().cloned() {
                                button {
                                    key: "{p.id}",
                                    class: "pl-opt",
                                    onclick: {
                                        let items = items.clone();
                                        move |_| {
                                            let items = items.clone();
                                            async move {
                                                match api::post::<()>(&format!("/api/playlists/{}/tracks", p.id), &items).await {
                                                    Ok(()) => on_close.call(()),
                                                    Err(e) => error.set(e),
                                                }
                                            }
                                        }
                                    },
                                    {icon(ic::QUEUE)}
                                    span { "{p.name}" }
                                    small { "{p.count} titres" }
                                }
                            }
                        },
                        Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
                        None => rsx! { div { class: "loading" } },
                    }
                }
                NewPlaylist { items: items2, on_done: move |_| on_close.call(()) }
                if !error().is_empty() {
                    p { class: "error", "{error}" }
                }
                button { class: "btn", onclick: move |_| on_close.call(()), "Annuler" }
            }
        }
    }
}
