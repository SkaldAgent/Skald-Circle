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

- Business Analyst aveva `meta.json` senza campo `icon` — aggiunto.
- `agents/README.md` riscritto con nuova guida stile Vector Paintings.
- Stile: `VectorPaintDaal` trigger, palette calde (terracotta, ambra, oro, corallo, teal), animali come personaggi.

### Prossimi passi

- Sviluppare l'app Skald Circle vera e propria

### Future ideas (TODO)

- **Auto-build on push**: webhook Gitea → systemd service su NiPoGi → `cargo build --release` → pacchetto pronto
- **One-liner install**: sito web con comando bash da copiare-incollare su macOS/Linux che fa installazione automatica
- **Package hosting**: servire builds via Caddy su `builds.skaldagent.net`
