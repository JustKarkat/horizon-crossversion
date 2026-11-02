// The version switch on Edit Server and Direct Connect: which Minecraft
// version ViaProxy tells the server it speaks, so a server whose status lies
// can still be joined. Drawn after the screen (draw_tooltip runs last) and
// clicked in Screen::click.

use super::Screen;
use crate::hud::{HudAtlas, HudBatch};

const GRAY: u32 = 0xFFAAAAAA;
const GREEN: u32 = 0xFF55FF55;

/// The line for this screen, and where it goes: (text, y, overridden).
fn line(screen: &Screen) -> Option<(String, f32, bool)> {
    let (address, y) = match screen {
        Screen::Edit { address, .. } => (address, 166.0),
        Screen::Direct { address } => (address, 160.0),
        _ => return None,
    };
    let settings = crate::net::crossversion::settings_summary();
    if !settings.0 {
        return None;
    }
    let key = address.trim().to_ascii_lowercase();
    let version = if key.is_empty() {
        "auto".to_owned()
    } else {
        crate::net::crossversion::server_version(&key)
    };
    let text = if version == "auto" {
        "ViaProxy asks the server its version (click to choose)".to_owned()
    } else {
        format!("ViaProxy acts as {version} to this server (click to change)")
    };
    Some((text, y, version != "auto"))
}

pub fn draw(screen: &Screen, atlas: &HudAtlas, batch: &mut HudBatch, size: (f32, f32)) {
    let Some((text, y, overridden)) = line(screen) else {
        return;
    };
    let width = HudBatch::text_width(atlas, &text);
    let x = (size.0 / 2.0 - width / 2.0).max(2.0);
    let y = size.1 / 4.0 + y;
    let color = if overridden { GREEN } else { GRAY };
    batch.text(atlas, &text, x, y, color, true);
}

/// A click on that line: the next version, and keep it for this server.
pub fn click(screen: &mut Screen, x: f32, y: f32, size: (f32, f32)) -> bool {
    let Some((text, line_y, _)) = line(screen) else {
        return false;
    };
    let _ = &text;
    let width = 220.0;
    let left = size.0 / 2.0 - width / 2.0;
    let top = size.1 / 4.0 + line_y - 2.0;
    let hit = x >= left && x <= left + width && y >= top && y <= top + 12.0;
    if !hit {
        return false;
    }
    let address = match screen {
        Screen::Edit { address, .. } | Screen::Direct { address } => address,
        _ => return false,
    };
    let key = address.trim().to_ascii_lowercase();
    if key.is_empty() {
        return true;
    }
    let next = crate::net::crossversion::cycle_server_version(&key);
    eprintln!("[crossversion] {key}: ViaProxy now acts as {next} to this server");
    true
}
