# agent-sidekick

agent-sidekick ist ein kleiner lokaler MCP-Server für Claude Code und die OpenAI-Codex-CLI.
Deine Sitzung kann damit Aufgaben an einen zweiten Agenten abgeben: an KI:connect.nrw (Hochschul-Kontingent, kostenlos, über den Agenten OpenCode), an ein zweites Claude-Konto, an Gemini oder an OpenAI Codex (mit deinem ChatGPT-Abo oder einem API-Schlüssel).
Die Hauptsitzung wählt Backend und Modell, der zweite Agent arbeitet im angegebenen Projektordner und liefert sein Ergebnis zurück.
Es ist ein einzelnes Programm für Windows 11 und Linux. Aktuelle Version und Downloads: <https://github.com/AAPPHH/agent-sidekick/releases/latest>.

## Voraussetzungen

- Mindestens eine Agenten-CLI, in der du agent-sidekick nutzt: die Claude-Code-CLI **oder** die Codex-CLI (oder beide). Die VS-Code-Erweiterungen allein reichen nicht: Das Setup braucht das Programm `claude` bzw. `codex`.
  - Claude-Code-CLI (`claude`), angemeldet mit deinem Hauptkonto. Installiere sie mit dem offiziellen Installer:
    - Windows (PowerShell): `irm https://claude.ai/install.ps1 | iex`
    - Linux: `curl -fsSL https://claude.ai/install.sh | bash`

    Weitere Wege stehen unter <https://code.claude.com/docs/en/setup>.
  - Codex-CLI (`codex`), angemeldet mit deinem ChatGPT- oder OpenAI-Konto: `npm install -g @openai/codex` (braucht Node.js), dann `codex login`. Ohne Claude Code siehe [Nur mit Codex (ohne Claude Code)](#nur-mit-codex-ohne-claude-code).

  Öffne nach der Installation ein neues Terminal. Nutzt du das Terminal in VS Code, starte VS Code komplett neu.
- Node.js, nur für KI:connect (OpenCode), Gemini und zum Installieren von Codex: Download unter <https://nodejs.org/en/download>, unter Windows auch mit `winget install OpenJS.NodeJS.LTS`.
- Für KI:connect: ein Konto an einer teilnehmenden Hochschule (Anmeldung über das Single Sign-On deiner Hochschule).
- Für das Profil „Claude-Zweitkonto“: ein zweites Claude-Abonnement.
- Für das Profil „OpenAI Codex“: die Codex-CLI (`npm install -g @openai/codex`, das Setup bietet es an) und ein ChatGPT-Abo (Plus, Pro, Business, Edu oder Enterprise) oder ein OpenAI-API-Schlüssel.

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
- Der Setup-Assistent prüft Claude Code, Node.js, OpenCode und Codex. Du wählst ein oder mehrere Profile: „KI:connect.nrw (Hochschul-Kontingent, kostenlos)“ (Vorschlag), „Claude-Zweitkonto“, „Gemini“, „OpenAI Codex (ChatGPT-Abo oder API-Key)“. Ein fehlendes OpenCode, Gemini CLI oder Codex installiert er nach Rückfrage mit `npm install -g`. Wählst du mehrere neue Profile, fragt er nach dem Standard-Backend (Vorschlag: `kiconnect`). Dann fragt er die Schlüssel verdeckt ab (den KI:connect- oder OpenAI-Schlüssel kann er gleich online prüfen), schreibt erst danach die Konfiguration, zeigt beim ChatGPT-Abo den Anmeldebefehl für Codex, meldet den Server bei Claude Code an (und, wenn Codex installiert ist, auf Wunsch auch bei Codex; ohne Claude Code nur bei Codex, ohne Rückfrage) und führt `agent-sidekick doctor` aus.
- Eine vorhandene `config.toml` wird nie stillschweigend überschrieben: Du wählst Zusammenführen, Sichern und neu schreiben oder Abbrechen.
- Eine bestimmte Version installierst du in PowerShell mit `$env:VERSION = 'v0.3.0'` vor dem ersten Befehl, unter Linux mit `curl -fsSL https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.sh | VERSION=v0.3.0 sh`.

Danach VS Code neu laden (siehe [Erster Test](#erster-test)).

### Auf einem weiteren Rechner einrichten

Auf jedem Rechner gilt dasselbe: die [Voraussetzungen](#voraussetzungen) installieren, dann die zwei Befehle von oben. Dabei gilt:

- Schlüssel und Tokens werden nicht übertragen. Sie liegen im Schlüsselbund des jeweiligen Rechners. Das Setup fragt sie neu ab. Dein KI:connect-Schlüssel und das Token des Zweitkontos funktionieren auf mehreren Rechnern; du kannst sie wiederverwenden oder neue erzeugen.
- Die Anmeldung bei Claude Code und bei Codex erledigt das Setup auf jedem Rechner selbst.
- Das Codex-Backend braucht auf jedem Rechner einmal den eigenen `codex login` (siehe [OpenAI Codex nutzen](#openai-codex-nutzen)).
- Deine `config.toml` kannst du vom ersten Rechner kopieren, wenn du dieselben Backends willst. Unter Windows liegt sie in `%APPDATA%\agent-sidekick\config.toml`, unter Linux in `~/.config/agent-sidekick/config.toml`. Pfade darin, etwa zu Programmen, musst du gegebenenfalls anpassen. Danach `agent-sidekick setup --backend <name>` für jeden Schlüssel und `agent-sidekick doctor` zur Kontrolle.

### Nur mit Codex (ohne Claude Code)

Du hast die Codex-CLI, aber kein Claude Code? Dann meldet das Setup agent-sidekick nur in Codex an. So gehst du vor:

1. Node.js installieren, unter Windows z. B. mit `winget install OpenJS.NodeJS.LTS` (sonst <https://nodejs.org/en/download>). Danach ein neues Terminal öffnen.
2. Die Codex-CLI installieren, falls sie fehlt:

   ```sh
   npm install -g @openai/codex
   ```

3. Codex einmal mit deinem eigenen Konto anmelden (ChatGPT-Abo oder OpenAI-API-Schlüssel):

   ```sh
   codex login
   ```

4. agent-sidekick installieren, wie oben unter [Windows 11](#windows-11) oder [Linux](#linux-x86_64-aarch64) beschrieben, z. B. in PowerShell:

   ```powershell
   irm https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.ps1 | iex
   ```

5. Der Setup-Assistent startet von selbst. Er meldet „Claude Code nicht gefunden – agent-sidekick wird nur in Codex angemeldet“ und bietet das Profil „Claude-Zweitkonto“ nicht an (es braucht Claude Code). Wähle z. B. „KI:connect.nrw“ und/oder „OpenAI Codex“. Am Ende trägt er agent-sidekick in deine Codex-Konfiguration ein (`~/.codex/config.toml`, mit Sicherung).
6. Eine neue Codex-Sitzung starten (`codex`); `/mcp` zeigt `agent-sidekick`.

Nutzt du das Profil „OpenAI Codex“ als zweiten Agenten mit einem ChatGPT-Konto, trag ein Modell ein, das dein Konto nutzen darf (siehe den Modell-Hinweis unter [Codex als zweiter Agent](#codex-als-zweiter-agent-mit-deinem-chatgpt-abo)). Ohne Setup-Assistent (kein interaktives Terminal) meldet `agent-sidekick setup` den Server ebenfalls in Codex an, wenn Claude Code fehlt und Codex installiert ist. Installierst du Claude Code später, führe `agent-sidekick setup --register-only` aus. `agent-sidekick doctor` wertet ein fehlendes Claude Code dann nur als Hinweis, solange kein Backend Claude braucht.

## OpenAI Codex nutzen

### Codex als zweiter Agent (mit deinem ChatGPT-Abo)

Wähle im Setup das Profil „OpenAI Codex (ChatGPT-Abo oder API-Key)“ und dann „ChatGPT-Abo“. Das Setup legt für das Backend `codex` einen eigenen Codex-Ordner an und zeigt dir den Anmeldebefehl. Dein eigenes `~/.codex` mit deiner normalen Codex-Anmeldung bleibt dabei unberührt.

Später oder erneut geht es mit:

```sh
agent-sidekick setup --backend codex
```

Der Befehl zeigt dir, was du in einem separaten Terminal ausführen sollst, unter Windows (PowerShell) zum Beispiel:

```powershell
$env:CODEX_HOME = "$env:APPDATA\agent-sidekick\codex\codex"; codex login; Remove-Item Env:CODEX_HOME
```

Unter Linux:

```sh
CODEX_HOME="$HOME/.config/agent-sidekick/codex/codex" codex login
```

Melde dich im Browser mit dem ChatGPT-Konto an, dessen Abo genutzt werden soll. Ohne Browser (z. B. über SSH) nimmst du `codex login --device-auth`. Prüfe danach mit `agent-sidekick backends test codex`. Die Läufe zählen auf das Codex-Kontingent deines ChatGPT-Abos.

Mit einem OpenAI-API-Schlüssel wählst du im Setup „OpenAI-API-Schlüssel“ und fügst den Schlüssel von <https://platform.openai.com/api-keys> ein. Abgerechnet wird dann pro Token über dein OpenAI-Konto.

Ohne weitere Angabe nimmt Codex sein Standardmodell. Mit ChatGPT-Anmeldung lehnt OpenAI manche Modelle ab („model is not supported when using Codex with a ChatGPT account“). Welche Modelle dein Konto nutzen darf, zeigt `codex debug models` (im Terminal mit gesetztem `CODEX_HOME` des Backends, nach der Anmeldung). Trag sie dann in der `config.toml` beim Backend `codex` als `models = ["..."]` ein; das erste ist der Standard. Getestet (Oktober 2026) mit einem ChatGPT-Konto: `models = ["gpt-6-luna", "gpt-5.6-terra", "gpt-5.6-luna"]`. Codex arbeitet im Projektordner in seiner Sandbox: Es darf dort Dateien ändern, seine Befehle haben aber kein Netzwerk. Deine Schlüssel stehen nicht in den Umgebungsvariablen dieser Befehle. Lesen darf die Sandbox aber die ganze Festplatte: Codex kann also Dateien mit Zugangsdaten öffnen, etwa die ChatGPT-Anmeldung `auth.json` im Codex-Ordner des Backends oder, wenn kein Schlüsselbund verfügbar ist, die Schlüsseldateien von agent-sidekick (`backend-<name>.key`). Lass Codex deshalb nur in Projekten arbeiten, denen du vertraust.

### agent-sidekick in Codex verwenden

Ist die Codex-CLI installiert, fragt das Setup „Auch in Codex anmelden?“ (ohne Claude Code meldet es ohne Rückfrage an). Du kannst es auch jederzeit selbst ausführen:

```sh
agent-sidekick setup --codex
```

Das trägt agent-sidekick in deine Codex-Konfiguration ein (`~/.codex/config.toml`, vorher wird eine Sicherung angelegt). Starte danach eine neue Codex-Sitzung; `/mcp` zeigt `agent-sidekick`. Bitte Codex dann zum Beispiel: „Nutze sidekick_backends und lass den Claude-Agenten die Tests reparieren.“ Vor jedem `sidekick_run` fragt Codex nach deiner Zustimmung, denn der zweite Agent arbeitet außerhalb der Codex-Sandbox.

## KI:connect-API-Schlüssel erzeugen

1. Öffne <https://chat.kiconnect.nrw/app> und melde dich mit deinem Hochschulkonto an.
2. Klicke unten links auf deinen Namen und wähle **API Key Management**.

3. Klicke auf **Create Key** (ein Name für den Schlüssel ist optional) und kopiere den Schlüssel.

4. Füge den Schlüssel ein, wenn der Setup-Assistent danach fragt, oder speichere ihn später mit `agent-sidekick setup --backend kiconnect`.

**Gültigkeit:** Ein Schlüssel gilt einen Monat. Jede Anmeldung im KI:connect-Webinterface verlängert ihn um einen weiteren Monat. Ist er abgelaufen, erzeugst du dort mit **Rotate** einen neuen und speicherst ihn mit `agent-sidekick setup --backend kiconnect`.

Die Nutzung über die API zählt auf dasselbe Kontingent wie das Webinterface, und die Zahl paralleler Anfragen pro Person ist begrenzt.

## Groq-API-Schlüssel erzeugen

Groq ist ein zweites kostenloses Backend mit den offenen Modellen gpt-oss-120b und gpt-oss-20b. Die kostenlose Stufe ist so eng begrenzt, dass der Agent nur etwa einen Schritt pro Minute schafft; KI:connect ist das bessere kostenlose Backend.

1. Öffne https://console.groq.com/keys und melde dich an (kostenlos, ohne Kreditkarte; Anmeldung z. B. mit Google oder GitHub).
2. Klicke auf "Create API Key", gib einen Namen ein und kopiere den Schlüssel; er wird nur einmal angezeigt.
3. Wähle im Setup das Profil "Groq (kostenlos, mit Limits)" oder speichere den Schlüssel später mit `agent-sidekick setup --backend groq`.

**Grenzen:** Pro Modell gelten in der kostenlosen Stufe 30 Anfragen pro Minute, 1000 pro Tag, 8000 Tokens pro Minute und 200.000 Tokens pro Tag (Stand Oktober 2026, aktuelle Werte unter https://console.groq.com/settings/limits). Ein OpenCode-Schritt schickt etwa 7000 Tokens, daher stößt fast jeder Schritt erst an das Limit (HTTP 429) und OpenCode wartet. Gelegentlich bricht ein Lauf ab, wenn das Modell einen ungültigen Tool-Aufruf erzeugt (Groq-Fehler `tool_use_failed`). Nutze Groq nur für sehr kleine Aufgaben.

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

Ist die Codex-CLI installiert, macht das Setup ohne Claude Code weiter und meldet agent-sidekick nur in Codex an (siehe [Nur mit Codex (ohne Claude Code)](#nur-mit-codex-ohne-claude-code)). Fehlen beide, bricht es ab, bevor es etwas schreibt: Installiere Claude Code oder Codex wie unter [Voraussetzungen](#voraussetzungen) beschrieben, öffne ein neues Terminal (im VS-Code-Terminal: VS Code komplett neu starten) und starte `agent-sidekick setup` erneut.

### OpenCode oder Node.js fehlt

- **OpenCode fehlt:** Das Setup bietet `npm install -g opencode-ai` an. Du kannst den Befehl auch selbst ausführen und danach `agent-sidekick setup` erneut starten.
- **Node.js fehlt:** Das Setup bricht ab, bevor es etwas schreibt. Installiere Node.js (siehe [Voraussetzungen](#voraussetzungen)), öffne ein **neues** Terminal und starte `agent-sidekick setup` erneut. Nutzt du das Terminal in VS Code, starte VS Code komplett neu. Ein neues Terminal in VS Code reicht nicht, es kennt Node.js sonst noch nicht.

### Codex: „No ChatGPT login“ oder „not logged in“

Das Codex-Backend hat noch keine Anmeldung in seinem eigenen Codex-Ordner. Führe `agent-sidekick setup --backend codex` aus und den Anmeldebefehl, den es anzeigt. Deine normale Codex-Anmeldung in `~/.codex` zählt dafür nicht.

### Codex bricht `sidekick_run` nach 60 Sekunden ab

Der Eintrag in der Codex-Konfiguration hat kein ausreichendes `tool_timeout_sec`, etwa weil er mit `codex mcp add` angelegt wurde. Führe `agent-sidekick setup --codex` aus; `agent-sidekick doctor` prüft den Eintrag.

### Groq oder KI:connect meldet 429 (Rate-Limit)

Das Kontingent pro Minute oder pro Tag ist erschöpft. Warte die angegebene Zeit ab oder gib die Aufgabe an ein anderes Backend ab, zum Beispiel mit `fallback`. Bei Groq hilft es, die Aufgabe kleiner zu schneiden. Die aktuellen Limits stehen bei Groq unter https://console.groq.com/settings/limits.

### /mcp zeigt agent-sidekick nicht an

Lade VS Code neu oder starte es komplett neu. Hilft das nicht, führe `agent-sidekick doctor` aus; es nennt den nächsten Schritt. `agent-sidekick setup --register-only` meldet den Server erneut bei Claude Code an, und `claude mcp list` zeigt, ob er eingetragen ist.

### Änderungen an der config.toml wirken nicht

Seit Version 0.5.0 liest agent-sidekick die `config.toml` beim nächsten Aufruf eines Werkzeugs (`sidekick_run`, `sidekick_backends`, `sidekick_status`) neu ein; ein Neustart ist nicht nötig. Ist die geänderte Datei fehlerhaft, arbeitet agent-sidekick mit der letzten gültigen Fassung weiter und meldet den Fehler als `config_error` in `sidekick_backends`; `agent-sidekick doctor` zeigt Details. Ältere Versionen lesen die Datei nur beim Start: Wähle in Claude Code `/mcp`, dann agent-sidekick und **Reconnect**.

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
- **Deinstallieren:** `agent-sidekick uninstall` entfernt die gespeicherten Schlüssel, die ChatGPT-Anmeldung des Codex-Backends und die Anmeldung bei Claude Code und bei Codex. `agent-sidekick uninstall --purge` löscht zusätzlich den Konfigurationsordner. Danach das Programm löschen: unter Windows den Ordner `%LOCALAPPDATA%\Programs\agent-sidekick`, unter Linux die Datei `~/.local/bin/agent-sidekick`.

## Sicherheit beim Gemini-Profil

Das Gemini-Profil startet die Gemini CLI mit `--skip-trust`. Ohne diese Option bricht sie in Ordnern ab, denen du nicht ausdrücklich vertraut hast. Mit `--skip-trust` vertraut sie aber jedem Projektordner, in dem sie arbeitet:

- Sie liest dort und in den Ordnern darüber eine `.env` oder `.gemini/.env`. Ein Projekt kann so zum Beispiel `GOOGLE_GEMINI_BASE_URL` auf einen fremden Server setzen. Dein Gemini-Schlüssel geht dann an diesen Server.
- Sie lädt `.gemini/settings.json` mit MCP-Servern und Hooks. Das ist fremder Code, der deinen Schlüssel sehen kann.

Nutze Gemini deshalb nur in Projektordnern, denen du vertraust, zum Beispiel nicht in frisch geklonten fremden Repositories. Nimm einen Schlüssel, den du jederzeit widerrufen kannst. Findet agent-sidekick solche Dateien, schreibt es eine Warnung ins Run-Log.

## Nutzungsbedingungen

- Jede Person nutzt ihren eigenen KI:connect-Schlüssel und ihre eigenen Claude-Konten.
- Gib Schlüssel und Tokens nie weiter und schreib sie nie in Prompts, Dateien oder Repositories.
- Beachte die Nutzungsbedingungen deiner Hochschule und von KI:connect.nrw, zum Beispiel welche Daten verarbeitet werden dürfen.
- Das zweite Claude-Konto ist nur für deine eigenen Zwecke. Für jedes Konto gelten die Bedingungen von Anthropic, für Gemini die von Google, für Codex und ChatGPT die von OpenAI.
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
