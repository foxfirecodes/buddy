//! Steam artwork belongs to the local GUI. Game launches never start this worker.
use eframe::egui::{self, ColorImage, Context, Rect, TextureHandle, pos2};
use image::ImageReader;
use std::{
    collections::{HashMap, HashSet},
    io::{Cursor, Read},
    path::Path,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

type ResultImage = (u32, Option<ColorImage>);
const MAX_COVERS: usize = 48;

pub(super) struct Artwork {
    jobs: SyncSender<(u32, Context)>,
    results: Receiver<ResultImage>,
    pending: HashSet<u32>,
    covers: HashMap<u32, (Option<TextureHandle>, u64)>,
}

impl Artwork {
    pub(super) fn new() -> Self {
        let (jobs, requests) = mpsc::sync_channel::<(u32, Context)>(8);
        let (send, results) = mpsc::sync_channel(8);
        thread::spawn(move || {
            let mut roots = None;
            for (id, ctx) in requests {
                let roots = roots.get_or_insert_with(|| {
                    crate::local::steam_installs()
                        .into_iter()
                        .map(|s| s.path().join("appcache/librarycache"))
                        .collect::<Vec<_>>()
                });
                let image = roots.iter().find_map(|root| local_cover(root, id));
                if send.send((id, image)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self {
            jobs,
            results,
            pending: HashSet::new(),
            covers: HashMap::new(),
        }
    }

    pub(super) fn poll(&mut self, ctx: &Context) {
        for (id, image) in self.results.try_iter() {
            self.pending.remove(&id);
            if self.covers.len() >= MAX_COVERS
                && let Some((&old, _)) =
                    self.covers.iter().min_by_key(|(id, entry)| (entry.1, **id))
            {
                self.covers.remove(&old);
            }
            let texture = image.map(|im| {
                ctx.load_texture(format!("steam-hero-{id}"), im, egui::TextureOptions::LINEAR)
            });
            self.covers.insert(id, (texture, ctx.cumulative_frame_nr()));
        }
    }

    fn texture(&mut self, ctx: &Context, game: &str) -> Option<&TextureHandle> {
        let id = game.strip_prefix("steam-")?.parse::<u32>().ok()?;
        if let Some((texture, last)) = self.covers.get_mut(&id) {
            *last = ctx.cumulative_frame_nr();
            return texture.as_ref();
        }
        if !self.pending.contains(&id) && self.jobs.try_send((id, ctx.clone())).is_ok() {
            self.pending.insert(id);
        }
        None
    }

    pub(super) fn paint(&mut self, ui: &egui::Ui, rect: Rect, game: &str) {
        let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        if let Some(texture) = self.texture(ui.ctx(), game) {
            let size = texture.size_vec2();
            let scale = (rect.width() / size.x).max(rect.height() / size.y);
            let visible = rect.size() / (size * scale);
            let uv = Rect::from_center_size(pos2(0.5, 0.5), visible);
            painter.image(texture.id(), rect, uv, egui::Color32::WHITE);
        } else {
            crate::phase::disk(&painter, rect, game);
        }
        // Scrim keeps both lines readable over bright or high-contrast Steam art.
        let mut mesh = egui::Mesh::default();
        for (p, alpha) in [
            (rect.left_top(), 168),
            (rect.right_top(), 18),
            (rect.right_bottom(), 85),
            (rect.left_bottom(), 230),
        ] {
            mesh.colored_vertex(p, egui::Color32::from_black_alpha(alpha));
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(mesh);
    }
}

fn local_cover(cache: &Path, id: u32) -> Option<ColorImage> {
    let dir = cache.join(id.to_string());
    let direct = dir.join("library_hero.jpg");
    let legacy = cache.join(format!("{id}_library_hero.jpg"));
    if let Some(image) = decode(&direct).or_else(|| decode(&legacy)) {
        return Some(image);
    }
    // Some Steam versions group the artwork under a content hash in the app directory.
    for entry in std::fs::read_dir(&dir).ok()?.flatten().take(128) {
        let name = entry.file_name();
        let hash = name.to_str().unwrap_or("");
        if hash.len() == 40
            && hash.bytes().all(|c| c.is_ascii_hexdigit())
            && entry.file_type().is_ok_and(|t| t.is_dir())
            && let Some(image) = decode(&entry.path().join("library_hero.jpg"))
        {
            return Some(image);
        }
    }
    None
}

fn decode(path: &Path) -> Option<ColorImage> {
    const MAX_BYTES: u64 = 12 * 1024 * 1024;
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let mut options = std::fs::File::options();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            (rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOFOLLOW).bits() as i32,
        );
    }
    let file = options.open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let image = image
        .thumbnail(image.width().min(720), image.height().min(240))
        .into_rgb8();
    Some(ColorImage::from_rgb(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_named_steam_heroes_and_skips_missing_and_corrupt_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        let app = cache.join("42");
        let nested = app.join("a".repeat(40));
        std::fs::create_dir_all(&nested).unwrap();
        let first = app.join("library_hero.jpg");
        let legacy = cache.join("42_library_hero.jpg");
        let hashed = nested.join("library_hero.jpg");
        assert!(local_cover(cache, 42).is_none());
        image::RgbImage::new(80, 30).save(&first).unwrap();
        assert_eq!(local_cover(cache, 42).unwrap().size, [80, 30]);
        std::fs::rename(&first, &legacy).unwrap();
        assert!(local_cover(cache, 42).is_some());
        std::fs::rename(&legacy, &hashed).unwrap();
        std::fs::write(&first, b"invalid JPEG").unwrap();
        assert!(local_cover(cache, 42).is_some());
        assert!(local_cover(cache, 43).is_none());
    }
}
