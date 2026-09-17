# DropRate Launcher

The native app. Pair with your wallet, install the games you own, play them.

Built on Tauri 2 — a Rust core with the system webview for UI. The installer is
around 5 MB rather than the ~85 MB an Electron build would be, and the parts that
enforce ownership compile into the binary instead of shipping as readable
JavaScript.

---

## Why this is a separate repo

`DropRate-Final` is what Vercel builds and deploys. Dropping a Rust desktop app
into it means every push to the site drags a `src-tauri/` tree along, and the
launcher will eventually want its own release tags, its own CI, and code-signing
secrets that have no business sitting next to the website's. Keep them apart.

The launcher talks to the site over the public API only — the same
`POST /api/crate` endpoint everything else uses. There is no shared code to keep
in sync, just the action names in `src-tauri/src/api.rs`.

---

## Setting up (Windows)

Four things, once. The first two are the only real work.

**1. Rust** — download and run `rustup-init.exe` from <https://rustup.rs>. Accept
the defaults. Close and reopen your terminal afterwards so `cargo` is on PATH.

**2. Microsoft C++ Build Tools** — Tauri compiles native code and needs a linker.
Get "Build Tools for Visual Studio" from
<https://visualstudio.microsoft.com/visual-cpp-build-tools/>, run it, and tick
**Desktop development with C++**. This is a few GB and takes a while; it is the
slowest part of the whole setup and you only do it once.

**3. WebView2** — already present on Windows 11 and on any updated Windows 10.
If the app later refuses to open a window, install the Evergreen Runtime from
Microsoft and try again.

**4. Node** — you already have it.

Then:

```bash
npm install
npm run dev
```

The first `npm run dev` compiles the whole Rust dependency tree and will take
several minutes. Every run after that is seconds.

To produce an installer:

```bash
npm run build
```

Output lands in `src-tauri/target/release/bundle/`. On Windows that is an NSIS
`.exe` installer.

### macOS and Linux

Same commands. macOS needs Xcode command line tools (`xcode-select --install`);
Linux needs `webkit2gtk` and `libayatana-appindicator` from your package manager.
For a macOS build you will also need an `.icns`, which `npm run icons` generates
from `src-tauri/icons/icon.png`.

---

## How it fits together

```
src/                     the UI. Plain HTML/CSS/JS, no build step, no framework.
src-tauri/src/
  main.rs                Tauri commands — the only surface the UI can reach
  api.rs                 DropRate API client; the only place the device token is used
  gate.rs                the resale gate + archive path safety (dependency-free, tested)
  install.rs             streaming download, checksum, extract, launch
  state.rs               on-disk state, written atomically
```

### The security shape

This is the part worth reading before changing anything.

**The launcher never sees a wallet.** No private key, no seed phrase, no signing.
Pairing happens in your browser, where the wallet already lives and where you can
read what you are approving. The launcher receives a *device token* and nothing
else.

**That token is deliberately weak.** It can list a library and request download
links. It cannot spend, transfer, list for sale, or sign anything. Revoking it
from the website kills it immediately.

**The token lives in Rust.** It is never handed to the webview. A compromised
page in the UI — an injected script, a bad dependency — has nothing to steal.
Commands are shaped to keep it that way: `open_pair_page` takes no argument, so
it can only ever open the pairing URL the server just issued.

**Ownership is re-checked server-side, against the chain, on every download and
before every launch.** The launcher does not get to decide whether you own
something; it asks, and the answer comes from `walletOwnsCopy`.

### The resale rule

`gate.rs` holds the decision, and its asymmetry is the whole point:

| Server says | Result |
|---|---|
| owned | launches |
| **not owned** | **blocked, always** — no grace, no override |
| couldn't ask (offline) | launches for up to 72 hours since the last confirmed check |

A definite "no" blocks immediately no matter how recently the game was verified —
that is the resale case. The grace window exists only for genuine uncertainty, so
that a flaky connection doesn't take someone's game away for the weekend. Selling
a copy also zeroes its local grace on the next library refresh, so a seller can't
buy 72 more hours by pulling the network cable.

Nineteen tests cover this and archive path safety:

```bash
rustc --test --edition 2021 src-tauri/src/gate.rs -o /tmp/gate_test && /tmp/gate_test
```

`gate.rs` has no dependencies precisely so that command works on its own, with no
network and no build tooling.

---

## What v0.1 does and doesn't do

**Does:** pair, list your library with chain-verified ownership, install a build
for your platform with progress and sha256 verification, launch it, update when a
newer build exists, uninstall, sign out.

**Doesn't yet:**

- **Chunked delivery.** Installing downloads the whole build every time. The
  design for 4 MB content-addressed chunks and delta patching is written up
  separately; this is the next piece and it is what makes a 100 GB game
  updatable in minutes instead of hours.
- **Encrypted-at-rest game files.** Files on disk are plain. The resale gate
  stops the *launcher* running a sold game; it does not stop someone running the
  `.exe` directly from the folder. Closing that needs encrypted chunks decrypted
  at launch, which lands with the chunk work.
- **Developer uploads.** Pairing is read-only by design. Publishing from the app
  needs a second permission tier, named on the pairing screen, so a player's
  launcher never silently carries publishing rights.
- **Resume after an interrupted download.** Restarting begins again. The download
  link is valid for six hours and ownership is re-checked on every request, so
  asking for a fresh one is cheap — but partial-file resume properly belongs to
  the chunk layer rather than being built twice.

---

## Shipping a release (so people can download it)

The site's download page (`droprate.xyz/download.html`) points at this repo's
GitHub Releases and picks up the newest installer automatically. To publish one:

1. **First time only** — put this folder on GitHub. In GitHub Desktop: File →
   Add local repository → pick this folder → "create a repository" when it asks
   → Publish repository. Name it **`droprate-launcher`** under **MooseNuggets**
   (that exact name is what `download.html` looks for; change `REPO` there if
   you pick another). Public or private both work for the download link.

2. Bump the version in `src-tauri/tauri.conf.json` and `package.json` if you
   want a new number, commit, push.

3. Tag it. In GitHub Desktop: History → right-click the commit → Create Tag →
   `v0.1.0` → then Push origin (tags push with it). Or from a terminal:
   `git tag v0.1.0 && git push origin v0.1.0`.

4. Wait ~10–15 min. Actions tab shows the build; Releases tab gets
   `DropRate v0.1.0` with a Windows `.exe` attached. The download button on the
   site updates itself the next time someone loads the page.

The workflow is the `release.yml` sitting in this folder's root. **Before step 3,
move it to `.github/workflows/release.yml`** (create those two folders — GitHub
only runs workflows from that exact path). It builds Windows only for now;
macOS and Linux lines are in there commented out.

**SmartScreen:** unsigned builds get a "Windows protected your PC" warning the
first time. That's expected until there's a code-signing certificate
(~$200–400/yr, or free via Azure Trusted Signing for some accounts). The
download page tells people to click More info → Run anyway.
