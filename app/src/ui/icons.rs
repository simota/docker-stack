//! Icons: the navigation and the row buttons of the tables.

use std::sync::OnceLock;

use noa_gpui_platform::noa_gpui_scene::VectorIcon;

const PLAY: &str = r#"<svg viewBox="0 0 24 24"><path d="M7 4.5v15l12-7.5z"/></svg>"#;
const STOP: &str =
    r#"<svg viewBox="0 0 24 24"><rect x="6" y="6" width="12" height="12" rx="1.5"/></svg>"#;
const PAUSE: &str = r#"<svg viewBox="0 0 24 24"><rect x="6" y="5" width="4" height="14" rx="1"/><rect x="14" y="5" width="4" height="14" rx="1"/></svg>"#;
const RESTART: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M20 12a8 8 0 1 1-2.3-5.6"/><path d="M20 4v5h-5"/></svg>"#;
const KILL: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="m6 6 12 12M18 6 6 18"/></svg>"#;
const LOGS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M5 6h14M5 12h14M5 18h9"/></svg>"#;
const EXEC: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="m5 7 5 5-5 5"/><path d="M12 18h7"/></svg>"#;
const TRASH: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/></svg>"#;
const EXPAND: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>"#;
const NAV_CONTAINERS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="4" y="4" width="7" height="7" rx="1"/><rect x="13" y="4" width="7" height="7" rx="1"/><rect x="4" y="13" width="7" height="7" rx="1"/><rect x="13" y="13" width="7" height="7" rx="1"/></svg>"#;
const NAV_IMAGES: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round"><path d="m12 3 9 5-9 5-9-5z"/><path d="m3 13 9 5 9-5"/></svg>"#;
const NAV_VOLUMES: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><ellipse cx="12" cy="6" rx="7" ry="3"/><path d="M5 6v12c0 1.7 3.1 3 7 3s7-1.3 7-3V6"/><path d="M5 12c0 1.7 3.1 3 7 3s7-1.3 7-3"/></svg>"#;
const NAV_NETWORKS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="6" cy="6" r="2.5"/><circle cx="18" cy="6" r="2.5"/><circle cx="12" cy="18" r="2.5"/><path d="M8 7.5 11 16M16 7.5 13 16M8.5 6h7"/></svg>"#;
const NAV_SETTINGS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="3.5"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3M5.3 5.3l2.1 2.1M16.6 16.6l2.1 2.1M5.3 18.7l2.1-2.1M16.6 7.4l2.1-2.1"/></svg>"#;
const INSPECT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 4H7a2 2 0 0 0-2 2v4l-2 2 2 2v4a2 2 0 0 0 2 2h2"/><path d="M15 4h2a2 2 0 0 1 2 2v4l2 2-2 2v4a2 2 0 0 1-2 2h-2"/></svg>"#;

pub struct Icons {
    pub play: VectorIcon,
    pub stop: VectorIcon,
    pub pause: VectorIcon,
    pub restart: VectorIcon,
    pub kill: VectorIcon,
    pub logs: VectorIcon,
    pub exec: VectorIcon,
    pub trash: VectorIcon,
    pub expand: VectorIcon,
    pub inspect: VectorIcon,
    /// Containers, images, volumes, networks, settings.
    pub nav: [VectorIcon; 5],
}

pub fn icons() -> &'static Icons {
    static ICONS: OnceLock<Icons> = OnceLock::new();
    ICONS.get_or_init(|| {
        let svg = |s: &str| VectorIcon::from_svg(s).expect("the row icons are valid SVG");
        Icons {
            play: svg(PLAY),
            stop: svg(STOP),
            pause: svg(PAUSE),
            restart: svg(RESTART),
            kill: svg(KILL),
            logs: svg(LOGS),
            exec: svg(EXEC),
            trash: svg(TRASH),
            expand: svg(EXPAND),
            inspect: svg(INSPECT),
            nav: [
                svg(NAV_CONTAINERS),
                svg(NAV_IMAGES),
                svg(NAV_VOLUMES),
                svg(NAV_NETWORKS),
                svg(NAV_SETTINGS),
            ],
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn row_icons_parse() {
        super::icons();
    }
}
