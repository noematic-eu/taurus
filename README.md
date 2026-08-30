# Taurus

<p align="center">
  <img src="src-tauri/icons/128x128.png" alt="Taurus" width="96" height="96" />
</p>

<p align="center">
  <strong>A desktop player for zipped static websites.</strong><br />
  The zip changes; the app does not.
</p>

<p align="center">
  <a href="#english">English</a> · <a href="#français">Français</a>
  &nbsp;·&nbsp;
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license" /></a>
  <img src="https://img.shields.io/badge/tauri-2-24c8db.svg" alt="Tauri 2" />
  <img src="https://img.shields.io/badge/version-0.1.0-1e3a4c.svg" alt="Version 0.1.0" />
</p>

Taurus opens a `.zip` that contains a static site (`OUVRIR.html` or `index.html`) in its own window. Course packs, lab materials, offline docs — the content lives in the zip, not in the binary.

Built with [Tauri 2](https://tauri.app/). The name is a pun on the framework; Taurus is the app, not Tauri.

---

## English

### What it does

Drop a zip, pick one from the file dialog, or pass a path on the command line. Taurus:

1. Extracts the archive to a temporary folder
2. Finds the entry page (`OUVRIR.html`, `ouvrir.html`, `index.html`, or `index.htm`)
3. Serves the files over HTTP on `127.0.0.1`
4. Opens a dedicated window on that local URL
5. Stops the server and deletes the temp files when the window closes

Several packs can be open at once. Updating a course means shipping a new zip, not a new installer.

### Pack format

A pack is a zip of a static website.

```text
course.zip
├── OUVRIR.html      # or index.html
├── styles.css
└── assets/
```

A single wrapper folder is unwrapped automatically (common when zipping a directory):

```text
native.zip
└── native/
    └── index.html
```

The pack HTML, CSS, and JavaScript run as a normal website. They have **no access** to Tauri APIs (file system, dialogs, shell, …).

### Security

| Measure | Detail |
|---|---|
| Isolation | Pack windows (`pack-*`) have an empty capability set |
| Loopback only | HTTP server binds to `127.0.0.1`, never the LAN |
| Path traversal | Requests outside the extracted root are rejected |
| Headers | `X-Content-Type-Options: nosniff`, `Cache-Control: no-store` |

Taurus is a viewer, not a sandbox for untrusted code. Treat a zip like a website you chose to open.

### Requirements

- **macOS** — Apple Silicon or Intel
- **Windows** — [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/) (bundled with recent Windows 10/11)
- **Linux** — WebKitGTK (build from source)

Unsigned builds may be blocked by Gatekeeper (macOS) or SmartScreen (Windows). Right-click → Open, or allow the app in system settings.

### Develop

[Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) (Rust, system webview) plus Node.js 22+:

```bash
cd taurus
npm install
npm run tauri dev
```

Pass zip paths as arguments to open them on launch:

```bash
npm run tauri dev -- -- /path/to/pack.zip
```

### Build

```bash
npx tauri build
```

Outputs depend on the host OS (`.dmg` / `.app` on macOS, NSIS / MSI / `.exe` on Windows).

Push a `v*` tag (or run the workflow by hand) to build **macOS and Windows** via [`.github/workflows/release.yml`](.github/workflows/release.yml). Windows cannot be cross-compiled from macOS.

### Icon

The mark is a geometric Taurus bull with an open book — the zip is the course, the app is the reader.

Master artwork: [`src-tauri/app-icon.png`](src-tauri/app-icon.png) (1024×1024). Regenerate macOS, Windows, Linux, iOS and Android sizes:

```bash
npx tauri icon src-tauri/app-icon.png
```

### Dependency audit

```bash
# once: brew install cargo-audit cargo-deny
npm run audit
```

That runs `npm audit`, [`cargo deny`](https://embarkstudios.github.io/cargo-deny/) (`src-tauri/deny.toml`) and `cargo audit`. CI repeats it on every push: [`.github/workflows/audit.yml`](.github/workflows/audit.yml).

Current lockfiles have **no known vulnerabilities**. Documented exceptions in `deny.toml`:

- GTK3 crates marked unmaintained (Linux WebKitGTK backend of Tauri)
- `unic-*` via `urlpattern` in `tauri-utils`

### Limitations

- Content must already be HTML. Markdown packs are not rendered.
- One native binary per OS; there is no portable “just a zip of the player”.
- Linux is supported at the Tauri level but not built in CI yet.

### License

[MIT](LICENSE) — © 2026 Baptiste Boussemart.

---

## Français

### À quoi ça sert

Glissez un zip, choisissez-le dans le dialogue, ou passez un chemin en ligne de commande. Taurus :

1. extrait l’archive dans un dossier temporaire ;
2. trouve la page d’entrée (`OUVRIR.html`, `ouvrir.html`, `index.html` ou `index.htm`) ;
3. sert les fichiers en HTTP sur `127.0.0.1` ;
4. ouvre une fenêtre dédiée sur cette URL locale ;
5. arrête le serveur et supprime les fichiers temporaires à la fermeture de la fenêtre.

Plusieurs packs peuvent être ouverts en même temps. Mettre à jour un cours, c’est envoyer un nouveau zip — pas un nouvel installeur.

### Format d’un pack

Un pack est un zip contenant un site statique.

```text
cours.zip
├── OUVRIR.html      # ou index.html
├── styles.css
└── assets/
```

Un dossier enveloppe unique est déroulé automatiquement (cas fréquent quand on zippe un répertoire) :

```text
native.zip
└── native/
    └── index.html
```

Le HTML, le CSS et le JavaScript du pack s’exécutent comme un site web normal. Ils **n’ont pas accès** aux API Tauri (fichier, dialogues, shell, …).

### Sécurité

| Mesure | Détail |
|---|---|
| Isolation | Les fenêtres de pack (`pack-*`) n’ont aucune capability |
| Boucle locale | Le serveur HTTP écoute `127.0.0.1`, jamais le réseau local |
| Traversée de chemin | Toute requête hors de la racine extraite est refusée |
| En-têtes | `X-Content-Type-Options: nosniff`, `Cache-Control: no-store` |

Taurus est un lecteur, pas un bac à sable pour du code non fiable. Un zip se traite comme un site que l’on a choisi d’ouvrir.

### Prérequis

- **macOS** — Apple Silicon ou Intel
- **Windows** — [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/) (inclus dans Windows 10/11 récents)
- **Linux** — WebKitGTK (compilation depuis les sources)

Les binaires non signés peuvent être bloqués par Gatekeeper (macOS) ou SmartScreen (Windows). Clic droit → Ouvrir, ou autoriser l’app dans les réglages système.

### Développement

[Prérequis Tauri 2](https://v2.tauri.app/start/prerequisites/) (Rust, webview système) et Node.js 22+ :

```bash
cd taurus
npm install
npm run tauri dev
```

Passez des chemins de zip en arguments pour les ouvrir au lancement :

```bash
npm run tauri dev -- -- /chemin/vers/pack.zip
```

### Compilation

```bash
npx tauri build
```

Les artefacts dépendent de l’OS hôte (`.dmg` / `.app` sur macOS, NSIS / MSI / `.exe` sur Windows).

Poussez un tag `v*` (ou lancez le workflow à la main) pour construire **macOS et Windows** via [`.github/workflows/release.yml`](.github/workflows/release.yml). Windows ne se compile pas en croisé depuis un Mac.

### Icône

Le pictogramme est un taureau géométrique avec un livre ouvert — le zip est le cours, l’appli est le lecteur.

Source : [`src-tauri/app-icon.png`](src-tauri/app-icon.png) (1024×1024). Régénérer les tailles macOS, Windows, Linux, iOS et Android :

```bash
npx tauri icon src-tauri/app-icon.png
```

### Audit des dépendances

```bash
# une fois : brew install cargo-audit cargo-deny
npm run audit
```

Cela lance `npm audit`, [`cargo deny`](https://embarkstudios.github.io/cargo-deny/) (`src-tauri/deny.toml`) et `cargo audit`. La CI le rejoue à chaque push : [`.github/workflows/audit.yml`](.github/workflows/audit.yml).

Les lockfiles actuels n’ont **aucune vulnérabilité connue**. Exceptions documentées dans `deny.toml` :

- crates GTK3 marquées non maintenues (backend WebKitGTK Linux de Tauri)
- `unic-*` via `urlpattern` dans `tauri-utils`

### Limites

- Le contenu doit déjà être du HTML. Les packs Markdown ne sont pas rendus.
- Un binaire natif par OS ; il n’existe pas de lecteur « juste un zip ».
- Linux est géré au niveau Tauri, mais pas encore construit en CI.

### Licence

[MIT](LICENSE) — © 2026 Baptiste Boussemart.
