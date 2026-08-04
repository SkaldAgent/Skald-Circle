# Voice input (speak instead of typing)

Every chat surface — the desktop chat and the mobile one — can show a **microphone button** next to the composer. Pressing it records, pressing it again stops, and the transcribed text lands in the message box for the user to edit before sending. On the desktop chat, holding **Space** records for as long as it is held.

The button only appears when the instance has a **transcription model** configured. If a user says the microphone is missing, that is the first thing to check — it is not a permission problem.

## Configuring transcription (admin)

Sidebar → **Models** → **Transcription**. Add a model, pick the provider, then pick or type the model id. Two kinds of provider work:

- **A cloud provider** (OpenAI, OpenRouter, ElevenLabs…). Audio is sent to that provider for transcription, so the spoken words leave the machine — worth saying out loud to anyone who asks, and the reason the local option exists.
- **A local plugin** — [plugins/whisper_local.md](plugins/whisper_local.md) transcribes on the machine itself, nothing leaves it. A plugin-provided transcriber takes precedence over the configured cloud models.

Several models can coexist; the lowest priority number is tried first. The optional **language** hint (e.g. `it`) improves accuracy when the speaker's language is known in advance; leaving it empty lets the model detect it.

Some providers can list their transcription models for you, so the id can be picked from a dropdown; others cannot, and the form then asks for the model id to be typed by hand. Both paths end up in the same place — a provider that cannot list is not a provider that cannot transcribe.

## Why the microphone button can do nothing when pressed

This one confuses people, and the cause is the **browser**, not Skald.

Browsers only give a page access to the microphone over a **secure connection**: `https://`, or `http://localhost` (and `127.0.0.1`). Skald typically runs on a machine on the home network and is opened at an address like `http://192.168.1.50:9000` — plain `http` on a non-local address — and there the browser hides the microphone entirely. The chat says so with an error message when the button is pressed.

What to suggest, in order of how well it holds up:

1. **Open Skald at `http://localhost:9000` instead of the IP address.** Works instantly, but only on the machine Skald itself runs on.
2. **Put Skald behind HTTPS.** Configured once, on the server, and then every device works — phones included. A Tailscale mesh ([plugins/remote_connectivity.md](plugins/remote_connectivity.md)) gives a hostname and a real certificate; a reverse proxy such as Caddy is the other common route. This is the answer for anyone using Skald from more than one device.
3. **Allow the insecure address in the browser's settings.** Chrome, Edge and Brave have a flag (`chrome://flags/#unsafely-treat-insecure-origin-as-secure`) that accepts the exact origin, e.g. `http://192.168.1.50:9000`, and needs a relaunch. Firefox has `dom.securecontext.allowlist` in `about:config`, taking the bare hostname. It has to be redone on every device and every browser profile, and these flags are explicitly temporary.

**Safari has no such setting** — neither on Mac nor on iPhone/iPad. For Safari users, HTTPS is the only route. Since iPhones and iPads cannot use anything but Safari's engine, that also means voice input on a phone needs HTTPS regardless of which browser is installed.

The other messages the microphone button can produce are ordinary: access **denied** means the browser was told no for this site and the permission has to be re-allowed in its settings; **not supported** means the browser is too old or is running with media features stripped out.
