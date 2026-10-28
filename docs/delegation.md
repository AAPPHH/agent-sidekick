# Delegieren nach Vertrauen

Für Menschen und für das Steuermodell. Grundlage ist der Steckbrief des Modells (`sidekick_steckbrief`), nie die Rohtabellen.

## Regeln

- Die Aufgabe richtet sich nach dem Vertrauen, nie umgekehrt. Erst den Steckbrief für Dimension und Scope lesen, dann das Modell wählen oder die Aufgabe kleiner schneiden.
- Aufgaben, die mehrere Dimensionen mischen (suchen, Tests schreiben, Code schreiben, entscheiden), werden getrennt und einzeln vergeben: gemischte Aufgaben als Workflow einreichen (`sidekick_workflow`), eine Dimension pro Schritt, Abhängigkeiten über `depends_on`.
- Worker dürfen zurückfragen (`ask_orchestrator`): der Aufruf kehrt mit Status `question` zurück, antworte mit `sidekick_answer`. Rückfragen ändern das Vertrauen nicht; der Steckbrief zählt sie.
- Offene Entscheidungen trifft das Steuermodell vorher und schreibt sie in den Task. Ein Modell mit Entscheidungsrecht (decisions ab 0,85) darf selbst entscheiden, sonst nicht.
- Kostet das Review mehr als Selbermachen, gehört das als `conclusion` ins Log (`sidekick_record_experience`). Beim nächsten Vergleich zieht das den Wert herunter.
- Ein Block wiegt schwerer als ein abgelehnter Diff: abgelehnt heißt falsch, Block heißt Ausbruchsversuch (geschützter Pfad oder Begriff, Limits).

## Mechanik

- Konto-Sperre: Backends mit `account` in `config.toml` werden abgelehnt, wenn das Konto dem des Aufrufers gleicht ("gleiches Konto wie der Aufrufer"); kontogebundene Backends ohne `account` ebenso ("Konto unbekannt"). Das Konto steht in `sidekick_backends` und in der Modellzeile des Steckbriefs.
- Prüfanteil = 1 − Vertrauen. Schreibrecht gibt es ab write-code ≥ 0,50 (write-tests ≥ 0,50 für Testaufgaben); darunter bekommt das Harness keine Schreibwerkzeuge.
- Geschützt für alles unter 1,00: `tests/**`, `spec/**`, `**/validate*` sowie das Entfernen von `decided_by` und `assert`.
- Vorschlagsmodus: unter der Schreibschwelle bekommt das Modell keine Schreibwerkzeuge, liefert aber den Diff als Text. Der Sidekick prüft und wendet ihn nur im Job-Worktree an und gibt Branch und Patch zurück. So steigt Vertrauen: gute, akzeptierte Vorschläge heben write-code/write-tests über 0,50, erst dann schreibt das Modell selbst.
- Ergebnisse gehen als Patch oder Branch zurück und werden nie angewendet. Ausnahme ist Gold (Opus 5.5).
- Erreicht ein Lauf das Schrittlimit, ist das ein normales Ergebnis (`aborted`), kein Fehler.
- Modelle ohne technische Sperre (`nur-diff`) werden bei Schreibrecht auf 0,60 gedeckelt; ohne Schreibrecht verwirft der Sidekick jeden Diff.

## Ablauf

1. Den kurzen Steckbrief lesen (`sidekick_steckbrief`; Scope `global`, `repo:<name>` oder `module:<repo>/<pfad>`; Tabellen und Zähler nur mit `detail=true`) und die Aufgabe danach formulieren: "Gut für", "So formulieren", "Nicht geben", "Aus Fehlern".
2. Aufgabe auf die erlaubten Dimensionen zuschneiden, Entscheidungen vorab festlegen.
3. `sidekick_dispatch` (oder `sidekick_workflow` für gemischte Aufgaben) starten (oder `sidekick_run` mit `dimension`, wenn das Modell registriert ist), Ergebnis prüfen, dann mit `sidekick_record_experience` und der `job_id` bewerten (`accepted` oder `rejected`; bei `rejected` die Lehre als Regel für die nächste Formulierung in `conclusion` schreiben, sonst Fehler; Modell, Scope und Dauer kommen aus dem Job, die Review-Zeit misst der Sidekick). Blocks und Aborts trägt der Sidekick selbst ein, für Blocks gibt es kein Urteil.
4. Mit `sidekick_sync` Erfahrungen teilen. Neue Modelle ohne Registry-Zeile werden abgelehnt.

Die Daten liegen im privaten Repo `AAPPHH/sidekick-data` (`data_repo`); Texte privater Einträge und Rohkennungen verlassen den Rechner nie.
