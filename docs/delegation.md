# Delegieren nach Vertrauen

Für Menschen und für das Steuermodell. Grundlage ist der Steckbrief des Modells (`sidekick_steckbrief`), nie die Rohtabellen.

## Regeln

- Die Aufgabe richtet sich nach dem Vertrauen, nie umgekehrt. Erst den Steckbrief für Dimension und Scope lesen, dann das Modell wählen oder die Aufgabe kleiner schneiden.
- Aufgaben, die mehrere Dimensionen mischen (suchen, Tests schreiben, Code schreiben, entscheiden), werden getrennt und einzeln vergeben.
- Offene Entscheidungen trifft das Steuermodell vorher und schreibt sie in den Task. Ein Modell mit Entscheidungsrecht (decisions ab 0,85) darf selbst entscheiden, sonst nicht.
- Kostet das Review mehr als Selbermachen, gehört das als `conclusion` ins Log (`sidekick_record_experience`). Beim nächsten Vergleich zieht das den Wert herunter.
- Ein Block wiegt schwerer als ein abgelehnter Diff: abgelehnt heißt falsch, Block heißt Ausbruchsversuch (geschützter Pfad oder Begriff, Limits).

## Mechanik

- Prüfanteil = 1 − Vertrauen. Schreibrecht gibt es ab write-code ≥ 0,50 (write-tests ≥ 0,50 für Testaufgaben); darunter bekommt das Harness keine Schreibwerkzeuge.
- Geschützt für alles unter 1,00: `tests/**`, `spec/**`, `**/validate*` sowie das Entfernen von `decided_by` und `assert`.
- Ergebnisse gehen als Patch oder Branch zurück und werden nie angewendet. Ausnahme ist Gold (Opus 5.5).
- Erreicht ein Lauf das Schrittlimit, ist das ein normales Ergebnis (`aborted`), kein Fehler.
- Modelle ohne technische Sperre (`nur-diff`) werden bei Schreibrecht auf 0,60 gedeckelt; ohne Schreibrecht verwirft der Sidekick jeden Diff.

## Ablauf

1. Steckbrief holen (`sidekick_steckbrief`), Scope passend zur Aufgabe wählen (`global`, `repo:<name>`, `module:<repo>/<pfad>`).
2. Aufgabe auf die erlaubten Dimensionen zuschneiden, Entscheidungen vorab festlegen.
3. `sidekick_dispatch` starten, Ergebnis prüfen, dann mit `sidekick_record_experience` bewerten (`accepted`, `rejected`, `note`; Blocks und Aborts trägt der Sidekick selbst ein).
4. Mit `sidekick_sync` Erfahrungen teilen. Neue Modelle ohne Registry-Zeile werden abgelehnt.

Die Daten liegen öffentlich unter `data/` im Code-Repo; Texte privater Einträge und Rohkennungen verlassen den Rechner nie.
