#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    error::Error,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
};

use mlua::Lua;
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use walkdir::WalkDir;
use zip::ZipArchive;

slint::include_modules!();

/// Reads a flat `key: value` scalar out of meta.yaml
/// shortcut: no YAML parsing (no nesting, no multi-line values), add a parser if meta.yaml grows
fn yaml_value(yaml: &str, key: &str) -> Option<String> {
    yaml.lines()
        .find_map(|line| {
            let (k, v) = line.trim().split_once(':')?;
            (k.trim() == key).then(|| v.trim().trim_matches(&['"', '\''][..]).to_string())
        })
        .filter(|v| !v.is_empty())
}

/// Opens one game zip, reads `meta.yaml` and its first `icon/*.jpg`
fn load_game(zip_path: &Path) -> Option<gamedata> {
    let mut zip = ZipArchive::new(File::open(zip_path).ok()?).ok()?;

    let mut meta = String::new();
    zip.by_name("meta.yaml")
        .ok()?
        .read_to_string(&mut meta)
        .ok()?;

    let name =
        yaml_value(&meta, "name").or_else(|| Some(zip_path.file_stem()?.to_str()?.to_string()))?;
    let publisher = yaml_value(&meta, "publisher").unwrap_or_else(|| "Unknown Publisher".into());

    let script = zip
        .file_names()
        .filter_map(Result::ok)
        .find(|n| n.eq_ignore_ascii_case("script.lua"))
        .map(|n| n.to_string())
        .unwrap_or_default();

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

/// Reads `script.lua` out of the package and runs it with a `game` table
/// (name, publisher, package path) available to the script.
/// shortcut: runs on the UI thread, move to a worker if scripts get slow
fn execute_script(
    zip_path: &Path,
    script_name: &str,
    game: &gamedata,
) -> Result<(), Box<dyn Error>> {
    if script_name.is_empty() {
        return Err(format!("{} has no script.lua", zip_path.display()).into());
    }

    let mut zip = ZipArchive::new(File::open(zip_path)?)?;
    let mut source = String::new();
    zip.by_name(script_name)?.read_to_string(&mut source)?;

    let lua = Lua::new();
    let game_table = lua.create_table()?;
    game_table.set("name", game.name.as_str())?;
    game_table.set("publisher", game.publisher.as_str())?;
    game_table.set("package", zip_path.to_string_lossy().as_ref())?;
    lua.globals().set("game", game_table)?;
    lua.load(&source).set_name(script_name).exec()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    let found = scan_folder(&games_dir()?);
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

    ui.on_patch_clicked(move |idx| {
        let idx = idx as usize;
        let (Some(game), Some(zip)) = (games.row_data(idx), sources.get(idx)) else {
            return;
        };
        if let Err(e) = execute_script(zip, &game.script, &game) {
            eprintln!("patch failed for {}: {e}", game.name);
        }
    });

    ui.run()?;
    Ok(())
}
