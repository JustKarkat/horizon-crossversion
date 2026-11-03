// Options > Mods: this mod's row and its settings page.
use crate::game::native_hooks::NativeMod;

const ID: &str = "crossversion";

pub fn register(mods: &mut Vec<NativeMod>) {
    let (enabled, java, proxy) = horizon_client::net::crossversion::summary();
    mods.push(NativeMod {
        id: ID.to_owned(),
        title: format!(
            "Cross-version {}",
            horizon_client::net::crossversion::VERSION
        ),
        description: format!(
            "Joins servers that speak an older Minecraft version by running ViaProxy \
             ({} GPL-3.0) on a loopback port; {}. Offline-mode servers only. Settings \
             live in horizon-mods/.horizon-mod-data/crossversion/config.json and the \
             state is {}.",
            proxy,
            java,
            if enabled { "on" } else { "off" }
        ),
        keys: Vec::new(),
    });
}

pub fn settings(mod_id: &str, buttons: &mut Vec<String>) {
    if mod_id != ID {
        return;
    }
    let (enabled, _, proxy) = horizon_client::net::crossversion::summary();
    buttons.push(if enabled {
        "Cross-version is on: turn it off".to_owned()
    } else {
        "Cross-version is off: turn it on".to_owned()
    });
    buttons.push(format!("Delete the downloaded ViaProxy ({proxy})"));
}

pub fn click(mod_id: &str, button: usize) {
    if mod_id != ID {
        return;
    }
    match button {
        0 => {
            let on = horizon_client::net::crossversion::toggle_enabled();
            eprintln!(
                "[crossversion] {} from the settings page",
                if on { "on" } else { "off" }
            );
        }
        1 => match horizon_client::net::crossversion::clear_downloads() {
            Ok(()) => {
                eprintln!("[crossversion] downloaded ViaProxy deleted; the next join fetches it")
            }
            Err(error) => eprintln!("[crossversion] could not delete it: {error}"),
        },
        _ => {}
    }
}
