# RFC 0001 — Nebenläufigkeit und Effekte in ZAL v0.1

Status: umgesetzt in v0.1.

**Entscheid 1: strukturierte Nebenläufigkeit mit Wert-Captures.** Jeder
nebenläufige Block wird zu einer eigenen Funktion; äussere Variablen werden als
Kopie übergeben und sind darin unveränderlich. Dadurch gibt es keine geteilten
veränderlichen Daten zwischen Fibern, ohne dass die Sprache Locks oder
Ownership-Regeln braucht. Kosten: Ergebnisse kommen nur über Rückgabewerte
zurück. Das passt zu Workflows (Ergebnisse fliessen entlang von Kanten).

**Entscheid 2: `return` und `?` gelten im Block.** Damit ist `retry(n) { a()?
b() }` ein Versuch über mehrere Schritte. Alternative (Rücksprung aus der
äusseren Funktion) hätte Abbruch und Aufräumen von Geschwister-Fibern mitten in
einem Block verlangt.

**Entscheid 3: `fn` ist rein.** Entscheidungslogik ohne Effekte ist testbar
und später replaybar. Effekte nur in `task`/`workflow`.

**Entscheid 4: Effekte sind Anfragen.** Statische Prüfung verhindert
undeklarierte Aufrufe; die Berechtigung entscheidet der Broker. Wildcards
werden nie vorab genehmigt.

**Entscheid 5: kooperative Fibers auf einem Thread.** Gleiches Modell wie der
heutige Node-Orchestrator (Event-Loop), deterministischer als Threads,
genug für I/O-gebundene Orchestrierung. Mehrkern-Ausführung ist Phase-3-Forschung.
