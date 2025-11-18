# 🐳 DockrTUI

A fast, modern and keyboard-driven terminal dashboard for Docker — built with Rust and `ratatui`.  
Because managing containers shouldn’t feel like typing spells in Bash.

---

<p align="center">
  <video
    src="https://github.com/user-attachments/assets/2ea68c8f-1e4c-4efc-a7c8-4e34972a6928"
    width="900"
    controls
  >
    Your browser does not support the video tag.
  </video>
</p>

> 🎥 *See DockrTUI in action!*  
> Browse containers, networks & compose projects at lightning speed — all from your terminal.

---

## 🚀 Features

- **Containers, Images, Networks, Volumes & Compose** — all in one place  
- **Quick actions** — start, stop, restart, inspect, prune  
- **Smart search and filtering**  
- **Built-in shell** inside containers (`cd`, history, autocomplete, etc.)  
- **Compose integration** — detect and control your Compose projects  
- **Volume management** — list, inspect, delete, and prune unused volumes
- **Clean, efficient TUI** powered by `ratatui`  

---

## ⚡ Installation

You can install DockrTUI directly with Cargo:

```bash
cargo install dockrtui
```

Once installed, run it from anywhere:

```bash
dockrtui
```

> 🐧 Requires Docker CLI installed and running.
> Tested on Linux and WSL2.

---
## 🔌 Docker Socket Detection



DockrTUI automatically detects which Docker socket to use:



**Priority:**



1. `DOCKER_HOST` environment variable (same as Docker CLI)

2. Rootless Docker socket: `/run/user/$UID/docker.sock`

3. Classic rootful Docker socket: [docker.sock](http://_vscodecontentref_/0)



This means DockrTUI works out-of-the-box with:



- Docker Desktop

- Docker rootless mode

- Podman (via Docker API compatibility)

- Custom Docker contexts (`docker context use …`)



If you want to explicitly override the socket:



```bash

export DOCKER_HOST=unix:///run/user/1000/docker.sock

dockrtui

```
---
## 🕹️ Usage

Navigate everything with your keyboard:

| Key        | Action                 |
| ---------- | ---------------------- |
| `Tab`      | Switch tab             |
| `↑` / `↓`  | Navigate               |
| `Enter` / `Space`    | Start / stop container |
| `r` / `F5` | Refresh                |
| `b`        | Open built-in shell    |
| `t`        | Show stats             |
| `l`        | Show logs              |
| `q`        | Quit                   |

---

## 🧭 Tabs Overview

* **Containers** → view, start, stop, restart, inspect
* **Images** → list, remove, check creation date and size
* **Networks** → inspect, clean, or create networks
* **Compose** → view detected Compose projects and run `up`, `down`, `logs`, etc.
* **Volumes** → list, inspect, delete, and prune unused volumes

---

## 🐚 Built-in Shell

Each container can be opened in an interactive shell directly from the UI.
Supports `cd`, autocompletion with `Tab`, and persistent working directory.

---

## 🛠️ Requirements

* Docker CLI (`docker ps` must work)
* UTF-8 terminal with 256 colors
* Rust toolchain (only for installation)

---

## 💡 Example

```bash
dockrtui
```

Then use `Tab` to move between Containers, Images, Networks, and Compose.

---

## ⚖️ License

MIT License © OrbitNet

Built with ❤️ in Rust.
