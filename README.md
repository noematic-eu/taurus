# Taurus

<p align="center">
  <img src="src-tauri/icons/128x128.png" alt="Taurus" width="96" height="96" />
</p>

<p align="center">
  <strong>A desktop player for a site folder, a zip, or a WACZ web archive.</strong><br />
  The archive changes; the app does not.
</p>

<p align="center">
  <a href="#english">English</a> · <a href="#français">Français</a>
  &nbsp;·&nbsp;
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license" /></a>
  <img src="https://img.shields.io/badge/tauri-2-24c8db.svg" alt="Tauri 2" />
  <img src="https://img.shields.io/badge/version-0.2.0-1e3a4c.svg" alt="Version 0.2.0" />
</p>

Taurus opens a site folder, a `.zip` (static site: `OUVRIR.html` or `index.html`), or a `.wacz` web archive in its own window. Course packs, Manning liveBooks, lab materials — the content lives in the folder or the archive, not in the binary.

Built with [Tauri 2](https://tauri.app/). The name is a pun on the framework; Taurus is the app, not Tauri.

---

## English

### What it does

Drop a folder, zip, or WACZ, pick one from the file dialog, pass a path on the command line, double-click an archive, or drop a folder or an archive on the app icon (bundled app, via macOS file associations). Taurus:

1. Serves a folder in place. A zip is read from its central directory and each file is decompressed only when requested. A WACZ is extracted to a temporary folder (WARC records become HTML + `figures/`)
2. Finds the entry page (`OUVRIR.html`, `ouvrir.html`, `index.html`, or `index.htm`)
3. Serves the files over HTTP on `127.0.0.1`
4. Opens a dedicated window on that local URL
5. Stops the server when the window closes, and deletes the temp files of an extracted WACZ. A folder or a zip you opened is left untouched

Several packs can be open at once. While you write category pages and indexes, open the folder: each save shows up on reload, with no zip to rebuild. Shipping a course is still a new zip, not a new installer.

A folder that already has an entry page is served as itself, even if a `.zip` or `.wacz` sits next to it. A folder with no entry page is treated as a Browsertrix collection: Taurus opens the `.wacz` or `.warc` inside. A single wrapper directory (one subfolder, no files) is unwrapped, same as a zip.

### Pack format

A pack is a folder of a static website, a zip of that website, a [WACZ](https://specs.webrecorder.net/wacz/latest/) (ISO WARC inside a zip), or a folder of Markdown notes. CloudFront figure URLs in Manning liveBooks are rewritten to local `figures/` files so the book works offline. A Markdown folder (no entry page) opens as a reader: search by file name, each note rendered to HTML. Links written as `/page/Slug` open the local `Slug_grokipedia.md`, or `Slug_wikipedia.md` if that is the only copy. Markdown files sitting inside an HTML site are still served as text.

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
| Path traversal | Requests outside the served root are rejected |
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

Pass a site folder, a zip, or a WACZ as an argument to open it on launch:

```bash
npm run tauri dev -- -- /path/to/site
npm run tauri dev -- -- /path/to/pack.zip
npm run tauri dev -- -- ~/kb/html/manning/_archives/a-simple-guide-to-retrieval-augmented-generation/a-simple-guide-to-retrieval-augmented-generation.wacz
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

- A folder of Markdown notes is rendered (search by file name). Markdown next to an `index.html` or `OUVRIR.html` is still served as text.
- Opening a folder reads it live. Closing the window does not delete that folder.
- One native binary per OS; there is no portable “just a zip of the player”.
- Linux is supported at the Tauri level but not built in CI yet.

### License

[MIT](LICENSE) — © 2026 Baptiste Boussemart.

---

## Français

### À quoi ça sert

Glissez un dossier, un zip ou un `.wacz`, choisissez-le dans le dialogue, passez un chemin en ligne de commande, double-cliquez une archive, ou déposez un dossier ou une archive sur l’icône de l’app (app packagée, associations de fichiers macOS). Taurus :

1. sert un dossier sur place. Un zip est lu par son catalogue central, et chaque fichier n’est décompressé qu’à la demande. Un WACZ est extrait dans un dossier temporaire (enregistrements WARC → HTML + `figures/`) ;
2. trouve la page d’entrée (`OUVRIR.html`, `ouvrir.html`, `index.html` ou `index.htm`) ;
3. sert les fichiers en HTTP sur `127.0.0.1` ;
4. ouvre une fenêtre dédiée sur cette URL locale ;
5. arrête le serveur à la fermeture de la fenêtre, et supprime les fichiers temporaires d’un WACZ extrait. Un dossier ou un zip ouvert n’est pas touché.

Plusieurs packs peuvent être ouverts en même temps. Pour écrire les pages de catégories et l’index, ouvrez le dossier : chaque enregistrement apparaît au rechargement, sans reconstruire de zip. Publier un cours reste un nouveau zip — pas un nouvel installeur.

Un dossier qui a déjà une page d’entrée est servi tel quel, même s’il contient aussi un `.zip` ou un `.wacz`. Sans page d’entrée, il est traité comme une collection Browsertrix : Taurus ouvre le `.wacz` ou le `.warc` qu’il contient. Un dossier enveloppe unique (un seul sous-dossier, aucun fichier) est déroulé, comme pour un zip.

### Format d’un pack

Un pack est un dossier de site statique, un zip de ce site, une archive [WACZ](https://specs.webrecorder.net/wacz/latest/) (WARC ISO dans un zip), ou un dossier de notes Markdown. Les URL CloudFront des figures Manning sont réécrits vers `figures/` pour la lecture hors-ligne. Un dossier Markdown (sans page d’entrée) s’ouvre comme un lecteur : recherche sur le nom du fichier, chaque note rendue en HTML. Les liens `/page/Slug` ouvrent le fichier local `Slug_grokipedia.md`, ou `Slug_wikipedia.md` s’il n’y a que celui-là. Les fichiers Markdown à l’intérieur d’un site HTML restent servis comme du texte.

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
| Traversée de chemin | Toute requête hors de la racine servie est refusée |
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

Passez un dossier de site, un zip ou un WACZ en argument pour l’ouvrir au lancement :

```bash
npm run tauri dev -- -- /chemin/vers/site
npm run tauri dev -- -- /chemin/vers/pack.zip
npm run tauri dev -- -- ~/kb/html/manning/_archives/a-simple-guide-to-retrieval-augmented-generation/a-simple-guide-to-retrieval-augmented-generation.wacz
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

- Un dossier de notes Markdown est rendu (recherche sur le nom du fichier). Un Markdown à côté d’un `index.html` ou `OUVRIR.html` reste servi comme du texte.
- Ouvrir un dossier le lit en direct. Fermer la fenêtre ne supprime pas ce dossier.
- Un binaire natif par OS ; il n’existe pas de lecteur « juste un zip ».
- Linux est géré au niveau Tauri, mais pas encore construit en CI.

### Licence

[MIT](LICENSE) — © 2026 Baptiste Boussemart.
