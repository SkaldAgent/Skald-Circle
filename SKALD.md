# Skald Circle — SKALD

## Stato attuale

Progetto nuova applicazione con agenti e chatbot per aiutare famiglie e piccoli gruppi a collaborare, con chat supervisionato per bambini/persone vulnerabili.

### Icone agenti — completate ✅

Tutti gli 11 agenti hanno ora icone in stile **Vector Paintings** (painterly vector, caldo e family-friendly), generate via ComfyUI:

| Agente | Animale | Stato |
|--------|---------|-------|
| Main Assistant | 🦊 Volpe | ✅ |
| Project Coordinator | 🦡 Tasso | ✅ |
| Researcher | 🐿️ Scoiattolo | ✅ |
| Generalist | 🦫 Castoro | ✅ |
| Code Explorer | 🕵️ Meerkat | ✅ |
| Software Architect | 🏗️ Airone | ✅ |
| Software Engineer | 🔧 Orso | ✅ |
| Spec Writer | 📝 Gufo | ✅ |
| Tech Lead | 👑 Cervo | ✅ |
| TIC | 👁️ Gatto | ✅ |
| Business Analyst | 💼 Gazza | ✅ |

### Refactoring — completato ✅

- Rimossa dipendenza da Tauri/desktop (`tauri.conf.json`, `src/desktop/`, `icons/`, `docs/desktop.md`, schemi gen/)
- Rimosso `build.rs` (non più necessario)
- Nuovo sistema i18n (core-api + plugin-mobile-connector + web)
- Refactoring sistema di configurazione

### Auto-build CI/CD ✅

Build automatica su NiPoGi con Gitea Actions (runner nativo v2.1.0):

| Componente | File | Stato |
|---|---|---|
| `scripts/package.sh` | Crea tarball distributivi da binari compilati | ✅ |
| `scripts/verify-version.sh` | Verifica che una release non sia già buildata | ✅ |
| `.gitea/workflows/nightly.yml` | Push su `main` → build amd64+arm64 → nightly/ | ✅ |
| `.gitea/workflows/release.yml` | PR check `verify-version` + merge → build → releases/v{ver}/ | ✅ |
| **act_runner** nativo su NiPoGi | v2.1.0, host-mode systemd service | ✅ |
| **Cross toolchain** (arm64) | `gcc-aarch64-linux-gnu` + `rustup target add` | ✅ |
| **Caddy `builds.skaldagent.net`** | Configurato + directory `/var/www/builds.skaldagent.net/` | ✅ |
| **Route53 `builds.skaldagent.net`** | A record → 145.40.169.107 | ✅ |
| **`install.sh`** | Script one-liner `curl ... | bash` | ⏳ Da creare |

### Prossimi passi

- Creare branch `release` su Gitea con branch protection (PR via UI)
- Testare il workflow con una PR su `release`
- Creare `install.sh` per installazione one-liner

### Future ideas (TODO)

- **One-liner install**: sito web con comando bash da copiare-incollare su macOS/Linux che fa installazione automatica
