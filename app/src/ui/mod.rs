//! The native window, drawn with noa-gpui: a command deck with navigation, usage
//! meters and the connection, and one page per resource kind.

mod i18n;
mod icons;
mod model;
mod pages;
mod stack_map;
mod tokens;
mod widgets;

use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use noa_gpui_platform::noa_gpui_core::style::{AlignItems, FlexDirection, Rect, Size, length};
use noa_gpui_platform::noa_gpui_core::theme::{MARK_CONTRAST, readable};
use noa_gpui_platform::noa_gpui_core::widgets::{
    Alert, AlertResponse, Button, ButtonClicked, CheckState, Checkbox, CheckboxChanged, Container,
    Label, Popover, Pressable, Sheet, SvgIcon, Toast,
};
use noa_gpui_platform::noa_gpui_core::{
    Appearance, Edge, Key, Message, OverlayDismissed, Role, Shortcut, ShortcutPressed, Style,
    Theme, UiTree, WidgetId,
};
use noa_gpui_platform::noa_gpui_scene::{Color, Size as Viewport};
use noa_gpui_platform::noa_gpui_text::TextStore;
use noa_gpui_platform::{App, AppOptions, Screen};

use crate::backend::{Backend, UiEvent};
use crate::commands;
use crate::config::{Config, ThemePref};
use crate::dto::{
    CleanupScope, ComposeCommand, ConnState, ContainerAction, ContextEntry, NoticeKind, PruneKind,
    Snapshot,
};
use i18n::{Lang, Tr};
use model::{CleanupForm, CleanupToggle, ConnView, Tone};
use pages::PageView;
use widgets::{Tick, Ticker};

/// The app's colors (cool grays and a Docker blue), light or dark; with
/// `increase_contrast`, control borders reach 3:1 on the background and surfaces.
fn palette(appearance: Appearance, increase_contrast: bool) -> Theme {
    let hex = Color::hex;
    let (accent, base) = match appearance {
        Appearance::Light => (
            hex(0x1d63ed),
            Theme {
                background: hex(0xf8f9fb),
                surface: hex(0xffffff),
                surface_alt: hex(0xeef1f6),
                text: hex(0x121821),
                muted: hex(0x526071),
                outline: hex(0xb6c2d2),
                separator: hex(0xd5dce6),
                ..Theme::for_appearance(appearance)
            },
        ),
        Appearance::Dark => (
            hex(0x4c8dff),
            Theme {
                background: hex(0x10141b),
                surface: hex(0x1c2430),
                surface_alt: hex(0x161c25),
                text: hex(0xe6ebf2),
                muted: hex(0x8f9eb2),
                outline: hex(0x38475e),
                separator: hex(0x263040),
                ..Theme::for_appearance(appearance)
            },
        ),
    };
    let status = match appearance {
        Appearance::Light => (hex(0x0f7b4b), hex(0xa85507), hex(0xc42828)),
        Appearance::Dark => (hex(0x38c982), hex(0xf2a93b), hex(0xf46464)),
    };
    let theme = base
        .with_accent(accent)
        .with_status_colors(status.0, status.1, status.2);
    // Only ever turned on: turning it off would bring back the design's own border,
    // not this palette's.
    if !increase_contrast {
        return theme;
    }
    let mut theme = theme.with_increased_contrast(true);
    // noa-gpui reaches 3:1 on the background and surfaces; the deck, banner and
    // table headers sit on surface_alt, so pull the border on until it is 3:1 there too.
    let grounds = [theme.background, theme.surface, theme.surface_alt];
    theme.outline = readable(theme.outline, theme.text, &grounds, MARK_CONTRAST);
    theme
}

pub fn run(backend: Backend, events: Receiver<UiEvent>) {
    let options = AppOptions {
        version: Some(env!("CARGO_PKG_VERSION").into()),
        size: (1240.0, 800.0),
        // Closing the window hides it; the screen quits unless the tray is on.
        keep_running: true,
        ..AppOptions::new("Docker Stack")
    };
    let screen_backend = backend.clone();
    let app = App::new(options, move |_| {
        Box::new(DockerScreen::new(screen_backend, events))
    });
    backend.set_waker(app.waker());
    app.run().expect("failed to run the event loop");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Containers,
    Images,
    Volumes,
    Networks,
    Settings,
}

const PAGES: [Page; 5] = [
    Page::Containers,
    Page::Images,
    Page::Volumes,
    Page::Networks,
    Page::Settings,
];

/// What a confirmed alert button does.
#[derive(Debug, Clone)]
pub enum Act {
    Container(String, ContainerAction),
    /// Update a container (id, name) to the newest image of its tag.
    UpdateContainer(String, String),
    /// Update containers (id, name) like [`Act::UpdateContainer`].
    UpdateContainers(Vec<(String, String)>),
    /// Connect a container (id) to a network (`true`), or disconnect it.
    ContainerNetwork(String, String, bool),
    Compose(String, ComposeCommand),
    Prune(PruneKind),
    RemoveImage(String, bool),
    /// Images with whether each is forced.
    RemoveImages(Vec<(String, bool)>),
    /// Containers with whether each is forced (running).
    RemoveContainers(Vec<(String, bool)>),
    RemoveVolume(String),
    RemoveVolumes(Vec<String>),
    RemoveNetwork(String),
    RemoveNetworks(Vec<String>),
    UseContext(Option<String>),
    GoTo(Page),
    /// Asks before pruning the build cache.
    AskPruneBuildCache,
    /// Opens the cross-resource cleanup sheet.
    OpenCleanup,
    /// Runs a cross-resource cleanup for `CleanupScope`.
    Cleanup(CleanupScope),
}

/// The command deck across the top: navigation, usage meters and the connection.
struct Sidebar {
    /// The emblem's six cells, lit by the containers' states.
    emblem: [WidgetId; 6],
    /// The navigation buttons; the current page's is shown selected.
    nav: Vec<WidgetId>,
    meters: Vec<MeterIds>,
    disk: WidgetId,
    cleanup: WidgetId,
    conn_dot: WidgetId,
    conn_title: WidgetId,
    conn_host: WidgetId,
    context: WidgetId,
}

/// The mark in front of a toast's text.
fn toast_prefix(kind: NoticeKind) -> &'static str {
    match kind {
        NoticeKind::Success => "✓ ",
        NoticeKind::Error => "⚠️ ",
    }
}

/// The brand emblem: two rows of three cells over a blue base. Returns the cells.
fn build_emblem(tree: &mut UiTree, parent: WidgetId) -> [WidgetId; 6] {
    let emblem = tree.insert(Some(parent), widgets::column(tokens::SPACE_HALF));
    let mut cells = Vec::with_capacity(6);
    for _ in 0..2 {
        let row = tree.insert(Some(emblem), widgets::row(2.5));
        for _ in 0..3 {
            cells.push(
                tree.insert(
                    Some(row),
                    Container::new(Style {
                        size: Size {
                            width: length(7.0),
                            height: length(6.0),
                        },
                        flex_shrink: 0.0,
                        ..Default::default()
                    })
                    .radius(1.2)
                    .background(Role::Outline),
                ),
            );
        }
    }
    tree.insert(
        Some(emblem),
        Container::new(Style {
            size: Size {
                width: length(26.0),
                height: length(3.0),
            },
            margin: noa_gpui_platform::noa_gpui_core::style::Rect {
                top: length(3.0),
                ..noa_gpui_platform::noa_gpui_core::style::Rect::zero()
            },
            ..Default::default()
        })
        .radius(1.0)
        .background(Role::Accent),
    );
    cells.try_into().expect("six cells")
}

/// An open popover and the act of each of its buttons.
struct OpenPopover {
    root: WidgetId,
    acts: HashMap<WidgetId, Act>,
}

const CLEANUP_SHEET_WIDTH: f32 = 540.0;

struct CleanupRow {
    row: WidgetId,
    checkbox: WidgetId,
    detail: WidgetId,
}

struct CleanupSheet {
    sheet: WidgetId,
    form: CleanupForm,
    containers: CleanupRow,
    networks: CleanupRow,
    dangling_images: CleanupRow,
    tagged_images: CleanupRow,
    volumes: CleanupRow,
    named_volumes: CleanupRow,
    build_cache: CleanupRow,
    warning: WidgetId,
    cancel: WidgetId,
    submit: WidgetId,
}

impl CleanupSheet {
    fn open(tree: &mut UiTree, form: CleanupForm, snap: &Snapshot, t: Tr) -> Self {
        let sheet = Sheet::new(CLEANUP_SHEET_WIDTH).open(tree, 0.0);
        let body = tree.insert(Some(sheet), widgets::column(tokens::SPACE_3));
        tree.insert(
            Some(body),
            Label::new(t.cleanup_title())
                .weight(tokens::WEIGHT_STRONG)
                .font_size(tokens::TEXT_MD),
        );
        tree.insert(
            Some(body),
            Label::new(t.cleanup_subtitle())
                .wrap(true)
                .font_size(tokens::TEXT_SM)
                .color(Role::Muted),
        );
        tree.insert(Some(body), widgets::rule(tokens::RULE, Role::Separator));

        let rows = tree.insert(Some(body), widgets::column(tokens::SPACE_2));
        let containers = Self::build_row(
            tree,
            rows,
            t.cleanup_stopped_containers(),
            form.containers,
            0.0,
        );
        let networks = Self::build_row(tree, rows, t.cleanup_unused_networks(), form.networks, 0.0);
        let dangling_images = Self::build_row(
            tree,
            rows,
            t.cleanup_dangling_images(),
            form.dangling_images,
            0.0,
        );
        let tagged_images = Self::build_row(
            tree,
            rows,
            t.cleanup_tagged_images(),
            form.tagged_images,
            tokens::SPACE_6,
        );
        let volumes_label = if form.legacy_volumes {
            t.cleanup_legacy_volumes()
        } else {
            t.cleanup_anonymous_volumes()
        };
        let volumes = Self::build_row(tree, rows, volumes_label, form.volumes, 0.0);
        let named_volumes = Self::build_row(
            tree,
            rows,
            t.cleanup_named_volumes(),
            form.named_volumes,
            tokens::SPACE_6,
        );
        let build_cache =
            Self::build_row(tree, rows, t.cleanup_build_cache(), form.build_cache, 0.0);

        let warning = tree.insert(
            Some(body),
            Container::new(Style {
                flex_direction: FlexDirection::Column,
                padding: widgets::padding_xy(tokens::SPACE_4, tokens::SPACE_3),
                ..Default::default()
            })
            .radius(6.0)
            .border(tokens::RULE, Role::Outline)
            .background(Role::SurfaceAlt),
        );
        tree.insert(
            Some(warning),
            Label::new(format!("⚠️ {}", t.cleanup_named_volumes_warning()))
                .wrap(true)
                .font_size(tokens::TEXT_SM),
        );

        tree.insert(
            Some(body),
            Label::new(t.cleanup_estimate_hint())
                .wrap(true)
                .font_size(tokens::TEXT_XS)
                .color(Role::Muted),
        );
        tree.insert(Some(body), widgets::rule(tokens::RULE, Role::Separator));

        let foot = tree.insert(Some(body), widgets::row(tokens::SPACE_3));
        tree.insert(Some(foot), widgets::spacer());
        let cancel = tree.insert(Some(foot), Button::new(t.cancel()).cancel_button(true));
        let submit = tree.insert(
            Some(foot),
            Button::new(t.cleanup_run()).default_button(true),
        );

        let mut this = Self {
            sheet,
            form,
            containers,
            networks,
            dangling_images,
            tagged_images,
            volumes,
            named_volumes,
            build_cache,
            warning,
            cancel,
            submit,
        };
        this.refresh(tree, snap, t);
        this
    }

    fn build_row(
        tree: &mut UiTree,
        parent: WidgetId,
        label: &str,
        checked: bool,
        indent: f32,
    ) -> CleanupRow {
        let row = tree.insert(
            Some(parent),
            Container::new(Style {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::CENTER,
                gap: widgets::spacing(tokens::SPACE_3),
                padding: Rect {
                    left: length(indent),
                    ..Rect::zero()
                },
                flex_shrink: 0.0,
                ..Default::default()
            }),
        );
        let checkbox = tree.insert(Some(row), Checkbox::new(label).checked(checked));
        tree.insert(Some(row), widgets::spacer());
        let detail = tree.insert(
            Some(row),
            Label::new("")
                .font_size(tokens::TEXT_SM)
                .color(Role::Muted)
                .font_family(widgets::MONO),
        );
        CleanupRow {
            row,
            checkbox,
            detail,
        }
    }

    fn toggle(&mut self, source: WidgetId, on: bool) -> bool {
        let which = if source == self.containers.checkbox {
            CleanupToggle::Containers
        } else if source == self.networks.checkbox {
            CleanupToggle::Networks
        } else if source == self.dangling_images.checkbox {
            CleanupToggle::DanglingImages
        } else if source == self.tagged_images.checkbox {
            CleanupToggle::TaggedImages
        } else if source == self.volumes.checkbox {
            CleanupToggle::Volumes
        } else if source == self.named_volumes.checkbox {
            CleanupToggle::NamedVolumes
        } else if source == self.build_cache.checkbox {
            CleanupToggle::BuildCache
        } else {
            return false;
        };
        self.form.set(which, on);
        true
    }

    fn refresh(&mut self, tree: &mut UiTree, snap: &Snapshot, t: Tr) {
        let p = model::cleanup_preview(snap, &self.form, t);
        for (row, checked, detail) in [
            (&self.containers, self.form.containers, &p.containers),
            (&self.networks, self.form.networks, &p.networks),
            (
                &self.dangling_images,
                self.form.dangling_images,
                &p.dangling_images,
            ),
            (
                &self.tagged_images,
                self.form.tagged_images,
                &p.tagged_images,
            ),
            (&self.volumes, self.form.volumes, &p.volumes),
            (
                &self.named_volumes,
                self.form.named_volumes,
                &p.named_volumes,
            ),
            (&self.build_cache, self.form.build_cache, &p.build_cache),
        ] {
            let want = if checked {
                CheckState::On
            } else {
                CheckState::Off
            };
            tree.update::<Checkbox, _>(row.checkbox, |cb, cx| cb.set_state(cx, want));
            tree.update::<Label, _>(row.detail, |l, cx| {
                if l.text() != detail {
                    l.set_text(cx, detail.clone());
                }
            });
        }
        set_hidden(tree, self.named_volumes.row, self.form.legacy_volumes);
        set_hidden(tree, self.warning, !p.warn_named_volumes);
        let submit_label = if p.prune_pending {
            t.cleanup_in_progress()
        } else {
            t.cleanup_run()
        };
        tree.update::<Button, _>(self.submit, |b, cx| {
            if b.label() != submit_label {
                b.set_label(cx, submit_label);
            }
        });
        if tree.is_enabled(self.submit) != p.can_submit {
            tree.set_enabled(self.submit, p.can_submit);
        }
    }

    fn close(self, tree: &mut UiTree) {
        Sheet::close(tree, self.sheet);
    }
}

/// A usage meter in the command deck: name and percent over a thin bar, the amount
/// under it.
struct MeterIds {
    name: WidgetId,
    percent: WidgetId,
    fill: WidgetId,
    amount: WidgetId,
}

/// The filled part of a meter's bar, `fraction` of its width.
fn meter_fill(fraction: f64) -> Container {
    Container::new(meter_fill_style(fraction))
        .radius(tokens::METER_BAR / 2.0)
        .background(Role::Accent)
}

fn meter_fill_style(fraction: f64) -> Style {
    Style {
        size: Size {
            width: noa_gpui_platform::noa_gpui_core::style::percent(fraction.clamp(0.0, 1.0) as f32),
            height: noa_gpui_platform::noa_gpui_core::style::percent(1.0),
        },
        ..Default::default()
    }
}

struct Banner {
    root: WidgetId,
    title: WidgetId,
    detail: WidgetId,
    start: WidgetId,
    rootless: WidgetId,
    retry: WidgetId,
}

/// Everything a page needs to read or change outside the widget tree.
pub struct Ctx<'a> {
    pub tree: &'a mut UiTree,
    pub text: &'a mut TextStore,
    pub backend: &'a Backend,
    pub snap: &'a Snapshot,
    pub config: &'a Config,
    pub t: Tr,
    /// Alerts the page opened, with what each button does.
    pub alerts: &'a mut HashMap<WidgetId, Vec<Option<Act>>>,
    /// Set when the page wants the config saved.
    pub save_config: &'a mut Option<Config>,
    /// Set when the page wants the context chooser.
    pub open_contexts: &'a mut bool,
}

impl Ctx<'_> {
    pub fn connected(&self) -> bool {
        matches!(self.snap.conn, ConnState::Connected { .. })
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        Toast::new(text).show(self.tree);
    }

    pub fn attempt(&mut self, result: Result<(), crate::dto::UiError>) {
        if let Err(e) = result {
            self.toast(e.message);
        }
    }

    /// Asks before running `act`; `ok` is the confirming button's title.
    pub fn confirm(&mut self, title: &str, body: &str, ok: &str, act: Act) {
        let alert = Alert::new(title)
            .informative_text(body)
            .button(ok)
            .button(self.t.cancel())
            .open(self.tree);
        self.alerts.insert(alert, vec![Some(act), None]);
    }

    /// Asks which of `choices` (button title, what it does) to run; Cancel comes last.
    pub fn choose(&mut self, title: &str, body: &str, choices: Vec<(String, Act)>) {
        let mut alert = Alert::new(title).informative_text(body);
        let mut acts = Vec::with_capacity(choices.len() + 1);
        for (label, act) in choices {
            alert = alert.button(label.as_str());
            acts.push(Some(act));
        }
        let alert = alert.button(self.t.cancel()).open(self.tree);
        acts.push(None);
        self.alerts.insert(alert, acts);
    }

    pub fn set_label(&mut self, id: WidgetId, text: impl Into<String>) {
        let text = text.into();
        self.tree.update::<Label, _>(id, |l, cx| {
            if l.text() != text {
                l.set_text(cx, text);
            }
        });
    }

    pub fn enable(&mut self, id: WidgetId, enabled: bool) {
        if self.tree.is_enabled(id) != enabled {
            self.tree.set_enabled(id, enabled);
        }
    }

    pub fn show(&mut self, id: WidgetId, shown: bool) {
        if self.tree.is_hidden(id) == shown {
            self.tree.set_hidden(id, !shown);
        }
    }
}

pub struct DockerScreen {
    tree: UiTree,
    backend: Backend,
    events: Receiver<UiEvent>,
    snap: Snapshot,
    config: Config,
    lang: Lang,
    system_locale: Option<String>,
    system_appearance: Appearance,
    page: Page,
    view: PageView,
    page_root: WidgetId,
    sidebar: Sidebar,
    banner: Banner,
    alerts: HashMap<WidgetId, Vec<Option<Act>>>,
    /// The popover open over the deck (disk usage, contexts), with what its buttons do.
    popover: Option<OpenPopover>,
    /// Modal sheet for cross-resource cleanup.
    cleanup: Option<CleanupSheet>,
    /// The context chooser opens once the contexts are listed.
    context_wanted: bool,
    ticker: WidgetId,
    contexts: Vec<ContextEntry>,
    starting_since: Option<Instant>,
    tray_changed: bool,
    quit: bool,
    /// A request for the main window: `Some(true)` shows it, `Some(false)` hides it.
    main_window: Option<bool>,
}

impl DockerScreen {
    fn new(backend: Backend, events: Receiver<UiEvent>) -> Self {
        let config = commands::get_config(&backend);
        let system_locale = sys_locale::get_locale();
        let lang = i18n::resolve(config.language, system_locale.as_deref());
        let t = Tr(lang);
        let snap = backend.snapshot();
        let mut tree = UiTree::new();
        let root = tree.insert(
            None,
            Container::new(Style {
                flex_direction: FlexDirection::Column,
                size: widgets::full(),
                ..Default::default()
            })
            .background(Role::Background),
        );
        let ticker = tree.insert(Some(root), Ticker);
        for shortcut in [
            Shortcut::command('r'),
            Shortcut::command('f'),
            Shortcut::command('l'),
            Shortcut::command('e'),
            Shortcut::key(Key::Escape),
            Shortcut::key(Key::Delete),
        ] {
            tree.add_shortcut(shortcut, ticker);
        }

        // ---- command deck ----
        // Two rows: brand, navigation and meters; then the connection at the far end.
        let deck_box = tree.insert(
            Some(root),
            Container::new(Style {
                flex_direction: FlexDirection::Column,
                gap: widgets::spacing(tokens::SPACE_HALF),
                padding: widgets::padding_xy(tokens::SPACE_5, tokens::SPACE_2),
                flex_shrink: 0.0,
                ..Default::default()
            })
            .background(Role::SurfaceAlt),
        );
        let deck = tree.insert(Some(deck_box), widgets::row(tokens::SPACE_4));
        let deck_end = tree.insert(Some(deck_box), widgets::row(tokens::SPACE_3));
        tree.insert(Some(deck_end), widgets::spacer());
        tree.insert(Some(root), widgets::rule(tokens::RULE_STRONG, Role::Accent));
        let icons = icons::icons();
        let brand = tree.insert(Some(deck), widgets::row(tokens::SPACE_3));
        let emblem = build_emblem(&mut tree, brand);
        tree.insert(
            Some(brand),
            Label::new("Docker Stack")
                .font_size(tokens::TEXT_MD)
                .weight(tokens::WEIGHT_STRONG),
        );
        let nav_row = tree.insert(Some(deck), widgets::row(tokens::SPACE_1));
        let nav = ['1', '2', '3', '4', ',']
            .into_iter()
            .enumerate()
            .map(|(i, key)| {
                tree.insert(
                    Some(nav_row),
                    Button::new("")
                        .detail_font_family(widgets::MONO)
                        .icon(icons.nav[i].clone())
                        .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed)
                        .shortcut(Shortcut::command(key)),
                )
            })
            .collect();
        tree.insert(Some(deck), widgets::vrule(tokens::RULE, Role::Separator));
        let mut meters = Vec::new();
        for _ in 0..3 {
            let item = tree.insert(
                Some(deck),
                Container::new(Style {
                    flex_direction: FlexDirection::Column,
                    gap: widgets::spacing(tokens::SPACE_1),
                    size: Size {
                        width: length(tokens::METER_WIDTH),
                        height: noa_gpui_platform::noa_gpui_core::style::auto(),
                    },
                    flex_shrink: 0.0,
                    ..Default::default()
                }),
            );
            let head = tree.insert(Some(item), widgets::row(0.0));
            let name = tree.insert(
                Some(head),
                Label::new("")
                    .font_size(tokens::TEXT_XS)
                    .weight(tokens::WEIGHT_STRONG)
                    .font_family(widgets::MONO),
            );
            tree.insert(Some(head), widgets::spacer());
            let percent = tree.insert(
                Some(head),
                Label::new("")
                    .font_size(tokens::TEXT_XS)
                    .font_family(widgets::MONO),
            );
            let track = tree.insert(
                Some(item),
                widgets::rule(tokens::METER_BAR, Role::Separator).radius(tokens::METER_BAR / 2.0),
            );
            let fill = tree.insert(Some(track), meter_fill(0.0));
            let amount = tree.insert(
                Some(item),
                Label::new("")
                    .font_size(tokens::TEXT_XS)
                    .color(Role::Muted)
                    .font_family(widgets::MONO),
            );
            meters.push(MeterIds {
                name,
                percent,
                fill,
                amount,
            });
        }
        let disk = tree.insert(
            Some(deck),
            Button::new(t.disk_short())
                .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed)
                .control_size(noa_gpui_platform::noa_gpui_core::widgets::ControlSize::Small),
        );
        let cleanup = tree.insert(
            Some(deck_end),
            Button::new(t.cleanup_open())
                .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed)
                .control_size(noa_gpui_platform::noa_gpui_core::widgets::ControlSize::Small),
        );
        // The connection: a status dot, the engine and its address; the chevron opens
        // the context chooser.
        let conn = tree.insert(
            Some(deck_end),
            Pressable::new(Style {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::CENTER,
                gap: widgets::spacing(tokens::SPACE_3),
                padding: widgets::padding_xy(tokens::SPACE_3, tokens::SPACE_1),
                ..Default::default()
            })
            .label(t.context_title())
            .tooltip(t.context_title()),
        );
        let conn_dot = tree.insert(
            Some(conn),
            Container::new(Style {
                size: Size {
                    width: length(tokens::DOT),
                    height: length(tokens::DOT),
                },
                flex_shrink: 0.0,
                ..Default::default()
            })
            .radius(tokens::DOT / 2.0)
            .background(Role::Muted),
        );
        let conn_text = tree.insert(Some(conn), widgets::column(tokens::SPACE_HALF));
        let conn_title = tree.insert(Some(conn_text), Label::new("").font_size(tokens::TEXT_SM));
        let conn_host = tree.insert(
            Some(conn_text),
            Label::new("")
                .font_size(tokens::TEXT_XS)
                .color(Role::Muted)
                .font_family(widgets::MONO),
        );
        tree.insert(
            Some(conn),
            SvgIcon::new(icons.expand.clone(), tokens::TEXT_BASE).color(Role::Muted),
        );
        let context = conn;

        // ---- main ----
        let main = tree.insert(Some(root), widgets::fill_column(0.0, 0.0));
        let banner_root = tree.insert(
            Some(main),
            widgets::bar(tokens::SPACE_2).background(Role::SurfaceAlt),
        );
        let banner_text = tree.insert(Some(banner_root), widgets::column(tokens::SPACE_HALF));
        let title = tree.insert(
            Some(banner_text),
            Label::new("").weight(tokens::WEIGHT_STRONG),
        );
        let detail = tree.insert(
            Some(banner_text),
            Label::new("").wrap(true).font_size(tokens::TEXT_SM),
        );
        let buttons = tree.insert(Some(banner_root), widgets::row(tokens::SPACE_3));
        let start = tree.insert(Some(buttons), Button::new(""));
        let rootless = tree.insert(Some(buttons), Button::new(t.switch_rootless()));
        let retry = tree.insert(Some(buttons), Button::new(""));
        let page_root = tree.insert(Some(main), widgets::fill_column(0.0, 0.0));

        let mut screen = Self {
            tree,
            backend,
            events,
            snap,
            config,
            lang,
            system_locale,
            system_appearance: Appearance::Light,
            page: Page::Containers,
            view: PageView::None,
            page_root,
            sidebar: Sidebar {
                emblem,
                nav,
                meters,
                disk,
                cleanup,
                conn_dot,
                conn_title,
                conn_host,
                context,
            },
            banner: Banner {
                root: banner_root,
                title,
                detail,
                start,
                rootless,
                retry,
            },
            alerts: HashMap::new(),
            popover: None,
            cleanup: None,
            context_wanted: false,
            ticker,
            contexts: Vec::new(),
            starting_since: None,
            tray_changed: true,
            quit: false,
            main_window: None,
        };
        screen.apply_theme();
        // Contexts are listed once so the banner can offer rootless Docker.
        screen.request_contexts(false);
        screen
    }

    fn t(&self) -> Tr {
        Tr(self.lang)
    }

    fn page_title(&self, page: Page) -> &'static str {
        let t = self.t();
        match page {
            Page::Containers => t.nav_containers(),
            Page::Images => t.nav_images(),
            Page::Volumes => t.nav_volumes(),
            Page::Networks => t.nav_networks(),
            Page::Settings => t.nav_settings(),
        }
    }

    fn apply_theme(&mut self) {
        let appearance = match self.config.theme {
            ThemePref::System => self.system_appearance,
            ThemePref::Light => Appearance::Light,
            ThemePref::Dark => Appearance::Dark,
        };
        self.tree
            .set_theme(palette(appearance, self.config.increase_contrast));
    }

    /// Runs `f` with a page context, then applies what it asked for.
    fn with_ctx<R>(
        &mut self,
        text: &mut TextStore,
        f: impl FnOnce(&mut PageView, &mut Ctx) -> R,
    ) -> R {
        let mut save = None;
        let mut open_contexts = false;
        let r = {
            let mut cx = Ctx {
                tree: &mut self.tree,
                text,
                backend: &self.backend,
                snap: &self.snap,
                config: &self.config,
                t: Tr(self.lang),
                alerts: &mut self.alerts,
                save_config: &mut save,
                open_contexts: &mut open_contexts,
            };
            f(&mut self.view, &mut cx)
        };
        if let Some(config) = save {
            self.save_config(config);
        }
        if open_contexts {
            self.request_contexts(true);
        }
        r
    }

    fn show_page(&mut self, page: Page, text: &mut TextStore) {
        self.page = page;
        let old = std::mem::replace(&mut self.view, PageView::None);
        old.close(&self.backend);
        for child in self.tree.children(self.page_root).to_vec() {
            self.tree.remove(child, text);
        }
        let root = self.page_root;
        self.view = self.with_ctx(text, |_, cx| {
            PageView::build(page, root, BTreeSet::new(), cx)
        });
        self.refresh(text);
    }

    /// Brings every widget in line with the snapshot and settings.
    fn refresh(&mut self, text: &mut TextStore) {
        let t = self.t();
        let snap = &self.snap;
        let counts = [
            Some(snap.containers.len()),
            Some(snap.images.len()),
            Some(snap.volumes.len()),
            Some(snap.networks.len()),
            None,
        ];
        for (i, page) in PAGES.iter().enumerate() {
            let label = self.page_title(*page).to_owned();
            let count = counts[i].map(|n| n.to_string()).unwrap_or_default();
            let current = *page == self.page;
            self.tree.update::<Button, _>(self.sidebar.nav[i], |b, cx| {
                if b.label() != label {
                    b.set_label(cx, label);
                }
                if b.detail_text() != count {
                    b.set_detail(cx, count);
                }
                if b.is_selected() != current {
                    b.set_selected(cx, current);
                }
            });
        }
        let theme = self.tree.theme().clone();
        for (&cell, slot) in self
            .sidebar
            .emblem
            .iter()
            .zip(model::emblem_slots(&snap.containers))
        {
            let color = match slot {
                model::Slot::Running => theme.success,
                model::Slot::Paused => theme.warning,
                model::Slot::Idle => theme.outline,
            };
            self.tree
                .update::<Container, _>(cell, |c, cx| c.set_background(cx, Some(color.into())));
        }
        let meters = model::meters(&snap.usage, t);
        for (m, ids) in meters.iter().zip(&self.sidebar.meters) {
            for (id, text) in [
                (ids.name, m.label.to_owned()),
                (ids.percent, m.percent.clone()),
                (ids.amount, m.amount.clone()),
            ] {
                self.tree.update::<Label, _>(id, |l, cx| {
                    if l.text() != text {
                        l.set_text(cx, text);
                    }
                });
            }
            let fraction = m.fraction.unwrap_or(0.0);
            self.tree.update::<Container, _>(ids.fill, |c, cx| {
                c.set_style(cx, meter_fill_style(fraction))
            });
        }
        // Fixed labels follow a language change.
        let prune_pending = snap
            .pending
            .iter()
            .any(|k| k == commands::PRUNE_PENDING_KEY);
        let cleanup_label = if prune_pending {
            t.cleanup_in_progress()
        } else {
            t.cleanup_open()
        };
        for (id, label) in [
            (self.sidebar.disk, t.disk_short()),
            (self.sidebar.cleanup, cleanup_label),
            (self.banner.rootless, t.switch_rootless()),
        ] {
            self.tree.update::<Button, _>(id, |b, cx| {
                if b.label() != label {
                    b.set_label(cx, label);
                }
            });
        }
        let can_open_cleanup = matches!(snap.conn, ConnState::Connected { .. }) && !prune_pending;
        if self.tree.is_enabled(self.sidebar.cleanup) != can_open_cleanup {
            self.tree
                .set_enabled(self.sidebar.cleanup, can_open_cleanup);
        }
        let snap = &self.snap;
        let view = model::describe_conn(&snap.conn, &snap.target, &snap.daemon, t);
        let (conn_title, dot) = match &snap.conn {
            ConnState::Connected { .. } => (view.title.clone(), self.tree.theme().success),
            ConnState::Connecting => (t.not_connected().to_owned(), self.tree.theme().warning),
            _ => (t.not_connected().to_owned(), self.tree.theme().critical),
        };
        let host = snap.target.host.clone();
        for (id, text) in [
            (self.sidebar.conn_title, conn_title),
            (self.sidebar.conn_host, host),
        ] {
            self.tree.update::<Label, _>(id, |l, cx| {
                if l.text() != text {
                    l.set_text(cx, text);
                }
            });
        }
        self.tree
            .update::<Container, _>(self.sidebar.conn_dot, |c, cx| {
                c.set_background(cx, Some(dot.into()))
            });
        self.refresh_banner(view);
        self.sync_cleanup_sheet(text);
        self.with_ctx(text, |view, cx| view.refresh(cx));
    }

    fn open_cleanup_sheet(&mut self, form: Option<CleanupForm>, text: &mut TextStore) {
        if let Some(open) = self.popover.take() {
            self.tree.close_overlay(open.root, text);
        }
        if let Some(old) = self.cleanup.take() {
            self.tree.close_overlay(old.sheet, text);
        }
        let form =
            form.unwrap_or_else(|| CleanupForm::new(model::snapshot_api_version(&self.snap)));
        let t = self.t();
        self.cleanup = Some(CleanupSheet::open(&mut self.tree, form, &self.snap, t));
    }

    fn sync_cleanup_sheet(&mut self, text: &mut TextStore) {
        let api = model::snapshot_api_version(&self.snap);
        let legacy_changed = self
            .cleanup
            .as_ref()
            .is_some_and(|s| s.form.legacy_volumes != model::prune_removes_named_volumes(api));
        if legacy_changed {
            let mut form = self.cleanup.as_ref().unwrap().form;
            form.sync_api(api);
            self.open_cleanup_sheet(Some(form), text);
            return;
        }
        let t = self.t();
        if let Some(sheet) = &mut self.cleanup {
            sheet.refresh(&mut self.tree, &self.snap, t);
        }
    }

    fn refresh_banner(&mut self, view: ConnView) {
        let t = self.t();
        let b = &self.banner;
        let connected = matches!(self.snap.conn, ConnState::Connected { .. });
        set_hidden(&mut self.tree, b.root, connected);
        if connected {
            self.starting_since = None;
            return;
        }
        let detail = match &view.starting {
            Some((_, limit)) => {
                let since = *self.starting_since.get_or_insert_with(Instant::now);
                t.starting_detail(since.elapsed().as_secs(), *limit)
            }
            None => {
                self.starting_since = None;
                view.detail.clone()
            }
        };
        if view.starting.is_some() {
            // Count the seconds while the daemon starts.
            self.tree
                .update::<Ticker, _>(self.ticker, |_, cx| cx.request_timer(1.0));
        }
        let tone = match view.tone {
            Tone::Error => "⛔ ",
            Tone::Warn => "⚠️ ",
            Tone::Info | Tone::Ok => "",
        };
        let title = format!("{tone}{}", view.title);
        self.tree.update::<Label, _>(b.title, |l, cx| {
            if l.text() != title {
                l.set_text(cx, title);
            }
        });
        self.tree.update::<Label, _>(b.detail, |l, cx| {
            if l.text() != detail {
                l.set_text(cx, detail);
            }
        });
        let rootless_available = matches!(self.snap.conn, ConnState::PermissionDenied { .. })
            && self.snap.target.context.as_deref() != Some(engine::ROOTLESS_CONTEXT)
            && self
                .contexts
                .iter()
                .any(|c| c.name == engine::ROOTLESS_CONTEXT);
        set_hidden(&mut self.tree, b.start, view.start.is_none());
        set_hidden(&mut self.tree, b.retry, view.retry.is_none());
        set_hidden(&mut self.tree, b.rootless, !rootless_available);
        if let Some(start) = view.start {
            self.tree
                .update::<Button, _>(b.start, |w, cx| w.set_label(cx, start));
        }
        if let Some(retry) = view.retry {
            self.tree
                .update::<Button, _>(b.retry, |w, cx| w.set_label(cx, retry));
        }
    }

    fn save_config(&mut self, value: Config) {
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            match commands::set_config(backend.clone(), value).await {
                Ok(saved) => backend.send(UiEvent::Config(saved)),
                Err(e) => backend.send(UiEvent::Notice(crate::dto::Notice::Error(e.message))),
            };
        });
    }

    fn request_contexts(&mut self, open: bool) {
        let backend = self.backend.clone();
        self.context_wanted |= open;
        self.backend.spawn(async move {
            backend.send(UiEvent::Contexts(commands::list_contexts().await));
        });
    }

    /// Opens a popover under `anchor`, closing the one already open.
    fn open_popover(&mut self, anchor: WidgetId, text: &mut TextStore) -> WidgetId {
        if let Some(open) = self.popover.take() {
            self.tree.close_overlay(open.root, text);
        }
        let root = Popover::new()
            .padding(tokens::SPACE_4)
            .closes_on_click(true)
            .open_from(&mut self.tree, anchor, Edge::Bottom);
        self.popover = Some(OpenPopover {
            root,
            acts: HashMap::new(),
        });
        root
    }

    /// A button in the open popover that runs `act`.
    fn popover_button(&mut self, parent: WidgetId, button: Button, act: Act) -> WidgetId {
        let id = self.tree.insert(Some(parent), button);
        if let Some(open) = &mut self.popover {
            open.acts.insert(id, act);
        }
        id
    }

    /// The contexts to switch to, under the connection.
    fn open_context_chooser(&mut self, text: &mut TextStore) {
        let t = self.t();
        self.context_wanted = false;
        let root = self.open_popover(self.sidebar.context, text);
        let body = self.tree.insert(
            Some(root),
            Container::column()
                .gap(tokens::SPACE_2)
                .width(tokens::POPOVER_WIDTH),
        );
        self.tree.insert(
            Some(body),
            Label::new(t.context_title()).weight(tokens::WEIGHT_STRONG),
        );
        let target = &self.snap.target;
        let current = format!("{} ({})", target.host, target.source);
        self.tree.insert(
            Some(body),
            Label::new(current)
                .font_size(tokens::TEXT_XS)
                .color(Role::Muted)
                .font_family(widgets::MONO),
        );
        self.tree.insert(
            Some(body),
            widgets::rule(tokens::RULE_STRONG, Role::Outline),
        );
        let contexts: Vec<_> = self
            .contexts
            .iter()
            .filter(|c| c.supported)
            .cloned()
            .collect();
        for c in contexts {
            let current = self.snap.target.context.as_deref() == Some(c.name.as_str());
            let row = self.tree.insert(Some(body), widgets::row(tokens::SPACE_3));
            let mut button = Button::new(c.name.clone())
                .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed);
            if current {
                button = button.selected(true);
            }
            self.popover_button(row, button, Act::UseContext(Some(c.name.clone())));
            self.tree.insert(
                Some(row),
                Label::new(c.host.clone())
                    .font_size(tokens::TEXT_XS)
                    .color(Role::Muted)
                    .font_family(widgets::MONO),
            );
        }
        self.tree
            .insert(Some(body), widgets::rule(tokens::RULE, Role::Separator));
        let row = self.tree.insert(Some(body), widgets::row(tokens::SPACE_3));
        self.popover_button(
            row,
            Button::new(t.context_follow_cli()),
            Act::UseContext(None),
        );
    }

    /// The disk usage by kind, with links to the pages that free it, under its button.
    fn open_disk_deck(&mut self, text: &mut TextStore) {
        let t = self.t();
        let Some(b) = self.snap.usage.disk_breakdown else {
            Toast::new(t.disk_no_breakdown()).show(&mut self.tree);
            return;
        };
        let total = b.images + b.containers + b.volumes + b.build_cache;
        let root = self.open_popover(self.sidebar.disk, text);
        let body = self.tree.insert(
            Some(root),
            Container::column()
                .gap(tokens::SPACE_3)
                .width(tokens::POPOVER_WIDTH),
        );
        let head = self.tree.insert(Some(body), widgets::row(tokens::SPACE_3));
        self.tree.insert(
            Some(head),
            Label::new(t.disk_title()).weight(tokens::WEIGHT_STRONG),
        );
        self.tree.insert(Some(head), widgets::spacer());
        self.tree.insert(
            Some(head),
            Label::new(model::format_bytes(total))
                .font_size(tokens::TEXT_SM)
                .font_family(widgets::MONO),
        );
        self.tree
            .insert(Some(body), widgets::rule(tokens::RULE, Role::Separator));
        for (label, size) in [
            (t.disk_images(), b.images),
            (t.disk_volumes(), b.volumes),
            (t.disk_build_cache(), b.build_cache),
            (t.disk_containers(), b.containers),
        ] {
            let share = if total > 0.0 { size / total } else { 0.0 };
            let row = self.tree.insert(Some(body), widgets::row(tokens::SPACE_4));
            let label_box = self
                .tree
                .insert(Some(row), Container::column().width(tokens::DISK_LABEL));
            self.tree.insert(
                Some(label_box),
                Label::new(label).font_size(tokens::TEXT_SM),
            );
            let track = self.tree.insert(
                Some(row),
                Container::new(Style {
                    flex_grow: 1.0,
                    size: Size {
                        width: noa_gpui_platform::noa_gpui_core::style::auto(),
                        height: length(tokens::DISK_BAR),
                    },
                    ..Default::default()
                })
                .radius(tokens::DISK_BAR / 2.0)
                .background(Role::Separator),
            );
            self.tree.insert(
                Some(track),
                Container::new(meter_fill_style(share))
                    .radius(tokens::DISK_BAR / 2.0)
                    .background(Role::Accent),
            );
            for (value, width) in [
                (model::format_bytes(size), tokens::DISK_SIZE),
                (format!("{:.0}%", share * 100.0), tokens::DISK_SHARE),
            ] {
                let cell = self.tree.insert(
                    Some(row),
                    Container::column()
                        .width(width)
                        .align_items(AlignItems::END),
                );
                self.tree.insert(
                    Some(cell),
                    Label::new(value)
                        .font_size(tokens::TEXT_SM)
                        .font_family(widgets::MONO),
                );
            }
        }
        self.tree
            .insert(Some(body), widgets::rule(tokens::RULE, Role::Separator));
        let buttons = self
            .tree
            .insert(Some(body), widgets::wrap_row(tokens::SPACE_2));
        self.popover_button(buttons, Button::new(t.cleanup_open()), Act::OpenCleanup);
        self.popover_button(
            buttons,
            Button::new(t.nav_images()),
            Act::GoTo(Page::Images),
        );
        self.popover_button(
            buttons,
            Button::new(t.nav_volumes()),
            Act::GoTo(Page::Volumes),
        );
        self.popover_button(
            buttons,
            Button::new(t.prune_build_cache()),
            Act::AskPruneBuildCache,
        );
    }

    fn run_act(&mut self, act: Act, text: &mut TextStore) {
        let b = self.backend.clone();
        let t = self.t();
        let result = match act {
            Act::Container(id, action) => commands::container_action(&b, id, action),
            Act::UpdateContainer(id, name) => commands::update_container(&b, id, name),
            Act::UpdateContainers(cs) => commands::update_containers(&b, cs),
            Act::ContainerNetwork(id, network, connect) => {
                commands::container_network(&b, id, network, connect)
            }
            Act::Compose(project, command) => commands::compose(&b, &project, command),
            Act::AskPruneBuildCache => {
                let size = self
                    .snap
                    .usage
                    .disk_breakdown
                    .map_or(0.0, |d| d.build_cache);
                if size <= 0.0 {
                    Toast::new(t.no_build_cache()).show(&mut self.tree);
                } else {
                    let id = Alert::new(t.prune_build_cache_title())
                        .informative_text(t.prune_build_cache_body(&model::format_bytes(size)))
                        .button(t.prune())
                        .button(t.cancel())
                        .open(&mut self.tree);
                    self.alerts
                        .insert(id, vec![Some(Act::Prune(PruneKind::BuildCache)), None]);
                }
                Ok(())
            }
            Act::Prune(kind) => commands::prune(&b, kind),
            Act::OpenCleanup => {
                self.open_cleanup_sheet(None, text);
                Ok(())
            }
            Act::Cleanup(scope) => commands::cleanup(&b, scope),
            Act::RemoveImage(id, force) => commands::remove_image(&b, id, force),
            Act::RemoveImages(images) => commands::remove_images(&b, images),
            Act::RemoveContainers(cs) => commands::remove_containers(&b, cs),
            Act::RemoveVolume(name) => commands::remove_volume(&b, name),
            Act::RemoveVolumes(names) => commands::remove_volumes(&b, names),
            Act::RemoveNetwork(id) => commands::remove_network(&b, id),
            Act::RemoveNetworks(ids) => commands::remove_networks(&b, ids),
            Act::UseContext(name) => {
                let backend = b.clone();
                b.spawn(async move {
                    match commands::use_context(backend.clone(), name).await {
                        Ok(saved) => backend.send(UiEvent::Config(saved)),
                        Err(e) => {
                            backend.send(UiEvent::Notice(crate::dto::Notice::Error(e.message)))
                        }
                    };
                });
                Ok(())
            }
            Act::GoTo(page) => {
                self.show_page(page, text);
                Ok(())
            }
        };
        if let Err(e) = result {
            Toast::new(e.message).show(&mut self.tree);
        }
    }

    fn drain_events(&mut self, text: &mut TextStore) {
        let mut dirty = false;
        while let Ok(event) = self.events.try_recv() {
            match event {
                UiEvent::State(patch) => {
                    let containers = patch.containers.is_some();
                    model::apply_patch(&mut self.snap, *patch);
                    self.tray_changed |= containers;
                    dirty = true;
                }
                UiEvent::Notice(n) => {
                    let text = model::notice_text(&n, self.t());
                    Toast::new(format!("{}{text}", toast_prefix(n.kind()))).show(&mut self.tree);
                }
                UiEvent::ContainerExited(exit) => {
                    let text = model::exit_notice(&exit, self.t());
                    Toast::new(format!("⚠️ {text}")).show(&mut self.tree);
                }
                UiEvent::ContainerUpdated(update) => {
                    let (kind, text) = model::update_notice(&update, self.t());
                    Toast::new(format!("{}{text}", toast_prefix(kind))).show(&mut self.tree);
                }
                UiEvent::CleanupFinished(report) => {
                    let (kind, text) = model::cleanup_notice(&report, self.t());
                    Toast::new(format!("{}{text}", toast_prefix(kind))).show(&mut self.tree);
                }
                UiEvent::Config(config) => {
                    let lang_changed = config.language != self.config.language;
                    self.config = config;
                    self.lang = i18n::resolve(self.config.language, self.system_locale.as_deref());
                    self.apply_theme();
                    self.tray_changed = true;
                    if lang_changed {
                        // Labels are made when a page is built.
                        let page = self.page;
                        self.show_page(page, text);
                        if let Some(open) = self.cleanup.take() {
                            let form = open.form;
                            self.tree.close_overlay(open.sheet, text);
                            self.open_cleanup_sheet(Some(form), text);
                        }
                    }
                    dirty = true;
                }
                UiEvent::Contexts(result) => match result {
                    Ok(list) => {
                        self.contexts = list;
                        dirty = true;
                        if self.context_wanted {
                            self.open_context_chooser(text);
                        }
                    }
                    Err(e) => {
                        self.context_wanted = false;
                        Toast::new(e.message).show(&mut self.tree);
                    }
                },
                other => {
                    self.with_ctx(text, |view, cx| view.stream_event(other, cx));
                }
            }
        }
        if dirty {
            self.refresh(text);
        }
    }
}

fn set_hidden(tree: &mut UiTree, id: WidgetId, hidden: bool) {
    if tree.is_hidden(id) != hidden {
        tree.set_hidden(id, hidden);
    }
}

impl Screen for DockerScreen {
    fn tree(&self) -> &UiTree {
        &self.tree
    }

    fn tree_mut(&mut self) -> &mut UiTree {
        &mut self.tree
    }

    fn handle(&mut self, message: Message, text: &mut TextStore) {
        if message.downcast_ref::<Tick>().is_some() {
            if self.starting_since.is_some() {
                let view = model::describe_conn(
                    &self.snap.conn,
                    &self.snap.target,
                    &self.snap.daemon,
                    self.t(),
                );
                self.refresh_banner(view);
            }
            return;
        }
        if message.source == self.ticker
            && let Some(ShortcutPressed(shortcut)) = message.downcast_ref::<ShortcutPressed>()
        {
            if self.cleanup.is_some() {
                if *shortcut == Shortcut::key(Key::Escape)
                    && let Some(sheet) = self.cleanup.take()
                {
                    sheet.close(&mut self.tree);
                }
                return;
            }
            if *shortcut == Shortcut::command('r') {
                commands::refresh(&self.backend);
                return;
            }
            if *shortcut == Shortcut::command('f') && self.page != Page::Containers {
                self.show_page(Page::Containers, text);
            }
            let shortcut = *shortcut;
            self.with_ctx(text, |view, cx| view.shortcut(shortcut, cx));
            return;
        }
        if let Some(r) = message.downcast_ref::<AlertResponse>() {
            if let Some(mut acts) = self.alerts.remove(&message.source)
                && let Some(Some(act)) = acts.get_mut(r.button).map(Option::take)
            {
                self.run_act(act, text);
            }
            return;
        }
        if message.downcast_ref::<OverlayDismissed>().is_some()
            && self
                .popover
                .as_ref()
                .is_some_and(|p| p.root == message.source)
        {
            self.popover = None;
            return;
        }
        if let Some(CheckboxChanged(state)) = message.downcast_ref::<CheckboxChanged>() {
            let t = self.t();
            if let Some(sheet) = &mut self.cleanup
                && sheet.toggle(message.source, *state == CheckState::On)
            {
                sheet.refresh(&mut self.tree, &self.snap, t);
                return;
            }
        }
        if message.downcast_ref::<ButtonClicked>().is_some() {
            let src = message.source;
            if self.cleanup.as_ref().is_some_and(|s| s.cancel == src) {
                if let Some(sheet) = self.cleanup.take() {
                    sheet.close(&mut self.tree);
                }
                return;
            }
            if self.cleanup.as_ref().is_some_and(|s| s.submit == src) {
                if let Some(sheet) = self.cleanup.take() {
                    let scope = sheet.form.scope();
                    sheet.close(&mut self.tree);
                    self.run_act(Act::Cleanup(scope), text);
                }
                return;
            }
            if let Some(act) = self.popover.as_mut().and_then(|p| p.acts.remove(&src)) {
                self.run_act(act, text);
                return;
            }
            if let Some(i) = self.sidebar.nav.iter().position(|&n| n == src) {
                self.show_page(PAGES[i], text);
                return;
            }
            if src == self.sidebar.disk {
                self.open_disk_deck(text);
                return;
            }
            if src == self.sidebar.cleanup {
                self.open_cleanup_sheet(None, text);
                return;
            }
            if src == self.sidebar.context {
                self.request_contexts(true);
                return;
            }
            if src == self.banner.retry {
                commands::retry_now(&self.backend);
                return;
            }
            if src == self.banner.start {
                if let Err(e) = commands::start_daemon(&self.backend) {
                    Toast::new(e.message).show(&mut self.tree);
                }
                return;
            }
            if src == self.banner.rootless {
                self.run_act(Act::UseContext(Some(engine::ROOTLESS_CONTEXT.into())), text);
                return;
            }
        }
        self.with_ctx(text, |view, cx| view.handle(&message, cx));
    }

    fn set_system_appearance(&mut self, appearance: Appearance) {
        self.system_appearance = appearance;
        self.apply_theme();
    }

    fn before_frame(&mut self, text: &mut TextStore) {
        if matches!(self.view, PageView::None) {
            self.show_page(Page::Containers, text);
        }
    }

    fn woken(&mut self, text: &mut TextStore) {
        self.drain_events(text);
    }

    fn prepare(&mut self, viewport: Viewport, text: &mut TextStore) -> bool {
        let prepared = self.tree.prepare(viewport, text).is_some();
        // The exec terminal follows its widget's size once it is laid out.
        self.view.sync_terminal_size(&mut self.tree, &self.backend);
        prepared
    }

    fn wants_close(&self) -> bool {
        self.quit
    }

    fn take_main_window_visibility(&mut self) -> Option<bool> {
        self.main_window.take()
    }

    fn main_window_visibility_changed(&mut self, visible: bool, _text: &mut TextStore) {
        // Without the tray nothing could show the window again: closing it quits.
        if !visible && !self.config.tray {
            self.quit = true;
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn take_system_menus_changed(&mut self) -> bool {
        std::mem::take(&mut self.tray_changed)
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn status_menu(&self) -> Option<Vec<noa_gpui_platform::noa_gpui_core::widgets::MenuItem>> {
        use noa_gpui_platform::noa_gpui_core::widgets::MenuItem;
        if !self.config.tray {
            return None;
        }
        let t = self.t();
        let (running, stacks) = model::tray_summary(&self.snap.containers);
        let mut items = vec![
            MenuItem::new(t.tray_show()),
            MenuItem::separator(),
            MenuItem::new(t.tray_running(running)).enabled(false),
        ];
        for (project, count) in stacks {
            items.push(MenuItem::new(format!("{project}  {count}")).enabled(false));
        }
        items.push(MenuItem::separator());
        items.push(MenuItem::new(t.tray_refresh()));
        items.push(MenuItem::new(t.tray_quit()));
        Some(items)
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn status_menu_chosen(&mut self, item: usize, _text: &mut TextStore) {
        let Some(items) = self.status_menu() else {
            return;
        };
        let count = items.len();
        if item == 0 {
            self.main_window = Some(true);
        } else if item + 1 == count {
            self.quit = true;
        } else if item + 2 == count {
            commands::refresh(&self.backend);
        }
    }
}

#[cfg(test)]
mod screenshots {
    use std::path::PathBuf;

    use noa_gpui_platform::Headless;
    use noa_gpui_platform::noa_gpui_text::{FontOptions, TextStore};

    use super::*;
    use crate::config::ConfigStore;
    use crate::dto::{
        ComposeLabels, Container, ContainerState, DiskBreakdown, EngineInfo, Image, Network,
        SystemUsage, Volume,
    };
    use crate::state::AppState;

    fn container(name: &str, state: ContainerState, stack: Option<(&str, &str)>) -> Container {
        Container {
            id: format!("{name:0<16}").replace(['-', '_'], "0"),
            name: name.into(),
            image: format!("{}:latest", stack.map_or(name, |s| s.1)),
            status: match state {
                ContainerState::Running => "Up 3 hours".into(),
                _ => "Exited (0) 2 days ago".into(),
            },
            state,
            created: 0.0,
            ports: if state == ContainerState::Running {
                vec!["0.0.0.0:8080->80/tcp".into()]
            } else {
                vec![]
            },
            compose: stack.map(|(project, service)| ComposeLabels {
                project: project.into(),
                service: service.into(),
            }),
            networks: vec!["bridge".into()],
            volumes: vec!["shop_db".into()],
        }
    }

    pub(super) fn sample(s: &mut Snapshot) {
        s.conn = ConnState::Connected {
            info: EngineInfo {
                version: "29.8.2".into(),
                api_version: "1.52".into(),
                os: "linux".into(),
                arch: "arm64".into(),
            },
        };
        s.compose_available = Some(true);
        s.containers = vec![
            container("shop-web-1", ContainerState::Running, Some(("shop", "web"))),
            container("shop-db-1", ContainerState::Running, Some(("shop", "db"))),
            container(
                "shop-worker-1",
                ContainerState::Exited,
                Some(("shop", "worker")),
            ),
            container("redis", ContainerState::Running, None),
            container("old-job", ContainerState::Exited, None),
            container("paused-api", ContainerState::Paused, None),
        ];
        // The shop stack: web on both networks, db only on the internal one.
        for c in &mut s.containers {
            match c.compose.as_ref().map(|l| l.service.as_str()) {
                Some("web") => c.networks = vec!["shop_default".into(), "shop_back".into()],
                Some(_) => c.networks = vec!["shop_back".into()],
                None => {}
            }
        }
        s.images = vec![
            Image {
                id: "sha256:aaaa1111bbbb2222".into(),
                short_id: "aaaa1111bbbb".into(),
                tags: vec!["nginx:1.27".into()],
                size: 192e6,
                created: 1.7e9,
                containers: 1.0,
                dangling: false,
            },
            Image {
                id: "sha256:cccc3333dddd4444".into(),
                short_id: "cccc3333dddd".into(),
                tags: vec![],
                size: 48e6,
                created: 1.6e9,
                containers: 0.0,
                dangling: true,
            },
        ];
        s.volumes = vec![Volume {
            name: "shop_db".into(),
            driver: "local".into(),
            mountpoint: "/var/lib/docker/volumes/shop_db/_data".into(),
            created_at: None,
            compose_project: Some("shop".into()),
            anonymous: false,
            size: Some(310e6),
        }];
        let network = |name: &str, internal: bool, builtin: bool| Network {
            id: format!("{name:0<16}").replace('_', "0"),
            name: name.into(),
            driver: "bridge".into(),
            scope: "local".into(),
            internal,
            builtin,
        };
        s.networks = vec![
            network("bridge", false, true),
            network("shop_default", false, false),
            network("shop_back", true, false),
        ];
        s.usage = SystemUsage {
            cpu_used_cores: Some(1.25),
            cpu_cores: Some(8),
            mem_usage: Some(3.1e9),
            mem_total: Some(8e9),
            disk_usage: Some(42e9),
            disk_total: Some(64e9),
            disk_breakdown: Some(DiskBreakdown {
                images: 20e9,
                containers: 1e9,
                volumes: 15e9,
                build_cache: 6e9,
            }),
        };
    }

    /// Draws every page with sample data to `target/screenshots/` for a look at the UI
    /// (drawn on the CPU where there is no GPU).
    #[test]
    fn pages_render_to_png() {
        // The GPU when there is one, else the CPU renderer.
        let mut headless = Headless::new_or_software();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _enter = runtime.enter();
        let target = engine::ConnectTarget {
            host: "unix:///var/run/docker.sock".into(),
            source: "sample".into(),
            context: None,
            tls: None,
        };
        let (backend, events) =
            Backend::new(AppState::new(target), ConfigStore::new(Config::default()));
        let mut screen = DockerScreen::new(backend, events);
        sample(&mut screen.snap);
        screen.lang = Lang::En;
        let mut text = TextStore::new(FontOptions::default());
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        for page in PAGES {
            screen.show_page(page, &mut text);
            let image = headless
                .render_screen(&mut screen, &mut text, (1240, 800), 1.0)
                .expect("render");
            image
                .save_png(dir.join(format!("{page:?}.png").to_lowercase()))
                .unwrap();
        }
        // The settings with "Increase contrast" on: darker control borders.
        screen.config.increase_contrast = true;
        screen.apply_theme();
        screen.show_page(Page::Settings, &mut text);
        headless
            .render_screen(&mut screen, &mut text, (1240, 800), 1.0)
            .expect("render")
            .save_png(dir.join("settings-contrast.png"))
            .unwrap();
        screen.config.increase_contrast = false;
        screen.apply_theme();

        screen.show_page(Page::Containers, &mut text);
        let id = screen.snap.containers[0].id.clone();
        screen.with_ctx(&mut text, |view, cx| view.select_container(&id, cx));
        headless
            .render_screen(&mut screen, &mut text, (1240, 800), 1.0)
            .expect("render")
            .save_png(dir.join("containers-detail.png"))
            .unwrap();
        for (expanded, name) in [
            (false, "containers-exec.png"),
            (true, "containers-exec-expanded.png"),
        ] {
            screen.with_ctx(&mut text, |view, cx| view.open_exec(expanded, cx));
            headless
                .render_screen(&mut screen, &mut text, (1240, 800), 1.0)
                .expect("render")
                .save_png(dir.join(name))
                .unwrap();
        }
        screen.with_ctx(&mut text, |view, cx| view.open_exec(false, cx));

        // The deck's popovers, over the page they were opened from.
        screen.open_disk_deck(&mut text);
        headless
            .render_screen(&mut screen, &mut text, (1240, 800), 1.0)
            .expect("render")
            .save_png(dir.join("popover-disk.png"))
            .unwrap();
        screen.contexts = vec![
            ContextEntry {
                name: "default".into(),
                host: "unix:///var/run/docker.sock".into(),
                current: true,
                supported: true,
            },
            ContextEntry {
                name: "rootless".into(),
                host: "unix:///run/user/1000/docker.sock".into(),
                current: false,
                supported: true,
            },
        ];
        screen.open_context_chooser(&mut text);
        headless
            .render_screen(&mut screen, &mut text, (1240, 800), 1.0)
            .expect("render")
            .save_png(dir.join("popover-contexts.png"))
            .unwrap();

        // Cross-resource cleanup sheet, with named-volume warning visible.
        screen
            .tree
            .close_overlay(screen.popover.take().unwrap().root, &mut text);
        screen.show_page(Page::Containers, &mut text);
        let mut form = CleanupForm::new(Some("1.52"));
        form.set(CleanupToggle::NamedVolumes, true);
        screen.open_cleanup_sheet(Some(form), &mut text);
        std::thread::sleep(std::time::Duration::from_millis(400));
        headless
            .render_screen(&mut screen, &mut text, (1240, 800), 1.0)
            .expect("render")
            .save_png(dir.join("cleanup-sheet.png"))
            .unwrap();
        if let Some(open) = screen.cleanup.take() {
            screen.tree.close_overlay(open.sheet, &mut text);
        }

        // A stack's network map, in its sheet.
        screen.show_page(Page::Containers, &mut text);
        screen.with_ctx(&mut text, |view, cx| view.open_map("shop", cx));
        // Past the sheet's slide.
        std::thread::sleep(std::time::Duration::from_millis(400));
        headless
            .render_screen(&mut screen, &mut text, (1240, 800), 1.0)
            .expect("render")
            .save_png(dir.join("stack-map.png"))
            .unwrap();
    }
}

#[cfg(test)]
mod accessibility {
    use noa_gpui_platform::noa_gpui_core::accesskit::Role as A;
    use noa_gpui_platform::noa_gpui_text::{FontOptions, TextStore};

    use super::*;
    use crate::config::ConfigStore;
    use crate::state::AppState;

    /// With "Increase contrast", control borders reach WCAG 1.4.11's 3:1 on the
    /// window's background and surfaces in both themes; without it they stay as faint
    /// as the platform's.
    #[test]
    fn increase_contrast_brings_control_borders_to_three_to_one() {
        use noa_gpui_platform::noa_gpui_core::theme::contrast;
        for appearance in [Appearance::Light, Appearance::Dark] {
            let grounds = |t: &Theme| [t.background, t.surface, t.surface_alt];
            let raised = palette(appearance, true);
            for ground in grounds(&raised) {
                let ratio = contrast(raised.outline, ground);
                assert!(ratio >= MARK_CONTRAST - 0.01, "{appearance:?}: {ratio}");
            }
            let plain = palette(appearance, false);
            assert!(
                grounds(&plain)
                    .iter()
                    .all(|&g| contrast(plain.outline, g) < MARK_CONTRAST)
            );
            // Nothing else changes.
            assert_eq!(raised.text, plain.text);
            assert_eq!(raised.accent, plain.accent);
            assert_eq!(raised.background, plain.background);
        }
    }

    /// Every control a screen reader can reach on every page, in both languages, has a
    /// name of its own (a placeholder is not one: it goes once text is typed), with
    /// the panes that start hidden shown.
    #[test]
    fn every_control_has_an_accessible_name() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _enter = runtime.enter();
        let target = engine::ConnectTarget {
            host: "unix:///var/run/docker.sock".into(),
            source: "sample".into(),
            context: None,
            tls: None,
        };
        let (backend, events) =
            Backend::new(AppState::new(target), ConfigStore::new(Config::default()));
        let mut screen = DockerScreen::new(backend, events);
        super::screenshots::sample(&mut screen.snap);
        let mut text = TextStore::new(FontOptions::default());
        let mut unnamed = Vec::new();
        let mut controls = 0;
        for lang in [Lang::En, Lang::Ja] {
            screen.lang = lang;
            for page in PAGES {
                screen.show_page(page, &mut text);
                if page == Page::Containers {
                    // With the detail pane and its log search open.
                    let id = screen.snap.containers[0].id.clone();
                    screen.with_ctx(&mut text, |view, cx| view.select_container(&id, cx));
                }
                // Panes that start hidden (inspect, run, compose output) too.
                let mut stack = vec![screen.page_root];
                while let Some(id) = stack.pop() {
                    screen.tree.set_hidden(id, false);
                    stack.extend(screen.tree.children(id).iter().copied());
                }
                screen.tree.prepare(Viewport::new(1240.0, 800.0), &mut text);
                for (_, node) in screen.tree.accessibility_update(1.0).nodes {
                    let control = matches!(
                        node.role(),
                        A::Button
                            | A::CheckBox
                            | A::TextInput
                            | A::MultilineTextInput
                            | A::SearchInput
                            | A::Tab
                            | A::ComboBox
                            | A::Switch
                            | A::Link
                            | A::RadioButton
                            | A::MenuItem
                            | A::SpinButton
                            | A::Slider
                    );
                    if !control {
                        continue;
                    }
                    controls += 1;
                    let named = node.label().is_some_and(|l| !l.trim().is_empty())
                        || !node.labelled_by().is_empty();
                    if !named {
                        unnamed.push(format!(
                            "{lang:?} {page:?}: {:?} (placeholder {:?})",
                            node.role(),
                            node.placeholder()
                        ));
                    }
                }
            }
            screen.open_cleanup_sheet(None, &mut text);
            screen.tree.prepare(Viewport::new(1240.0, 800.0), &mut text);
            for (_, node) in screen.tree.accessibility_update(1.0).nodes {
                if matches!(node.role(), A::Button | A::CheckBox) {
                    controls += 1;
                    let named = node.label().is_some_and(|l| !l.trim().is_empty())
                        || !node.labelled_by().is_empty();
                    if !named {
                        unnamed.push(format!("{lang:?} cleanup: {:?}", node.role()));
                    }
                }
            }
            if let Some(open) = screen.cleanup.take() {
                screen.tree.close_overlay(open.sheet, &mut text);
            }
        }
        assert!(controls > 100, "only {controls} controls found");
        assert!(unnamed.is_empty(), "controls without a name: {unnamed:#?}");
    }

    fn click_on(screen: &mut DockerScreen, text: &mut TextStore, vp: Viewport, id: WidgetId) {
        use noa_gpui_platform::noa_gpui_core::{Event, PointerButton};
        screen.tree.prepare(vp, text);
        let r = screen.tree.window_rect(id).expect("visible rect");
        let pos = noa_gpui_platform::noa_gpui_scene::Point::new(
            r.x + r.width / 2.0,
            r.y + r.height / 2.0,
        );
        let button = PointerButton::Primary;
        screen
            .tree
            .dispatch(Event::PointerDown { pos, button }, text);
        screen.tree.dispatch(Event::PointerUp { pos, button }, text);
        for m in screen.tree.take_messages() {
            screen.handle(m, text);
        }
    }

    #[test]
    fn cleanup_sheet_toggles_hierarchy_warns_on_named_volumes_and_fits_window() {
        use noa_gpui_platform::noa_gpui_core::Event;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _enter = runtime.enter();
        let target = engine::ConnectTarget {
            host: "unix:///var/run/docker.sock".into(),
            source: "sample".into(),
            context: None,
            tls: None,
        };
        let (backend, events) =
            Backend::new(AppState::new(target), ConfigStore::new(Config::default()));
        let mut screen = DockerScreen::new(backend, events);
        super::screenshots::sample(&mut screen.snap);
        screen.lang = Lang::Ja;
        let mut text = TextStore::new(FontOptions::default());
        let vp = Viewport::new(960.0, 700.0);
        screen.show_page(Page::Containers, &mut text);

        // Click the deck's Clean up… button and settle the sheet's slide animation.
        let open_id = screen.sidebar.cleanup;
        click_on(&mut screen, &mut text, vp, open_id);
        for now in [0.0, 1.0] {
            screen
                .tree
                .dispatch(Event::AnimationFrame { now }, &mut text);
        }
        for m in screen.tree.take_messages() {
            screen.handle(m, &mut text);
        }
        let sheet = screen.cleanup.as_ref().expect("cleanup sheet opened");
        assert!(screen.tree.is_hidden(sheet.warning));
        assert!(!screen.tree.is_hidden(sheet.named_volumes.row));

        // Checking "Include unused named volumes" shows the warning and stays within a 960x700 window.
        let named_cb = sheet.named_volumes.checkbox;
        click_on(&mut screen, &mut text, vp, named_cb);
        screen.tree.prepare(vp, &mut text);
        let sheet = screen.cleanup.as_ref().unwrap();
        assert!(!screen.tree.is_hidden(sheet.warning));
        assert!(screen.tree.is_enabled(sheet.submit));
        for id in [
            sheet.sheet,
            sheet.containers.checkbox,
            sheet.tagged_images.checkbox,
            sheet.named_volumes.checkbox,
            sheet.warning,
            sheet.cancel,
            sheet.submit,
            screen.sidebar.cleanup,
        ] {
            let b = screen.tree.window_rect(id).expect("laid out");
            assert!(
                b.x >= 0.0 && b.right() <= 960.0 && b.bottom() <= 700.0,
                "widget {id:?} out of 960x700 bounds: {b:?}"
            );
        }

        // Unchecking parent DanglingImages unchecks TaggedImages too.
        let tagged_cb = sheet.tagged_images.checkbox;
        let dangling_cb = sheet.dangling_images.checkbox;
        click_on(&mut screen, &mut text, vp, tagged_cb);
        assert!(screen.cleanup.as_ref().unwrap().form.tagged_images);
        click_on(&mut screen, &mut text, vp, dangling_cb);
        assert!(!screen.cleanup.as_ref().unwrap().form.tagged_images);

        // Cancel button dismisses the sheet cleanly.
        let cancel_id = screen.cleanup.as_ref().unwrap().cancel;
        click_on(&mut screen, &mut text, vp, cancel_id);
        assert!(screen.cleanup.is_none());
    }
}
