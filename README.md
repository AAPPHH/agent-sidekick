# agent-sidekick

agent-sidekick ist ein kleiner lokaler MCP-Server für Claude Code.
Deine Claude-Code-Sitzung kann damit Aufgaben an einen zweiten Agenten abgeben: an KI:connect.nrw (Hochschul-Kontingent, kostenlos, über den Agenten OpenCode), an ein zweites Claude-Konto oder an Gemini.
Die Hauptsitzung wählt Backend und Modell, der zweite Agent arbeitet im angegebenen Projektordner und liefert sein Ergebnis zurück.
Es ist ein einzelnes Programm für Windows 11 und Linux.

## Installation (zwei Befehle)

### Windows 11

In PowerShell:

```powershell
irm https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.ps1 | iex
```

```powershell
agent-sidekick setup
```

Der erste Befehl installiert das Programm nach `%LOCALAPPDATA%\Programs\agent-sidekick` und trägt diesen Ordner in deinen Benutzer-PATH ein.

### Linux (x86_64, aarch64)

```sh
curl -fsSL https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.sh | sh
```

```sh
agent-sidekick setup
```

Der erste Befehl installiert das Programm nach `~/.local/bin`. Er bietet an, die PATH-Zeile in die rc-Datei deiner Shell einzutragen, und schreibt sie nie ohne Nachfrage.

### Was dabei passiert

- Die Installation prüft den Download gegen `SHA256SUMS` und braucht weder Rust noch Git noch Node.js.
- In einem interaktiven Terminal startet die Installation am Ende selbst `agent-sidekick setup`. Den zweiten Befehl brauchst du nur, wenn das Setup nicht lief oder du es abgebrochen hast.
- Der Setup-Assistent prüft Claude Code, Node.js und OpenCode. Du wählst ein oder mehrere Profile: „KI:connect.nrw (Hochschul-Kontingent, kostenlos)“ (Vorschlag), „Claude-Zweitkonto“, „Gemini“. Ein fehlendes OpenCode oder Gemini CLI installiert er nach Rückfrage mit `npm install -g`. Wählst du mehrere neue Profile, fragt er nach dem Standard-Backend (Vorschlag: `kiconnect`). Dann fragt er die Schlüssel verdeckt ab (den KI:connect-Schlüssel kann er gleich online prüfen), schreibt erst danach die Konfiguration, meldet den Server bei Claude Code an und führt `agent-sidekick doctor` aus.
- Eine vorhandene `config.toml` wird nie stillschweigend überschrieben: Du wählst Zusammenführen, Sichern und neu schreiben oder Abbrechen.
- Eine bestimmte Version installierst du in PowerShell mit `$env:VERSION = 'v0.3.0'` vor dem ersten Befehl, unter Linux mit `curl -fsSL https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.sh | VERSION=v0.3.0 sh`.

Danach VS Code neu laden (siehe [Erster Test](#erster-test)).

## Voraussetzungen

- Die Claude-Code-CLI (`claude`), angemeldet mit deinem Hauptkonto. Die VS-Code-Erweiterung allein reicht nicht: Das Setup und der Server brauchen das Programm `claude`. Installiere es mit dem offiziellen Installer:
  - Windows (PowerShell): `irm https://claude.ai/install.ps1 | iex`
  - Linux: `curl -fsSL https://claude.ai/install.sh | bash`

  Öffne danach ein neues Terminal. Nutzt du das Terminal in VS Code, starte VS Code komplett neu. Weitere Wege stehen unter <https://code.claude.com/docs/en/setup>.
- Node.js, nur für KI:connect (OpenCode) und Gemini: Download unter <https://nodejs.org/en/download>, unter Windows auch mit `winget install OpenJS.NodeJS.LTS`.
- Für KI:connect: ein Konto an einer teilnehmenden Hochschule (Anmeldung über das Single Sign-On deiner Hochschule).
- Für das Profil „Claude-Zweitkonto“: ein zweites Claude-Abonnement.

## KI:connect-API-Schlüssel erzeugen

1. Öffne <https://chat.kiconnect.nrw/app> und melde dich mit deinem Hochschulkonto an.
2. Klicke unten links auf deinen Namen und wähle **API Key Management**.

   > Screenshot folgt: Menü unter deinem Namen mit dem Eintrag „API Key Management“

3. Klicke auf **Create Key** (ein Name für den Schlüssel ist optional) und kopiere den Schlüssel.

   > Screenshot folgt: neu erzeugter Schlüssel in der API-Key-Verwaltung

4. Füge den Schlüssel ein, wenn der Setup-Assistent danach fragt, oder speichere ihn später mit `agent-sidekick setup --backend kiconnect`.

**Gültigkeit:** Ein Schlüssel gilt einen Monat. Jede Anmeldung im KI:connect-Webinterface verlängert ihn um einen weiteren Monat. Ist er abgelaufen, erzeugst du dort mit **Rotate** einen neuen und speicherst ihn mit `agent-sidekick setup --backend kiconnect`.

Die Nutzung über die API zählt auf dasselbe Kontingent wie das Webinterface, und die Zahl paralleler Anfragen pro Person ist begrenzt.

## Token für das Claude-Zweitkonto erzeugen

Öffne ein separates Terminal und führe aus:

Windows (PowerShell):

```powershell
$env:CLAUDE_CONFIG_DIR = "$HOME\.claude-sidekick"; claude setup-token
```

Linux:

```sh
CLAUDE_CONFIG_DIR=~/.claude-sidekick claude setup-token
```

Melde dich im Browser mit dem **zweiten** Claude-Konto an. Kopiere den ausgegebenen Token (er beginnt mit `sk-ant-oat01-`) und füge ihn ein, wenn das Setup danach fragt, oder speichere ihn später mit `agent-sidekick setup --backend sidekick`.

## Erster Test

1. VS Code neu laden: Befehlspalette (Strg+Shift+P) → **Developer: Reload Window**.
2. In Claude Code `/mcp` eingeben: `agent-sidekick` muss als verbunden angezeigt werden.
3. Einen Prompt ausprobieren, zum Beispiel:
   - „Nutze sidekick_backends und zeig mir, welche Backends erreichbar sind.“
   - „Lass kiconnect die README dieses Projekts zusammenfassen.“

Die Ergebnisse der kostenlosen KI:connect-Modelle sind Entwürfe: Prüf die Änderungen (Diff), bevor du sie übernimmst.

## FAQ

### Claude Code wurde nicht gefunden

Das Setup braucht die Claude-Code-CLI. Installiere sie wie unter [Voraussetzungen](#voraussetzungen) beschrieben, öffne ein neues Terminal (im VS-Code-Terminal: VS Code komplett neu starten) und starte `agent-sidekick setup` erneut.

### OpenCode oder Node.js fehlt

- **OpenCode fehlt:** Das Setup bietet `npm install -g opencode-ai` an. Du kannst den Befehl auch selbst ausführen und danach `agent-sidekick setup` erneut starten.
- **Node.js fehlt:** Das Setup bricht ab, bevor es etwas schreibt. Installiere Node.js (siehe [Voraussetzungen](#voraussetzungen)), öffne ein **neues** Terminal und starte `agent-sidekick setup` erneut. Nutzt du das Terminal in VS Code, starte VS Code komplett neu. Ein neues Terminal in VS Code reicht nicht, es kennt Node.js sonst noch nicht.

### /mcp zeigt agent-sidekick nicht an

Lade VS Code neu oder starte es komplett neu. Hilft das nicht, führe `agent-sidekick doctor` aus; es nennt den nächsten Schritt. `agent-sidekick setup --register-only` meldet den Server erneut bei Claude Code an, und `claude mcp list` zeigt, ob er eingetragen ist.

### Der KI:connect-Schlüssel ist abgelaufen

Erzeuge im KI:connect-Webinterface unter **API Key Management** mit **Rotate** einen neuen Schlüssel und speichere ihn mit `agent-sidekick setup --backend kiconnect` (siehe [KI:connect-API-Schlüssel erzeugen](#kiconnect-api-schlüssel-erzeugen)).

### `agent-sidekick` wird nach der Installation nicht gefunden (PATH)

- **Windows:** Die PATH-Änderung gilt erst in neuen Terminals. Schließe das Terminal und öffne ein neues. Nutzt du das Terminal in VS Code, starte VS Code komplett neu. Bis dahin geht es mit dem vollen Pfad:

  ```powershell
  & "$env:LOCALAPPDATA\Programs\agent-sidekick\agent-sidekick.exe" setup
  ```

- **Linux:** Nimm das Angebot der Installation an, oder trage `export PATH="$HOME/.local/bin:$PATH"` in `~/.bashrc` ein (zsh: `~/.zshrc`) und öffne ein neues Terminal.

Claude Code startet den Server über den absoluten Pfad, der beim Setup eingetragen wurde. Der Server funktioniert also auch dann, wenn PATH noch nicht aktualisiert ist.

## Aktualisieren und deinstallieren

- **Aktualisieren:** `agent-sidekick update` prüft, ob es eine neue Version gibt, und zeigt den passenden Installationsbefehl an. Führe ihn aus und lade VS Code neu. `agent-sidekick doctor` weist höchstens einmal am Tag auf ein Update hin.
- **Deinstallieren:** `agent-sidekick uninstall` entfernt die gespeicherten Schlüssel und die Anmeldung bei Claude Code. `agent-sidekick uninstall --purge` löscht zusätzlich den Konfigurationsordner. Danach das Programm löschen: unter Windows den Ordner `%LOCALAPPDATA%\Programs\agent-sidekick`, unter Linux die Datei `~/.local/bin/agent-sidekick`.

## Sicherheit beim Gemini-Profil

Das Gemini-Profil startet die Gemini CLI mit `--skip-trust`. Ohne diese Option bricht sie in Ordnern ab, denen du nicht ausdrücklich vertraut hast. Mit `--skip-trust` vertraut sie aber jedem Projektordner, in dem sie arbeitet:

- Sie liest dort und in den Ordnern darüber eine `.env` oder `.gemini/.env`. Ein Projekt kann so zum Beispiel `GOOGLE_GEMINI_BASE_URL` auf einen fremden Server setzen. Dein Gemini-Schlüssel geht dann an diesen Server.
- Sie lädt `.gemini/settings.json` mit MCP-Servern und Hooks. Das ist fremder Code, der deinen Schlüssel sehen kann.

Nutze Gemini deshalb nur in Projektordnern, denen du vertraust, zum Beispiel nicht in frisch geklonten fremden Repositories. Nimm einen Schlüssel, den du jederzeit widerrufen kannst. Findet agent-sidekick solche Dateien, schreibt es eine Warnung ins Run-Log.

## Nutzungsbedingungen

- Jede Person nutzt ihren eigenen KI:connect-Schlüssel und ihre eigenen Claude-Konten.
- Gib Schlüssel und Tokens nie weiter und schreib sie nie in Prompts, Dateien oder Repositories.
- Beachte die Nutzungsbedingungen deiner Hochschule und von KI:connect.nrw, zum Beispiel welche Daten verarbeitet werden dürfen.
- Das zweite Claude-Konto ist nur für deine eigenen Zwecke. Für jedes Konto gelten die Bedingungen von Anthropic, für Gemini die von Google.
- agent-sidekick wechselt nicht automatisch zwischen Konten und umgeht keine Nutzungslimits.
- Die Schlüssel liegen im Schlüsselbund des Betriebssystems (Windows-Anmeldeinformationsverwaltung, unter Linux Secret Service). Nur wenn kein Schlüsselbund verfügbar ist, liegen sie in einer geschützten Datei im Konfigurationsordner.

## Weiterführende Dokumentation

- [Technische Referenz](docs/REFERENCE.md): Backends, Konfiguration, Tools, Sicherheit (Englisch)
- [Entwicklung](docs/DEVELOPMENT.md)
- [Releases](docs/RELEASING.md)
- [Anmeldung als Claude-Code-Plugin](plugin/README.md) (Alternative zur Anmeldung durch das Setup)
- [Designentscheidungen und Testnotizen](NOTES.md)
- [Changelog](CHANGELOG.md)

## Lizenz

Nach deiner Wahl unter der [Apache License 2.0](LICENSE-APACHE) oder der [MIT-Lizenz](LICENSE-MIT).
