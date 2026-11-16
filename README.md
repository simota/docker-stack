# Docker Stack

A desktop GUI for the local Docker Engine: containers, logs, exec, stats, images, volumes, networks and Compose projects.

## Install

There are no prebuilt packages yet. Clone the repository and build from source with `scripts/install.sh`.

```sh
git clone git@github.com:simota/docker-stack.git
cd docker-stack
./scripts/install.sh --deps
```

`--deps` first installs the build dependencies listed under Requirements. You can leave it off on later runs.

### Requirements

- Rust (`cargo`): <https://rustup.rs>
- The [noa-gpui](https://github.com/simota/noa-gpui) UI toolkit. cargo fetches it from crates.io (`noa-gpui-platform`); nothing to check out by hand
- A running Docker daemon (Docker Engine on Linux, Docker Desktop or similar on macOS)
- Linux only: a C compiler, pkg-config, and Vulkan (or OpenGL) drivers. `--deps` installs them with apt or dnf/yum (needs sudo)
- macOS only: the Xcode Command Line Tools. `--deps` runs `xcode-select --install`

### Where it installs

| OS | Method | What it does |
|---|---|---|
| Linux | `user` (default) | Puts the binary in `~/.local/bin` and a `.desktop` entry and icons in `~/.local/share` (no sudo) |
| macOS | `app` (default) | Builds `Docker Stack.app` and copies it to `/Applications` |

`--prefix` changes the target directory.

```sh
# macOS, into ~/Applications
./scripts/install.sh --prefix ~/Applications
```

`./scripts/install.sh --help` lists every option. `./scripts/install.sh --check` only reports the build requirements and the state of the Docker daemon and `docker compose`, without building anything.

On macOS, `./scripts/build-app.sh` builds `Docker Stack.app` without installing it. The bundle lands at `target/release/Docker Stack.app` (change the directory with `--out`).

To run it from the checkout without installing: `cargo run -p docker-stack`.

### Launch

- Linux: open "Docker Stack" from the application menu, or run `docker-stack`
- macOS: open it from Launchpad, or run `open "/Applications/Docker Stack.app"`

If the app cannot reach the daemon on Linux, add your user to the `docker` group and log in again:

```sh
sudo usermod -aG docker $USER
```

With the tray on (Settings → Background), the app shows a tray icon (in the menu bar on macOS) and keeps running when the window is closed; quit from the tray menu. On Linux the tray needs a panel that supports StatusNotifierItem (KDE, or GNOME with the AppIndicator extension). Wayland does not let apps hide their windows, so closing behaves as the desktop decides.

### Update

Pull the latest code and install again:

```sh
git pull
./scripts/install.sh
```

### Uninstall

```sh
./scripts/install.sh --uninstall
```

Pass the same `--prefix` you installed with. Settings (`~/.config/docker-stack/config.toml`) are kept; delete the file by hand if you no longer need it.

## Usage and reference

### Keyboard shortcuts

On macOS, use `⌘` instead of `Ctrl`.

| Key | Action |
|---|---|
| `Ctrl+1` – `Ctrl+4` | Open Containers / Images / Volumes / Networks |
| `Ctrl+,` | Open Settings |
| `Ctrl+F` | Focus the container search |
| `Ctrl+R` | Reload every list |
| `Ctrl+L` / `Ctrl+E` | Open logs / exec for the selected container |
| `Esc` | Clear the selection and close the detail pane (Inspect on Images, Volumes and Networks) |
| `Delete` | Remove the selected containers, images, volumes or networks (asks first; built-in networks are left out) |

The icon buttons at the right end of each row act on that row:

- Container rows: start, stop, restart, pause or resume, kill, logs, exec and remove (the set follows the container's state)
- Stack rows: Map (a diagram of the stack's services with their published ports and the networks they join), Up, Start, Restart, Stop, Pull and Down. Click the stack row to collapse or expand it. Stacks are listed after the standalone containers
- Rows on the other lists (Images, Volumes, Networks): Inspect and remove. Inspect on a network lists the containers attached to it

Run on an image's row opens a form beside the list that creates and starts a container from it, like `docker run -d`. Its fields are a name, a command, published ports (`8080:80`, `127.0.0.1:5432:5432`, `53/udp`), named volumes (`data:/var/lib/data`, `cache:/cache:ro`; one that does not exist yet is created), a network, a restart policy and environment variables (`KEY=value`, one per line). Every field is checked before anything reaches the daemon, and the message names the field that is wrong. Host paths cannot be mounted. If the container does not start (a port already taken, say), it is removed again so the same name can be used on the next try; the form keeps what was typed for that image.

With containers selected, "Start selected" and "Stop selected" start the stopped ones or stop the running ones among them.

Exec is a terminal attached to a shell in the container; full-screen programs such as vim or top work. Each container keeps its shell while you switch to another tab or container. Before "New shell" you can set the command (bash if the container has it, else sh), the user and the working directory. When the shell ends, Enter or "Reconnect" opens a new one.

"Clean up…" (in the top bar, and in the disk usage popover) opens a sheet that removes unused items across resource types, in dependency order:

1. Stopped containers
2. Unused custom networks
3. Dangling images (optionally unused tagged images too)
4. Unused anonymous volumes (optionally unused named volumes too, with a warning that their data cannot be recovered)
5. Unused build cache

Running and paused containers are never touched. The counts and sizes the sheet shows are estimates. Afterwards a notice gives what was removed and reclaimed, and which step failed if one did.

"Duplicate…" in a container's detail pane opens the Run form filled in with that container's settings: its image, command, published ports, named volumes, network, restart policy and environment variables (minus what the image already sets), named `<name>-copy`. Change what you need (a port, an environment variable) and Run; the form is checked as if you had typed it. When the image tag now names a different image than the container was created from, the form runs the original image by its id.

What the form cannot carry is left out and listed in a notice:

- Bind mounts and tmpfs (host paths cannot be mounted from the app)
- Anonymous volumes (the copy gets fresh ones)
- A changed entrypoint
- `--network container:…`
- Named volumes whose container path has a space, comma or colon
- Environment variables whose value cannot be one line of the form (a line break, or spaces at either end); only their names are shown
- Any other setting the form has no field for (resource limits such as `--memory` and `--cpus`, `--privileged`, `--cap-add`, `--device`, `--user`, `--label`, `--add-host`, `--log-opt` and so on), shown as its `docker run` option

The daemon's own defaults and the image's settings are not listed.

"Networks…" in a container's detail pane opens a dialog with a button to connect the container to each other network and one to disconnect it from each network it is on (as `docker network connect` / `disconnect` do; the change applies without a restart). A container on the host's network, on none, or sharing another container's network cannot change its networks.

"Update…" in a container's detail pane pulls the image tag the container was created from (`nginx:1.27`, say). When the tag now names a different image, it asks, then replaces the container with one made from that image. The container's settings (everything given to `docker run`, bind mounts and resource limits included), its name, its volumes (anonymous ones keep their data) and its networks (with aliases and static addresses) stay as they were. What the image decides (variables such as `PATH`, the command, labels) comes from the new image.

A running container stops for a moment and the new one starts; a stopped one is replaced and stays stopped. When a step fails, the new container is removed, and the original gets its name back and is started again. When the tag has not moved, a notice says so and nothing is replaced. When the pull fails but the local tag already names a newer image (one built locally, say), that image is used.

"Update…" does not apply to every container:

- Containers of a stack are updated with the stack's Pull and Up instead
- A container created from an image ID has no tag to update from
- One started with `--rm` is refused, since stopping it would delete it with nothing to put back

"Check for updates" in the Containers header looks for a newer image of the tag each listed container was created from, without pulling anything (the daemon asks the registry for the tag's digest, once per tag). A container with a newer image says "Newer image available" under its status, and one whose tag already names a newer local image (pulled on the Images page, say) says "Newer image pulled". A notice gives the count, and how many could not be checked (an image only built locally, a registry that wants credentials). Containers of stacks are checked too, so it also tells when a stack's Pull and Up are due. When the check finds standalone containers with a newer image, "Update all (n)…" appears in the header. It lists them and asks, then updates each the way "Update…" does; one that fails does not stop the rest.

When a container stops on its own with a non-zero exit code, or the kernel kills it for running out of memory, a notice names it and the exit code. Containers stopped, restarted or killed through Docker (from the app, the CLI or compose) are not reported, and a container a restart policy keeps bringing back is reported at most once a minute. A container with a health check (`HEALTHCHECK`) that turns unhealthy is reported too, even while it keeps running (at most once a minute per container, counted apart from exits). This works for every container on the daemon, not only those started from the app.

### Which daemon it connects to

The first match wins:

1. The Docker context pinned in Settings (pick it in Settings or with the connection button at the right of the top bar)
2. The `DOCKER_HOST` environment variable
3. The current Docker context (`DOCKER_CONTEXT`, else `currentContext` in `~/.docker/config.json`)
4. The rootless Docker socket (`$XDG_RUNTIME_DIR/docker.sock`, only when `/var/run/docker.sock` is missing)
5. `/var/run/docker.sock`

SSH contexts are not supported.

If the local daemon is not running at launch, the app starts it: on macOS it opens Docker Desktop; on Linux it runs `systemctl` (through `pkexec` for the system Docker) to start `docker.service` or `docker-desktop.service`. When the socket location does not say how to start the daemon (colima, for example), it leaves it alone. The "cannot reach" banner also has a button to start it by hand.

### Settings file

Settings live in `$XDG_CONFIG_HOME/docker-stack/config.toml` (default `~/.config/docker-stack/config.toml`). Change them in Settings or edit the file; unknown keys are ignored. A file that is not valid TOML is moved to `config.toml.invalid` and the app starts from the defaults, so a hand edit is not saved over.

| Key | Default | Meaning |
|---|---|---|
| `context` | none | Docker context to pin; none follows the CLI default |
| `log_lines` | `10000` | Lines kept per open log (100–1,000,000) |
| `theme` | `"system"` | `"system"` / `"light"` / `"dark"` |
| `show_stopped` | `true` | Show stopped containers |
| `language` | `"system"` | `"system"` / `"en"` / `"ja"` |
| `tray` | `false` | Show a tray icon and keep running when the window is closed |
| `increase_contrast` | `false` | Draw the borders of text fields, buttons and checkboxes at 3:1 or more against the background (WCAG 1.4.11) |

### Docker settings and network access

- Docker's own configuration (under `~/.docker`) is only read, never written.
- The app talks only to the Docker daemon. Image pulls, and the registry lookups of "Check for updates", are done by the daemon.
- The window can only send a fixed set of operations. It cannot pass arbitrary Docker API calls or host paths: Run mounts named volumes only, and an environment variable needs its value (a bare `KEY` is not copied from the app's environment).
- "Copy redacted" in Inspect replaces environment values that look like passwords or tokens with `<redacted>`.
