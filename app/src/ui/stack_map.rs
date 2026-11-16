//! The network map of a compose stack: its published ports, its services and the
//! networks they join, in three columns joined by lines.

use std::collections::BTreeSet;

use noa_gpui_platform::noa_gpui_core::style::{AlignItems, JustifyContent, Size, length};
use noa_gpui_platform::noa_gpui_core::widgets::{Button, Container, Label, Sheet};
use noa_gpui_platform::noa_gpui_core::{PaintCx, Role, Style, UiTree, Widget, WidgetId};
use noa_gpui_platform::noa_gpui_scene::Point;

use super::i18n::Tr;
use super::model;
use super::{tokens, widgets};
use crate::dto::{Container as DockerContainer, Network};

/// A service of the stack: one box per container.
#[derive(Debug, Clone, PartialEq)]
pub struct MapService {
    pub name: String,
    pub state: ServiceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Running,
    Paused,
    Stopped,
}

/// A network the stack's services join.
#[derive(Debug, Clone, PartialEq)]
pub struct MapNetwork {
    pub name: String,
    /// Driver, and "internal" for a network with no route out.
    pub detail: String,
}

/// What the map shows. Links are (left, right) indexes into the columns either side.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StackMap {
    /// Host port → container port ("8080→80/tcp").
    pub ports: Vec<String>,
    pub services: Vec<MapService>,
    pub networks: Vec<MapNetwork>,
    pub port_links: Vec<(usize, usize)>,
    pub network_links: Vec<(usize, usize)>,
}

/// The map of compose project `project` from the engine's containers and networks.
pub fn stack_map(project: &str, containers: &[DockerContainer], networks: &[Network]) -> StackMap {
    let mut members: Vec<&DockerContainer> = containers
        .iter()
        .filter(|c| c.compose.as_ref().is_some_and(|l| l.project == project))
        .collect();
    members.sort_by(|a, b| {
        service_name(a)
            .cmp(service_name(b))
            .then(a.name.cmp(&b.name))
    });

    let joined: BTreeSet<&str> = members
        .iter()
        .flat_map(|c| c.networks.iter().map(String::as_str))
        .collect();
    let mut map = StackMap {
        networks: joined
            .iter()
            .map(|&name| MapNetwork {
                name: name.to_owned(),
                detail: network_detail(networks.iter().find(|n| n.name == name)),
            })
            .collect(),
        ..Default::default()
    };
    for (s, c) in members.iter().enumerate() {
        map.services.push(MapService {
            name: service_name(c).to_owned(),
            state: service_state(c),
        });
        let mut seen = BTreeSet::new();
        for port in c.ports.iter().filter_map(|p| host_port(p)) {
            if seen.insert(port.clone()) {
                map.port_links.push((map.ports.len(), s));
                map.ports.push(port);
            }
        }
        for name in &c.networks {
            if let Some(n) = map.networks.iter().position(|n| &n.name == name) {
                map.network_links.push((s, n));
            }
        }
    }
    map
}

fn service_name(c: &DockerContainer) -> &str {
    c.compose
        .as_ref()
        .map_or(c.name.as_str(), |l| l.service.as_str())
}

fn service_state(c: &DockerContainer) -> ServiceState {
    match c.state {
        crate::dto::ContainerState::Paused => ServiceState::Paused,
        _ if model::is_running(c) => ServiceState::Running,
        _ => ServiceState::Stopped,
    }
}

fn network_detail(n: Option<&Network>) -> String {
    match n {
        Some(n) if n.internal => format!("{} · internal", n.driver),
        Some(n) => n.driver.clone(),
        None => String::new(),
    }
}

/// "0.0.0.0:8080→80/tcp" (or "[::]:8080->80/tcp") without the address: "8080→80/tcp".
/// The IPv4 and IPv6 bindings of one port come out the same, so they show once.
fn host_port(published: &str) -> Option<String> {
    let (host, container) = published
        .split_once('→')
        .or_else(|| published.split_once("->"))?;
    let port = host.rsplit_once(':').map_or(host, |(_, p)| p);
    Some(format!("{port}→{container}"))
}

// ---- drawing ----

/// Height of a box in a column, and the space between two boxes.
const BOX: f32 = 40.0;
const BOX_GAP: f32 = tokens::SPACE_3;
/// Width of a column of boxes, and of the lines between two columns.
const COLUMN: f32 = 190.0;
const LINKS: f32 = 72.0;
/// noa-gpui's `Sheet` pads its content by 20 px on every side.
const SHEET_PADDING: f32 = 20.0;

/// Height of the map: its longest column.
fn map_height(map: &StackMap) -> f32 {
    let rows = map
        .ports
        .len()
        .max(map.services.len())
        .max(map.networks.len());
    column_height(rows.max(1))
}

fn column_height(rows: usize) -> f32 {
    rows as f32 * BOX + rows.saturating_sub(1) as f32 * BOX_GAP
}

/// The middle of box `index` in a column of `rows` boxes, centered in `height`.
fn box_middle(index: usize, rows: usize, height: f32) -> f32 {
    (height - column_height(rows)) / 2.0 + index as f32 * (BOX + BOX_GAP) + BOX / 2.0
}

/// The lines between two columns of boxes: from the right edge of a box on the left to
/// the left edge of one on the right.
struct Links {
    left: usize,
    right: usize,
    links: Vec<(usize, usize)>,
    height: f32,
}

impl Widget for Links {
    fn style(&self) -> Style {
        Style {
            size: Size {
                width: length(LINKS),
                height: length(self.height),
            },
            flex_shrink: 0.0,
            ..Default::default()
        }
    }

    fn paint(&mut self, cx: &mut PaintCx) {
        let b = cx.bounds();
        let color = cx.color(Role::Muted);
        let bend = LINKS / 4.0;
        for &(l, r) in &self.links {
            let from = Point::new(b.x, b.y + box_middle(l, self.left, self.height));
            let to = Point::new(b.x + b.width, b.y + box_middle(r, self.right, self.height));
            let a = Point::new(from.x + bend, from.y);
            let z = Point::new(to.x - bend, to.y);
            cx.line(from, a, tokens::RULE, color);
            cx.line(a, z, tokens::RULE, color);
            cx.line(z, to, tokens::RULE, color);
        }
    }
}

/// The open map: its sheet, and the button that closes it.
pub struct MapSheet {
    pub sheet: WidgetId,
    pub close: WidgetId,
}

impl MapSheet {
    /// Opens the map of `project` in a sheet over the window.
    pub fn open(tree: &mut UiTree, t: Tr, project: &str, map: &StackMap) -> Self {
        let width = 3.0 * COLUMN + 2.0 * LINKS + 2.0 * SHEET_PADDING;
        let sheet = Sheet::new(width).open(tree, 0.0);
        let body = tree.insert(Some(sheet), Container::column().gap(tokens::SPACE_4));
        let head = tree.insert(Some(body), widgets::row(tokens::SPACE_3));
        tree.insert(
            Some(head),
            Label::new(t.network_map_title(project))
                .weight(tokens::WEIGHT_STRONG)
                .font_size(tokens::TEXT_MD),
        );

        let titles = tree.insert(Some(body), widgets::row(LINKS));
        for title in [t.map_ports(), t.map_services(), t.nav_networks()] {
            let cell = tree.insert(Some(titles), Container::column().width(COLUMN));
            tree.insert(
                Some(cell),
                Label::new(title)
                    .font_size(tokens::TEXT_XS)
                    .color(Role::Muted)
                    .font_family(widgets::MONO),
            );
        }

        let height = map_height(map);
        let graph = tree.insert(Some(body), Container::row().align_items(AlignItems::START));
        let ports = column(tree, graph, height);
        if map.ports.is_empty() {
            tree.insert(
                Some(ports),
                Label::new(t.map_no_ports())
                    .font_size(tokens::TEXT_SM)
                    .color(Role::Muted),
            );
        }
        for port in &map.ports {
            node(tree, ports, port, None, None);
        }
        tree.insert(
            Some(graph),
            Links {
                left: map.ports.len(),
                right: map.services.len(),
                links: map.port_links.clone(),
                height,
            },
        );
        let services = column(tree, graph, height);
        for s in &map.services {
            let theme = tree.theme();
            let dot = match s.state {
                ServiceState::Running => theme.success,
                ServiceState::Paused => theme.warning,
                ServiceState::Stopped => theme.muted,
            };
            node(tree, services, &s.name, None, Some(dot));
        }
        tree.insert(
            Some(graph),
            Links {
                left: map.services.len(),
                right: map.networks.len(),
                links: map.network_links.clone(),
                height,
            },
        );
        let networks = column(tree, graph, height);
        for n in &map.networks {
            node(tree, networks, &n.name, Some(&n.detail), None);
        }

        let foot = tree.insert(Some(body), widgets::row(tokens::SPACE_3));
        tree.insert(Some(foot), widgets::spacer());
        let close = tree.insert(Some(foot), Button::new(t.close()).cancel_button(true));
        Self { sheet, close }
    }

    pub fn close(&self, tree: &mut UiTree) {
        Sheet::close(tree, self.sheet);
    }
}

/// A column of boxes `height` tall, the boxes in its middle.
fn column(tree: &mut UiTree, parent: WidgetId, height: f32) -> WidgetId {
    tree.insert(
        Some(parent),
        Container::column()
            .width(COLUMN)
            .height(height)
            .gap(BOX_GAP)
            .justify_content(JustifyContent::CENTER),
    )
}

/// A box: a name, a smaller second line, and a state dot before them.
fn node(
    tree: &mut UiTree,
    parent: WidgetId,
    name: &str,
    detail: Option<&str>,
    dot: Option<noa_gpui_platform::noa_gpui_scene::Color>,
) {
    let b = tree.insert(
        Some(parent),
        Container::new(Style {
            flex_direction: noa_gpui_platform::noa_gpui_core::style::FlexDirection::Row,
            align_items: AlignItems::CENTER,
            gap: widgets::spacing(tokens::SPACE_2),
            padding: widgets::padding_xy(tokens::SPACE_3, 0.0),
            size: Size {
                width: length(COLUMN),
                height: length(BOX),
            },
            flex_shrink: 0.0,
            ..Default::default()
        })
        .radius(6.0)
        .border(tokens::RULE, Role::Outline)
        .background(Role::Surface),
    );
    if let Some(color) = dot {
        tree.insert(
            Some(b),
            Container::column()
                .width(tokens::DOT)
                .height(tokens::DOT)
                .radius(tokens::DOT / 2.0)
                .background(color),
        );
    }
    let text = tree.insert(Some(b), Container::column().grow(1.0));
    tree.insert(
        Some(text),
        Label::new(name)
            .font_size(tokens::TEXT_SM)
            .font_family(widgets::MONO),
    );
    if let Some(detail) = detail.filter(|d| !d.is_empty()) {
        tree.insert(
            Some(text),
            Label::new(detail)
                .font_size(tokens::TEXT_XS)
                .color(Role::Muted),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{ComposeLabels, ContainerState};

    fn container(
        name: &str,
        project: &str,
        state: ContainerState,
        ports: &[&str],
        nets: &[&str],
    ) -> DockerContainer {
        DockerContainer {
            id: name.into(),
            name: format!("{project}-{name}-1"),
            image: "img".into(),
            state,
            status: String::new(),
            created: 0.0,
            ports: ports.iter().map(|p| (*p).to_owned()).collect(),
            compose: Some(ComposeLabels {
                project: project.into(),
                service: name.into(),
            }),
            networks: nets.iter().map(|n| (*n).to_owned()).collect(),
            volumes: Vec::new(),
        }
    }

    fn network(name: &str, internal: bool) -> Network {
        Network {
            id: name.into(),
            name: name.into(),
            driver: "bridge".into(),
            scope: "local".into(),
            internal,
            builtin: false,
        }
    }

    #[test]
    fn a_stack_maps_ports_to_services_to_networks() {
        let containers = [
            container(
                "web",
                "shop",
                ContainerState::Running,
                &["0.0.0.0:8080→80/tcp", "[::]:8080→80/tcp"],
                &["shop_default", "shop_back"],
            ),
            container("db", "shop", ContainerState::Exited, &[], &["shop_back"]),
            container(
                "other",
                "blog",
                ContainerState::Running,
                &["0.0.0.0:9000→9000/tcp"],
                &["blog_default"],
            ),
        ];
        let networks = [network("shop_default", false), network("shop_back", true)];
        let map = stack_map("shop", &containers, &networks);

        // Services by name; the other stack is left out.
        let names: Vec<_> = map.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["db", "web"]);
        assert_eq!(map.services[0].state, ServiceState::Stopped);
        assert_eq!(map.services[1].state, ServiceState::Running);
        // One port for the IPv4 and IPv6 bindings, to web.
        assert_eq!(map.ports, ["8080→80/tcp"]);
        assert_eq!(map.port_links, [(0, 1)]);
        // Networks by name, internal marked; db joins back, web both.
        let nets: Vec<_> = map
            .networks
            .iter()
            .map(|n| (n.name.as_str(), n.detail.as_str()))
            .collect();
        assert_eq!(
            nets,
            [
                ("shop_back", "bridge · internal"),
                ("shop_default", "bridge")
            ]
        );
        assert_eq!(map.network_links, [(0, 0), (1, 1), (1, 0)]);
    }

    #[test]
    fn host_ports_drop_the_address() {
        assert_eq!(
            host_port("0.0.0.0:8080->80/tcp").as_deref(),
            Some("8080→80/tcp")
        );
        assert_eq!(host_port(":::53→53/udp").as_deref(), Some("53→53/udp"));
        assert_eq!(host_port("[::1]:53→53/udp").as_deref(), Some("53→53/udp"));
        assert_eq!(host_port("80/tcp"), None);
    }

    #[test]
    fn links_meet_the_middle_of_their_boxes() {
        // Three boxes fill the height; one box sits in its middle.
        let height = column_height(3);
        assert_eq!(box_middle(0, 3, height), BOX / 2.0);
        assert_eq!(box_middle(2, 3, height), height - BOX / 2.0);
        assert_eq!(box_middle(0, 1, height), height / 2.0);
    }
}
