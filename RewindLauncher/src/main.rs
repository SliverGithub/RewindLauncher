#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::BTreeMap,
    error::Error,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use mlua::Lua;
use rfd::FileDialog;
use serde::Deserialize;
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use walkdir::WalkDir;
use zip::ZipArchive;

slint::include_modules!();

#[derive(Default, Deserialize)]
struct Meta {
    name: Option<String>,
    publisher: Option<String>,
    version: Option<i32>,
    /// URL of a small yaml naming the current version and its zip.
    update: Option<String>,
    #[serde(default)]
    modlist: Vec<ModItem>,
}

/// One mod list entry: a plain mod, or a named group of nested mods.
#[derive(Deserialize)]
#[serde(untagged)]
enum ModItem {
    Mod(String),
    Group(BTreeMap<String, Vec<ModItem>>),
}

/// Flattens the tree into rows, keeping each row's indent depth.
fn modlist_rows(items: &[ModItem]) -> Vec<modentry> {
    fn walk(items: &[ModItem], depth: i32, out: &mut Vec<modentry>) {
        for item in items {
            match item {
                ModItem::Mod(name) => out.push(modentry {
                    name: name.as_str().into(),
                    depth,
                }),
                ModItem::Group(group) => {
                    for (name, mods) in group {
                        out.push(modentry {
                            name: name.as_str().into(),
                            depth,
                        });
                        walk(mods, depth + 1, out);
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    walk(items, 0, &mut out);
    out
}

/// Opens one game zip, reads `meta.yaml` and its first `icon/*.jpg`
fn load_game(zip_path: &Path) -> Option<gamedata> {
    let mut zip = ZipArchive::new(File::open(zip_path).ok()?).ok()?;

    let mut meta_yaml = String::new();
    zip.by_name("meta.yaml")
        .ok()?
        .read_to_string(&mut meta_yaml)
        .ok()?;

    let meta: Meta = rust_yaml::from_str(&meta_yaml).unwrap_or_default();
    let modlist = if meta.modlist.is_empty() {
        vec![modentry {
            name: "This mod package has no mod list in it's meta.yaml file".into(),
            depth: 0,
        }]
    } else {
        modlist_rows(&meta.modlist)
    };
    let name = meta
        .name
        .or_else(|| Some(zip_path.file_stem()?.to_str()?.to_string()))?;
    let publisher = meta.publisher.unwrap_or_else(|| "Unknown Publisher".into());
    let script = zip
        .file_names()
        .filter_map(Result::ok)
        .find(|n| n.eq_ignore_ascii_case("script.lua"))
        .map(|n| n.to_string())
        .unwrap_or_default();
    let ver = meta.version.unwrap_or_else(|| 0.into());

    let icon_name = zip
        .file_names()
        .filter_map(Result::ok)
        .find(|n| n.eq_ignore_ascii_case("icon.jpeg"))
        .map(|n| n.to_string());

    let icon = icon_name
        .and_then(|internal| {
            let mut bytes = Vec::new();
            zip.by_name(&internal).ok()?.read_to_end(&mut bytes).ok()?;
            let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
            let (width, height) = decoded.dimensions();
            Some(Image::from_rgba8(
                SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(decoded.as_raw(), width, height),
            ))
        })
        .unwrap_or_default();

    Some(gamedata {
        name: name.into(),
        publisher: publisher.into(),
        icon,
        script: script.into(),
        modlist: ModelRc::from(Rc::new(VecModel::from(modlist))),
        version: ver.into(),
    })
}

fn scan_folder(root: &Path) -> Vec<(PathBuf, gamedata)> {
    WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
        })
        .filter_map(|e| load_game(e.path()).map(|game| (e.path().to_path_buf(), game)))
        .collect()
}

/// `$XDG_DATA_HOME/Rewind` (default `~/.local/share/Rewind`), created if missing
fn games_dir() -> Result<PathBuf, std::io::Error> {
    let dir = if std::env::consts::OS == "windows" {
        let home = std::env::var_os("USERPROFILE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE").unwrap_or_default();
                let path = std::env::var_os("HOMEPATH").unwrap_or_default();
                PathBuf::from(drive).join(path)
            });
        home.join("rewind")
    } else {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".local/share")
            });
        data_home.join("rewind")
    };

    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// What a package's `update` URL serves: the current version and a link to its zip.
#[derive(Deserialize)]
struct UpdateInfo {
    version: i32,
    /// Full URL of the zip.
    package: String,
}

/// `meta.yaml` of an installed package, read without decoding its icon.
fn read_meta(zip_path: &Path) -> Option<Meta> {
    let mut zip = ZipArchive::new(File::open(zip_path).ok()?).ok()?;
    let mut yaml = String::new();
    zip.by_name("meta.yaml").ok()?.read_to_string(&mut yaml).ok()?;
    rust_yaml::from_str(&yaml).ok()
}

fn fetch_update(url: &str) -> Result<UpdateInfo, Box<dyn Error>> {
    let body = ureq::get(url).call()?.body_mut().read_to_string()?;
    Ok(rust_yaml::from_str(&body)?)
}

fn fetch_package(url: &str, part: &Path) -> Result<(), Box<dyn Error>> {
    let mut res = ureq::get(url).call()?;
    let mut file = File::create(part)?;
    std::io::copy(&mut res.body_mut().as_reader(), &mut file)?;
    file.sync_all()?;
    Ok(())
}

/// Version of a downloaded package file, but only if it is a real package: a zip
/// whose `meta.yaml` parses, names `name` and carries a version. Anything else
/// (error page, truncated download) must never be written.
fn package_version(zip_path: &Path, name: &str) -> Option<i32> {
    let meta = read_meta(zip_path)?;
    if meta.name.as_deref() != Some(name) {
        return None;
    }
    meta.version
}

/// Makes the renames durable. POSIX needs the directory synced; Windows has no
/// directory handle, the rename is already journaled there.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_: &Path) {}

/// Swaps the downloaded `.part` in, keeping the old file until the new one is in place.
fn replace_zip(zip_path: &Path, part: &Path) -> std::io::Result<()> {
    let old = zip_path.with_extension("zip.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(zip_path, &old)?;
    std::fs::rename(part, zip_path)?;
    if let Some(dir) = zip_path.parent() {
        sync_dir(dir);
    }
    let _ = std::fs::remove_file(&old);
    Ok(())
}

/// Undoes a swap that a crash interrupted: a `.old` next to a missing zip goes
/// back, one next to a live zip is just leftover litter.
fn recover_packages(root: &Path) {
    for entry in WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let file = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !file.to_ascii_lowercase().ends_with(".zip.old") {
            continue;
        }
        let zip = path.with_file_name(&file[..file.len() - 4]);
        if zip.exists() {
            let _ = std::fs::remove_file(path);
        } else {
            let _ = std::fs::rename(path, &zip);
        }
    }
}

/// Fetches each package's own update yaml and replaces the zip only when that
/// yaml names a newer version. Returns the rows that changed.
fn update_packages(sources: &[PathBuf]) -> Vec<(usize, PathBuf)> {
    let mut updated = Vec::new();
    for (row, path) in sources.iter().enumerate() {
        let Some(meta) = read_meta(path) else {
            continue;
        };
        let (Some(name), Some(version), Some(url)) = (meta.name, meta.version, meta.update) else {
            continue;
        };
        let Ok(info) = fetch_update(&url) else {
            continue;
        };
        if info.version <= version {
            continue;
        }
        let part = path.with_extension("zip.part");
        let replaced = fetch_package(&info.package, &part).is_ok()
            && package_version(&part, &name).is_some_and(|new| new > version)
            && replace_zip(path, &part).is_ok();
        if replaced {
            updated.push((row, path.clone()));
        } else {
            let _ = std::fs::remove_file(&part);
        }
    }
    updated
}

/// Reads `script.lua` out of the package and runs it with a `game` table
/// (name, publisher, package path) available to the script.
/// shortcut: opens the folder dialog from a worker thread; macOS needs it on the main thread
fn execute_script(
    zip_path: &Path,
    script_name: &str,
    game_name: &str,
) -> Result<(), Box<dyn Error>> {
    if script_name.is_empty() {
        return Err(format!("{} has no script.lua", zip_path.display()).into());
    }

    let mut zip = ZipArchive::new(File::open(zip_path)?)?;
    let mut source = String::new();
    zip.by_name(script_name)?.read_to_string(&mut source)?;

    let lua = Lua::new();
    let game_table = lua.create_table()?;
    let gamepath = FileDialog::new()
        .set_title(format!("Pick the game folder for {game_name}"))
        .pick_folder();
    game_table.set("package", zip_path.to_string_lossy().as_ref())?;
    game_table.set("game_path", gamepath)?;
    let pkg_path = zip_path.to_path_buf();
    let read_file_fn = lua.create_function(move |lua, entry_name: String| {
        let mut archive = match File::open(&pkg_path)
            .and_then(|f| ZipArchive::new(f).map_err(std::io::Error::other))
        {
            Ok(a) => a,
            Err(e) => return Ok((mlua::Value::Nil, Some(e.to_string()))),
        };
        let mut entry = match archive.by_name(&entry_name) {
            Ok(entry) => entry,
            Err(e) => return Ok((mlua::Value::Nil, Some(e.to_string()))),
        };
        let mut buf = Vec::new();
        if let Err(e) = entry.read_to_end(&mut buf) {
            return Ok((mlua::Value::Nil, Some(e.to_string())));
        }
        let bytes = lua.create_string(&buf)?;
        Ok((mlua::Value::String(bytes), None))
    })?;
    game_table.set("read_file", read_file_fn)?;
    lua.globals().set("game", game_table)?;
    lua.load(&source).set_name(script_name).exec()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    let dir = games_dir()?;
    recover_packages(&dir);
    let found = scan_folder(&dir);
    let sources = Rc::new(
        found
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>(),
    );
    let games = Rc::new(VecModel::from(
        found.into_iter().map(|(_, game)| game).collect::<Vec<_>>(),
    ));
    ui.set_games(ModelRc::from(games.clone()));
    let busy = Arc::new(AtomicBool::new(false));
    let ui_handle = ui.as_weak();

    let update_ui = ui_handle.clone();
    let update_sources = (*sources).clone();
    std::thread::spawn(move || {
        for (row, path) in update_packages(&update_sources) {
            let update_ui = update_ui.clone();
            let _ = update_ui.upgrade_in_event_loop(move |ui| {
                if let Some(game) = load_game(&path) {
                    ui.get_games().set_row_data(row, game);
                }
            });
        }
    });
    ui.on_patch_clicked(move |idx| {
        if busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let idx = idx as usize;
        let (Some(game), Some(zip)) = (games.row_data(idx), sources.get(idx).cloned()) else {
            busy.store(false, Ordering::SeqCst);
            return;
        };
        let name = game.name.to_string();
        let script = game.script.to_string();
        if let Some(ui) = ui_handle.upgrade() {
            ui.set_patching(true);
        }
        let ui_handle = ui_handle.clone();
        let busy = busy.clone();
        std::thread::spawn(move || {
            if let Err(e) = execute_script(&zip, &script, &name) {
                eprintln!("patch failed for {name}: {e}");
            }
            let _ = ui_handle.upgrade_in_event_loop(|ui| ui.set_patching(false));
            busy.store(false, Ordering::SeqCst);
        });
    });

    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rewind-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rows(meta: &Meta) -> Vec<(String, i32)> {
        modlist_rows(&meta.modlist)
            .into_iter()
            .map(|row| (row.name.to_string(), row.depth))
            .collect()
    }

    #[test]
    fn example_package() {
        let meta: Meta =
            rust_yaml::from_str(include_str!("../../ExamplePackage/Package/meta.yaml")).unwrap();
        assert_eq!(meta.name.as_deref(), Some("Not GTA6"));
        assert_eq!(
            rows(&meta),
            [
                ("some modloader idk".to_string(), 0),
                ("Graphics".to_string(), 0),
                ("some graphics patch".to_string(), 1),
                ("some texture pack".to_string(), 1)
            ]
        );
    }

    #[test]
    fn three_levels() {
        let meta: Meta = rust_yaml::from_str(
            "modlist:\n  - Graphics:\n      - Textures:\n          - hd pack\n",
        )
        .unwrap();
        assert_eq!(
            rows(&meta),
            [
                ("Graphics".to_string(), 0),
                ("Textures".to_string(), 1),
                ("hd pack".to_string(), 2)
            ]
        );
    }

    #[test]
    fn validity_guard() {
        use std::io::{Cursor, Write};

        let dir = temp_dir("guard");
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            w.start_file("meta.yaml", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"name: Game\nversion: 2\n").unwrap();
            w.finish().unwrap();
        }
        let path = dir.join("game.zip");
        std::fs::write(&path, &buf).unwrap();
        assert_eq!(package_version(&path, "Game"), Some(2));
        assert_eq!(package_version(&path, "Other"), None);
        std::fs::write(&path, b"<html>404</html>").unwrap();
        assert_eq!(package_version(&path, "Game"), None);
        std::fs::write(&path, &buf[..buf.len() / 2]).unwrap();
        assert_eq!(package_version(&path, "Game"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovers_interrupted_swap() {
        let dir = temp_dir("recover");
        let zip = dir.join("game.zip");
        let old = dir.join("game.zip.old");

        // crash after the zip was renamed away: the old copy goes back
        std::fs::write(&old, b"old").unwrap();
        recover_packages(&dir);
        assert_eq!(std::fs::read(&zip).unwrap(), b"old");
        assert!(!old.exists());

        // crash after the new zip landed: the leftover backup is dropped
        std::fs::write(&zip, b"new").unwrap();
        std::fs::write(&old, b"old").unwrap();
        recover_packages(&dir);
        assert_eq!(std::fs::read(&zip).unwrap(), b"new");
        assert!(!old.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_modlist() {
        let meta: Meta =
            rust_yaml::from_str("modlist:\n  - loader\n  - Graphics:\n      - patch\n").unwrap();
        assert_eq!(
            rows(&meta),
            [
                ("loader".to_string(), 0),
                ("Graphics".to_string(), 0),
                ("patch".to_string(), 1)
            ]
        );
    }
}
