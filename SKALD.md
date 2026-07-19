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

### Auto-build CI/CD 🚧

Implementazione in corso per build automatica su NiPoGi con Gitea Actions:

| Componente | File | Stato |
|---|---|---|
| `scripts/package.sh` | Crea tarball distributivi da binari compilati | ✅ |
| `scripts/verify-version.sh` | Verifica che una release non sia già buildata | ✅ |
| `.gitea/workflows/nightly.yml` | Push su `main` → build amd64+arm64 → nightly/ | ✅ |
| `.gitea/workflows/release.yml` | PR check `verify-version` + merge → build → releases/v{ver}/ | ✅ |
| **act_runner** su NiPoGi | Esegue i workflow | 🔧 Da installare |
| **Cross toolchain** (arm64) | `gcc-aarch64-linux-gnu` per cross-compilazione | 🔧 Da installare |
| **Caddy `builds.skaldagent.net`** | Serve i tarball + `install.sh` | 🔧 Da configurare |
| **`install.sh`** | Script one-liner `curl ... | bash` | ⏳ Da creare |

### Prossimi passi

- Configurare NiPoGi: act_runner, toolchain, Caddy
- Testare i workflow con una PR su `release`
- Creare `install.sh`

### Future ideas (TODO)

- **One-liner install**: sito web con comando bash da copiare-incollare su macOS/Linux che fa installazione automatica
