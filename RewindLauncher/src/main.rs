#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::RefCell,
    collections::HashMap,
    error::Error,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
};

use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use walkdir::WalkDir;
use zip::ZipArchive;

slint::include_modules!();

/// Holds a single game's open ZIP archive and caches any images decoded from it
pub struct GameArchive {
    archive: ZipArchive<File>,
    image_cache: HashMap<String, Image>,
}

impl GameArchive {
    pub fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let file = File::open(path)?;
        let archive = ZipArchive::new(file)?;
        Ok(Self {
            archive,
            image_cache: HashMap::new(),
        })
    }

    pub fn get_image(&mut self, internal_path: &str) -> Option<Image> {
        if let Some(img) = self.image_cache.get(internal_path) {
            return Some(img.clone());
        }

        let mut zip_file = self.archive.by_name(internal_path).ok()?;
        let mut bytes = Vec::with_capacity(zip_file.size() as usize);
        zip_file.read_to_end(&mut bytes).ok()?;

        let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
        let (width, height) = decoded.dimensions();

        let buffer =
            SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(decoded.as_raw(), width, height);
        let slint_image = Image::from_rgba8(buffer);

        self.image_cache
            .insert(internal_path.to_string(), slint_image.clone());
        Some(slint_image)
    }

    pub fn get_text(&mut self, internal_path: &str) -> Option<String> {
        let mut zip_file = self.archive.by_name(internal_path).ok()?;
        let mut text = String::new();
        zip_file.read_to_string(&mut text).ok()?;
        Some(text)
    }

    /// Finds the first file inside a specific subfolder matching an extension
    pub fn find_file_in_folder(&self, folder: &str, ext: &str) -> Option<String> {
        let prefix = if folder.ends_with('/') {
            folder.to_string()
        } else {
            format!("{folder}/")
        };

        self.archive
            .file_names()
            .filter_map(Result::ok)
            .find(|name| {
                name.starts_with(&prefix)
                    && !name.ends_with('/')
                    && Path::new(name.as_ref())
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
            })
            .map(|s| s.to_string())
    }
}

/// Manages the entire library of game ZIPs
pub struct GameLibrary {
    /// Maps game index (matching the Slint VecModel row) to its open GameArchive
    archives: Vec<GameArchive>,
}

impl GameLibrary {
    pub fn scan_folder(root_dir: &Path) -> (Self, Vec<gamedata>) {
        let mut archives = Vec::new();
        let mut ui_items = Vec::new();

        let zip_paths: Vec<PathBuf> = WalkDir::new(root_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file()
                    && e.path()
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
            })
            .map(|e| e.into_path())
            .collect();

        for zip_path in zip_paths {
            let Ok(mut game_zip) = GameArchive::open(&zip_path) else {
                eprintln!("Skipping unreadable zip: {:?}", zip_path);
                continue;
            };

            // Verify this ZIP has your expected folder structure (e.g., an `icon/` or `meta/` folder)
            let icon_path = game_zip.find_file_in_folder("icon", "png");
            let has_meta = game_zip
                .archive
                .file_names()
                .filter_map(Result::ok)
                .any(|n| n.starts_with("meta/"));

            if icon_path.is_none() && !has_meta {
                continue; // Not a valid game archive
            }

            // Fallback name from the ZIP filename (e.g., "SuperGame.zip" -> "SuperGame")
            let fallback_name = zip_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown Game")
                .to_string();

            let name = game_zip
                .get_text("meta/name.txt")
                .map(|s| s.trim().to_string())
                .unwrap_or(fallback_name);

            let publisher = game_zip
                .get_text("meta/publisher.txt")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "Unknown Publisher".to_string());

            let icon = icon_path
                .and_then(|p| game_zip.get_image(&p))
                .unwrap_or_default();

            ui_items.push(gamedata {
                name: name.into(),
                publisher: publisher.into(),
                icon,
            });

            // Keep the archive handle alive at the exact same index as `ui_items`
            archives.push(game_zip);
        }

        (Self { archives }, ui_items)
    }

    /// Retrieve an image on-demand for a specific game by its list index
    pub fn get_game_image(&mut self, game_index: usize, internal_path: &str) -> Option<Image> {
        self.archives.get_mut(game_index)?.get_image(internal_path)
    }
}

/// `$XDG_DATA_HOME/Rewind` (default `~/.local/share/Rewind`), created if missing
fn games_dir() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    let dir = base.join("Rewind");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    // 1. Scan directory: each valid .zip becomes one entry in `ui_games` and `library`
    let (library, ui_games) = GameLibrary::scan_folder(&games_dir()?);
    let library = Rc::new(RefCell::new(library));

    // 2. Send the list of games to Slint
    let games_model = Rc::new(VecModel::from(ui_games));
    ui.set_games(ModelRc::from(games_model));

    // 3. Load extra images (like a banner/screenshot) lazily when a user clicks a game
    let ui_weak = ui.as_weak();
    let lib_clone = library.clone();
    ui.on_game_selected(move |index| {
        if let Some(ui) = ui_weak.upgrade() {
            if let Some(banner) = lib_clone
                .borrow_mut()
                .get_game_image(index as usize, "images/banner.png")
            {
                ui.set_selected_banner(banner);
            }
        }
    });

    ui.run()?;
    Ok(())
}
