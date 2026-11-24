//! Visages : détection YuNet (MIT) et empreinte SFace (Apache 2.0), modèles OpenCV Zoo exécutés en Rust pur (tract).
//! Regroupement par utilisateur : un visage rejoint la personne dont l'empreinte moyenne est la plus proche.
//! Données biométriques (nLPD art. 5) : désactivé par défaut, BITFLOW_FACES=1 pour l'activer ; tout reste sur le serveur.
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, OnceLock},
};

use image::{Rgb, RgbImage, imageops};
use tract_onnx::prelude::*;

use crate::App;

type Model = Arc<TypedRunnableModel>;

/// Entrée fixe du modèle YuNet 2023mar.
const DETECT: u32 = 640;
const MIN_SCORE: f32 = 0.9;
/// Similarité cosinus minimale pour rattacher un visage à une personne (seuil SFace : 0.363 ; plus strict pour ne pas fusionner deux personnes).
const SAME_PERSON: f32 = 0.42;
/// Visages plus petits que ça (px dans l'aperçu) : empreinte peu fiable, ignorés.
const MIN_FACE: f32 = 48.0;

struct Models {
    detect: Model,
    embed: Model,
}

static MODELS: OnceLock<Option<Mutex<Models>>> = OnceLock::new();

fn models(dir: &Path) -> Option<&'static Mutex<Models>> {
    MODELS
        .get_or_init(|| {
            let load = || -> anyhow::Result<Models> {
                let detect = tract_onnx::onnx()
                    .model_for_path(dir.join("yunet.onnx"))?
                    .with_input_fact(0, f32::fact([1, 3, DETECT as usize, DETECT as usize]).into())?
                    .into_optimized()?
                    .into_runnable()?;
                let embed = tract_onnx::onnx()
                    .model_for_path(dir.join("sface.onnx"))?
                    .with_input_fact(0, f32::fact([1, 3, 112, 112]).into())?
                    .into_optimized()?
                    .into_runnable()?;
                Ok(Models { detect, embed })
            };
            load().map_err(|e| tracing::error!("modèles de visages ({}) : {e:#}", dir.display())).ok().map(Mutex::new)
        })
        .as_ref()
}

/// Visage détecté, coordonnées en pixels de l'image analysée.
#[derive(Debug, Clone)]
pub struct Face {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
    /// Yeux, nez, coins de la bouche (ordre YuNet).
    pub marks: [(f32, f32); 5],
}

/// Détection sur l'image réduite à 640 px (bandes noires en bas/à droite), coordonnées ramenées à l'image d'origine.
fn detect(m: &Model, img: &RgbImage) -> anyhow::Result<Vec<Face>> {
    let scale = DETECT as f32 / img.width().max(img.height()) as f32;
    let small = imageops::resize(img, (img.width() as f32 * scale) as u32, (img.height() as f32 * scale) as u32, imageops::FilterType::Triangle);
    // YuNet : BGR, 0-255, NCHW.
    let input = tract_ndarray::Array4::<f32>::from_shape_fn((1, 3, DETECT as usize, DETECT as usize), |(_, c, y, x)| {
        small.get_pixel_checked(x as u32, y as u32).map_or(0.0, |p| p[2 - c] as f32)
    });
    let out = m.run(tvec!(Tensor::from(input).into()))?;
    let names = ["cls_8", "cls_16", "cls_32", "obj_8", "obj_16", "obj_32", "bbox_8", "bbox_16", "bbox_32", "kps_8", "kps_16", "kps_32"];
    let outputs: Vec<Vec<f32>> = out.iter().map(|t| t.to_plain_array_view::<f32>().map(|a| a.iter().copied().collect())).collect::<Result<_, _>>()?;
    anyhow::ensure!(outputs.len() == names.len(), "sorties YuNet inattendues");
    let mut faces = vec![];
    for (i, stride) in [8usize, 16, 32].into_iter().enumerate() {
        let (cls, obj, bbox, kps) = (&outputs[i], &outputs[3 + i], &outputs[6 + i], &outputs[9 + i]);
        let cols = DETECT as usize / stride;
        for idx in 0..cls.len() {
            let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
            if score < MIN_SCORE {
                continue;
            }
            let (r, c) = ((idx / cols) as f32, (idx % cols) as f32);
            let s = stride as f32;
            let (cx, cy) = ((c + bbox[idx * 4]) * s, (r + bbox[idx * 4 + 1]) * s);
            let (w, h) = (bbox[idx * 4 + 2].exp() * s, bbox[idx * 4 + 3].exp() * s);
            let marks = std::array::from_fn(|n| ((kps[idx * 10 + 2 * n] + c) * s / scale, (kps[idx * 10 + 2 * n + 1] + r) * s / scale));
            faces.push(Face { x: (cx - w / 2.0) / scale, y: (cy - h / 2.0) / scale, w: w / scale, h: h / scale, score, marks });
        }
    }
    Ok(nms(faces, 0.3))
}

fn iou(a: &Face, b: &Face) -> f32 {
    let (x1, y1) = (a.x.max(b.x), a.y.max(b.y));
    let (x2, y2) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    inter / (a.w * a.h + b.w * b.h - inter)
}

fn nms(mut faces: Vec<Face>, threshold: f32) -> Vec<Face> {
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Face> = vec![];
    for f in faces {
        if kept.iter().all(|k| iou(k, &f) < threshold) {
            kept.push(f);
        }
    }
    kept
}

/// Points de référence ArcFace pour un visage aligné de 112×112 (ceux d'OpenCV FaceRecognizerSF::alignCrop).
const REFERENCE: [(f32, f32); 5] = [(38.2946, 51.6963), (73.5318, 51.5014), (56.0252, 71.7366), (41.5493, 92.3655), (70.7299, 92.2041)];

/// Similitude (échelle + rotation + translation) aux moindres carrés envoyant `src` sur `dst` : [a, -b, tx; b, a, ty].
fn similarity(src: &[(f32, f32); 5], dst: &[(f32, f32); 5]) -> [f32; 4] {
    let mean = |p: &[(f32, f32); 5]| p.iter().fold((0.0, 0.0), |m, q| (m.0 + q.0 / 5.0, m.1 + q.1 / 5.0));
    let (ms, md) = (mean(src), mean(dst));
    let (mut a, mut b, mut norm) = (0.0, 0.0, 0.0);
    for (s, d) in src.iter().zip(dst) {
        let (sx, sy, dx, dy) = (s.0 - ms.0, s.1 - ms.1, d.0 - md.0, d.1 - md.1);
        a += sx * dx + sy * dy;
        b += sx * dy - sy * dx;
        norm += sx * sx + sy * sy;
    }
    let (a, b) = (a / norm, b / norm);
    [a, b, md.0 - (a * ms.0 - b * ms.1), md.1 - (b * ms.0 + a * ms.1)]
}

/// Visage redressé 112×112 : chaque pixel de sortie est échantillonné (bilinéaire) par la transformation inverse.
fn align(img: &RgbImage, marks: &[(f32, f32); 5]) -> RgbImage {
    let [a, b, tx, ty] = similarity(marks, &REFERENCE);
    let det = a * a + b * b;
    RgbImage::from_fn(112, 112, |u, v| {
        let (u, v) = (u as f32 - tx, v as f32 - ty);
        let (x, y) = ((a * u + b * v) / det, (-b * u + a * v) / det);
        imageops::interpolate_bilinear(img, x, y).unwrap_or(Rgb([0, 0, 0]))
    })
}

/// Empreinte SFace normalisée (128 valeurs) : entrée RGB 0-255.
fn embed(m: &Model, face: &RgbImage) -> anyhow::Result<Vec<f32>> {
    let input = tract_ndarray::Array4::<f32>::from_shape_fn((1, 3, 112, 112), |(_, c, y, x)| face.get_pixel(x as u32, y as u32)[c] as f32);
    let out = m.run(tvec!(Tensor::from(input).into()))?;
    let mut v: Vec<f32> = out[0].to_plain_array_view::<f32>()?.iter().copied().collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter_mut().for_each(|x| *x /= n);
    Ok(v)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>() / (n(a) * n(b))
}

/// Visages d'une image et leurs empreintes.
pub fn analyze(dir: &Path, img: &RgbImage) -> anyhow::Result<Vec<(Face, Vec<f32>)>> {
    let m = models(dir).ok_or_else(|| anyhow::anyhow!("modèles indisponibles"))?.lock().unwrap();
    let faces = detect(&m.detect, img)?;
    faces.into_iter().filter(|f| f.w.min(f.h) >= MIN_FACE).map(|f| Ok((f.clone(), embed(&m.embed, &align(img, &f.marks))?))).collect()
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Photos pas encore analysées (une à la fois : le calcul occupe un cœur), puis regroupement.
pub async fn run(app: &App) -> anyhow::Result<()> {
    if !app.cfg.faces {
        return Ok(());
    }
    let todo: Vec<(i64, i64, String)> =
        sqlx::query_as("SELECT id, owner, path FROM photos WHERE faces = 0 AND video = 0 AND thumb = 1 ORDER BY taken DESC").fetch_all(&app.db).await?;
    if todo.is_empty() {
        return Ok(());
    }
    tracing::info!("visages : {} photos à analyser", todo.len());
    // Empreinte moyenne (somme) et nombre de visages de chaque personne, par propriétaire.
    let mut people: HashMap<i64, Vec<(i64, Vec<f32>)>> = HashMap::new();
    let rows: Vec<(i64, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT pe.owner, f.person_id, f.embedding FROM faces f JOIN people pe ON pe.id = f.person_id",
    )
    .fetch_all(&app.db)
    .await?;
    for (owner, person, emb) in rows {
        let list = people.entry(owner).or_default();
        let emb = from_blob(&emb);
        match list.iter_mut().find(|(p, _)| *p == person) {
            Some((_, sum)) => sum.iter_mut().zip(&emb).for_each(|(s, e)| *s += e),
            None => list.push((person, emb)),
        }
    }
    let models_dir = app.cfg.models.clone();
    for (photo, owner, path) in todo {
        let preview = crate::photos::preview(app, photo, &path).await;
        let dir = models_dir.clone();
        let found = match preview {
            Ok(file) => tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let img = image::open(&file)?.to_rgb8();
                Ok((analyze(&dir, &img)?, img.width() as f32, img.height() as f32))
            })
            .await?,
            Err(e) => Err(e),
        };
        let Ok((faces, w, h)) = found.inspect_err(|e| tracing::warn!("visages {path} : {e:#}")) else {
            sqlx::query("UPDATE photos SET faces = 2 WHERE id = ?").bind(photo).execute(&app.db).await?;
            continue;
        };
        let mut tx = app.db.begin().await?;
        sqlx::query("DELETE FROM faces WHERE photo_id = ?").bind(photo).execute(&mut *tx).await?;
        let list = people.entry(owner).or_default();
        for (f, emb) in faces {
            let best = list.iter_mut().map(|(p, sum)| (cosine(sum, &emb), p, sum)).max_by(|a, b| a.0.total_cmp(&b.0));
            let person = match best {
                Some((sim, p, sum)) if sim >= SAME_PERSON => {
                    sum.iter_mut().zip(&emb).for_each(|(s, e)| *s += e);
                    *p
                }
                _ => {
                    let id: i64 = sqlx::query_scalar("INSERT INTO people(owner) VALUES (?) RETURNING id").bind(owner).fetch_one(&mut *tx).await?;
                    list.push((id, emb.clone()));
                    id
                }
            };
            sqlx::query("INSERT INTO faces(photo_id, person_id, x, y, w, h, score, embedding) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(photo).bind(person).bind(f.x / w).bind(f.y / h).bind(f.w / w).bind(f.h / h).bind(f.score).bind(to_blob(&emb))
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE photos SET faces = 1 WHERE id = ?").bind(photo).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Vignette carrée d'un visage (marge de 40 %) découpée dans l'aperçu.
pub fn crop(preview: &Path, x: f32, y: f32, w: f32, h: f32) -> anyhow::Result<RgbImage> {
    let img = image::open(preview)?.to_rgb8();
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let side = (w * iw).max(h * ih) * 1.4;
    let (cx, cy) = ((x + w / 2.0) * iw, (y + h / 2.0) * ih);
    let x0 = (cx - side / 2.0).clamp(0.0, (iw - side).max(0.0));
    let y0 = (cy - side / 2.0).clamp(0.0, (ih - side).max(0.0));
    let side = side.min(iw).min(ih) as u32;
    let face = imageops::crop_imm(&img, x0 as u32, y0 as u32, side, side).to_image();
    Ok(imageops::resize(&face, 160, 160, imageops::FilterType::Triangle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_identity_and_scale() {
        let [a, b, tx, ty] = similarity(&REFERENCE, &REFERENCE);
        assert!((a - 1.0).abs() < 1e-4 && b.abs() < 1e-4 && tx.abs() < 1e-3 && ty.abs() < 1e-3);
        let doubled = REFERENCE.map(|(x, y)| (x * 2.0 + 10.0, y * 2.0 - 5.0));
        let [a, b, ..] = similarity(&doubled, &REFERENCE);
        assert!((a - 0.5).abs() < 1e-4 && b.abs() < 1e-4);
    }

    /// Portraits réels : BITFLOW_TEST_FACES=<dossier avec yunet.onnx, sface.onnx et des .jpg>.
    #[test]
    #[ignore]
    fn real_faces() {
        let dir = std::path::PathBuf::from(std::env::var("BITFLOW_TEST_FACES").unwrap());
        let mut all = vec![];
        for e in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "jpg")) {
            let img = image::open(e.path()).unwrap().to_rgb8();
            let img = imageops::resize(&img, 1600, 1600 * img.height() / img.width(), imageops::FilterType::Triangle);
            let t = std::time::Instant::now();
            let faces = analyze(&dir, &img).unwrap();
            println!("{:?} : {} visage(s) en {:?} {:?}", e.file_name(), faces.len(), t.elapsed(), faces.first().map(|f| (f.0.x, f.0.y, f.0.w, f.0.score)));
            all.extend(faces.into_iter().map(|f| (e.file_name().to_string_lossy().into_owned(), f.1)));
        }
        for (i, (na, a)) in all.iter().enumerate() {
            for (nb, b) in &all[i + 1..] {
                println!("{na} ~ {nb} : {:.3}", cosine(a, b));
            }
        }
    }
}
