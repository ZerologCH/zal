# ZAL: Anleitung für den Einstieg

ZAL ist eine kleine, statisch typisierte Sprache für Abläufe, die externe Werkzeuge (Tools) aufrufen: Build-Pipelines, Agenten-Schritte, Datenflüsse. Die Sprache prüft vor dem Lauf, welche Werkzeuge ein Programm überhaupt benutzen will (Effekte), kennt Nebenläufigkeit als Sprachmittel und führt Programme in einer VM aus, die jeden Werkzeugaufruf einzeln freigeben lässt.

Diese Anleitung ist für Entwickler und KI-Agenten, die ZAL noch nie gesehen haben. Alle Codebeispiele liegen als Dateien in `docs/anleitung-beispiele/` und wurden mit `zal check` geprüft (wo es einen Einstieg gibt, auch mit `zal run`). Die Referenz ist `SPEC.md`, ein kompaktes Merkblatt für Agenten steht in `docs/AGENTS.md`.

## 1. Bauen und Installieren

```bash
cargo build --release -p zal-cli
# Programm: target/release/zal
target/release/zal --version
```

Die wichtigsten Befehle:

| Befehl | Zweck |
|---|---|
| `zal check datei.zal [--json]` | Typen und Effekte prüfen |
| `zal run datei.zal --entry name [--args '[...]'] [--mock m.json] [--policy p.json]` | ausführen |
| `zal test datei.zal [--mock m.json]` | `test`-Blöcke ausführen |
| `zal fmt datei.zal [--check] [--write]` | formatieren |
| `zal build datei.zal [-o out.zir.json]` / `zal ir datei.zal` | IR schreiben / anzeigen |
| `zal native datei.zal [-o dir]` | zu einer nativen Binärdatei übersetzen |
| `zal bench ...` | wiederholte Läufe messen |

## 2. Das erste Programm

Jede Datei beginnt mit `module`. Kommentare beginnen mit `//`.

```zal
module anleitung.hallo

// `fn` ist rein: keine Tools, kein `requires`. `print` ist zum Debuggen erlaubt.
fn gruss(name: Str) -> Str {
    "Hallo, " + name + "!"
}

fn main() -> Str {
    let text = gruss("Welt")
    print(text)
    text
}
```

Prüfen und ausführen:

```bash
zal check docs/anleitung-beispiele/01_hallo.zal
# ok: module anleitung.hallo — 2 functions, 0 externs
zal run docs/anleitung-beispiele/01_hallo.zal --entry main
# Hallo, Welt!        (von print, auf stdout)
# "Hallo, Welt!"      (Rückgabewert als JSON)
```

`--entry` nennt die Funktion, die startet. Hat sie Parameter, übergibst du sie als JSON-Array: `--args '["demo","staging"]'`.

## 3. Sprachgrundlagen

Die ganze Tour steht in `02_grundlagen.zal`. Die Abschnitte darunter erklären sie Stück für Stück.

```zal
module anleitung.grundlagen

// Record: benannte Felder, Konstruktion mit `Name { ... }`.
type Punkt = { x: Int, y: Int }

// Enum: Varianten mit oder ohne Nutzlast.
enum Form { Kreis(Int), Quadrat(Int), Leer }

fn flaeche(f: Form) -> Int {
    match f {
        Form.Kreis(r) => 3 * r * r
        Form.Quadrat(a) => a * a
        Form.Leer => 0
    }
}

fn vorzeichen(n: Int) -> Str {
    if n < 0 { "negativ" } else if n == 0 { "null" } else { "positiv" }
}

fn summe(xs: List<Int>) -> Int {
    let mut total = 0
    for x in xs { total = total + x }
    total
}

fn bis_zehn() -> Int {
    let mut i = 0
    while true {
        i = i + 1
        if i >= 10 { break }
    }
    i
}

// Option: es gibt vielleicht keinen Wert.
fn erste_gerade(xs: List<Int>) -> Option<Int> {
    for x in xs {
        if x % 2 == 0 { return Some(x) }
    }
    None
}

// Result und `?`: bei Err sofort zurueck, sonst den Wert entpacken.
fn teile(a: Int, b: Int) -> Result<Int, Str> {
    ensure b != 0, "Division durch null"
    Ok(a / b)
}

fn rechne(a: Int, b: Int, c: Int) -> Result<Int, Str> {
    let ab = teile(a, b)?
    let abc = teile(ab, c)?
    Ok(abc)
}

// Listen und Maps: `push` und `map_set` liefern neue Werte, also zuweisen.
fn woerter_zaehlen(woerter: List<Str>) -> Map<Int> {
    let mut m: Map<Int> = map_new()
    for w in woerter {
        let alt = match map_get(m, w) { Some(n) => n, None => 0 }
        m = map_set(m, w, alt + 1)
    }
    m
}

fn main() -> Int {
    let p = Punkt { x: 3, y: 4 }
    let (a, b) = (p.x, p.y)
    print(vorzeichen(a - b))
    let werte = [1, 3, 4, 7]
    let gerade = match erste_gerade(werte) { Some(n) => n, None => -1 }
    let r = match rechne(100, 5, 2) {
        Ok(n) => n
        Err(e) => {
            print(e)
            0
        }
    }
    let zaehler = woerter_zaehlen(["a", "b", "a"])
    let n_a = match map_get(zaehler, "a") { Some(n) => n, None => 0 }
    // Summe 15 + gerade 4 + rechne 10 + Zaehler 2 + Schleife 10 + Quadrat 4
    summe(werte) + gerade + r + n_a + bis_zehn() + flaeche(Form.Quadrat(2))
}

test "Flaechen stimmen" {
    assert flaeche(Form.Kreis(2)) == 12, "Kreis r=2"
    assert flaeche(Form.Leer) == 0
}

test "teile gibt Err bei null" {
    let r = teile(1, 0)
    assert match r { Ok(_) => false, Err(_) => true }, "erwartet Err"
}

test "Gesamtsumme" {
    // 15 + 4 + 10 + 2 + 10 + 4
    assert main() == 45, "Summe stimmt"
}
```

### Typen

| Typ | Beispiel |
|---|---|
| `Int` | `42`, `10_000` (64 Bit; Überlauf und Division durch 0 brechen den Lauf ab) |
| `Bool`, `Str`, `Unit` | `true`, `"text"`, `()` |
| `Dur` | `250ms`, `5s`, `2min`, `1h` |
| `List<T>`, `Map<T>` | `[1, 2]`, `map_new()`; Map-Schlüssel sind immer `Str` |
| `Option<T>` | `Some(x)`, `None` |
| `Result<T, E>` | `Ok(x)`, `Err(e)` |
| Tupel | `(1, "a")`, Zugriff `t.0`, zerlegen mit `let (a, b) = t` |

Funktionssignaturen sind immer explizit. Fehlt `-> T`, ist der Rückgabetyp `Unit`. Leere Container und `None` brauchen Kontext: `let m: Map<Int> = map_new()`.

### Variablen, Blöcke, Ausdrücke

- `let x = 1` ist unveränderlich, `let mut x = 1` erlaubt `x = x + 1`.
- Blöcke sind Ausdrücke: der letzte Ausdruck ohne `;` ist der Wert. Es gibt kein `;`.
- `if` und `match` sind ebenfalls Ausdrücke. `if` ohne `else` hat den Typ `Unit`.
- `match` muss bei `Bool`, `Option`, `Result` und Enums vollständig sein. Für `Int` und `Str` braucht es `_ =>`.
- Muster sind nur eine Ebene tief: `Some(x)`, `Err(_)`, `Form.Kreis(r)`. Verschachteltes `Some(Ok(x))` gibt es nicht; matche zweimal.

### Schleifen

`for x in liste { ... }` und `while bedingung { ... }`, dazu `break` und `continue`. `range(n)` liefert `[0, ..., n-1]`.

### Records und Enums

```zal
type Punkt = { x: Int, y: Int }            // Record
enum Form { Kreis(Int), Quadrat(Int), Leer }  // Enum
```

Konstruktion: `Punkt { x: 3, y: 4 }`, `Form.Kreis(2)`, `Form.Leer`. Feldzugriff: `p.x`. Im Kopf von `if`, `while`, `for` und `match` beginnt `Name {` den Block; setze ein Record-Literal dort in Klammern.

### Option, Result und `?`

- `e?` auf einem `Result<T, E>` liefert `T` oder gibt `Err(e)` aus der aktuellen Funktion zurück. Diese muss `Result<_, E>` mit demselben `E` liefern.
- `e?` auf einer `Option<T>` liefert `T` oder gibt `None` zurück.
- `ensure bedingung, fehler` gibt `Err(fehler)` zurück, wenn die Bedingung falsch ist.
- `assert bedingung, "text"` bricht den Lauf ab (für Tests).

### Listen und Maps

Alle Werte sind unveränderlich; `push` und `map_set` liefern eine neue Liste bzw. Map, die du zuweisen musst (`xs = push(xs, 1)`). Das ist billig: der Compiler ändert an Ort und Stelle, wenn niemand sonst die Liste hält.

| Builtin | Bedeutung |
|---|---|
| `len(x)` | Länge von `List`, `Map`, `Str` |
| `push(xs, v)`, `set(xs, i, v)`, `remove(xs, i)` | neue Liste |
| `xs[i]` | Element; bricht ab, wenn der Index ausserhalb liegt |
| `get(xs, i)` | `Option<T>`, kein Abbruch |
| `range(n)` | `[0, ..., n-1]` |
| `map_new()`, `map_set(m, k, v)`, `map_get(m, k)`, `map_has(m, k)`, `map_keys(m)` | Maps; `map_get` liefert eine `Option` |
| `str(x)` | in `Str` umwandeln |
| `print(s)` | Debug-Ausgabe (auch in `fn` erlaubt) |
| `json_str(s)`, `json_get_str(doc, feld)` | JSON-Hilfen |
| `now_ms()` | Zeitstempel (macht eine Funktion nicht wiederholbar) |

`xs[i] = v` gibt es nicht; schreibe `xs = set(xs, i, v)`.

## 4. Effekte: Tools, `requires`, Policy

### Tools deklarieren

Ein Tool ist eine Operation des Hosts (Prozess, Dienst, Mock). Du deklarierst es mit `extern`:

```zal
extern tool build(repo: Str) -> Result<Artefakt, Str>
extern tool test(a: Artefakt) -> Result<Bericht, Str> readonly
```

- Die Art ist eine von `tool net file secret worker deploy`. Das Tool `build` der Art `tool` hat den Effekt `tool.build`.
- Fehler des Tools kommen als `Err` im deklarierten `Result` zurück. Ausfälle der Infrastruktur brechen den Lauf ab.
- `readonly` verspricht: keine Nebenwirkungen. Nur dann darf ZAL unabhängige Aufrufe selbst parallel ausführen (siehe Abschnitt 5).
- Werte gehen als JSON über die Grenze: Records als Objekte, Listen als Arrays, `Result` als `{"ok": v}` bzw. `{"err": e}`.

### Warum `fn` rein ist: fn, task, workflow

| Art | `requires` | ruft Tools auf | `budget` |
|---|---|---|---|
| `fn` | verboten | nein | nein |
| `task` | ja | ja | nein |
| `workflow` | ja | ja | ja (Einstiegspunkt) |
| `test "name"` | optional | nur in Tests | nein |

Eine `fn` darf keine Effekte haben. So gilt für sie: gleiche Eingabe, gleiche Ausgabe, kein Seiteneffekt. Wer Werkzeuge braucht, schreibt eine `task` oder einen `workflow` und nennt jeden Effekt in `requires`. Der Compiler prüft das auch transitiv: ruft eine `task` eine andere `task` auf, braucht sie deren Effekte ebenfalls. `requires tool.*` deckt alle Effekte der Art `tool` ab; bevorzuge explizite Listen.

Ein `workflow` ist der Einstieg und darf ein `budget` tragen (Abschnitt 5).

### Beispiel: Build-Pipeline

```zal
module anleitung.pipeline

type Artefakt = { id: Str, groesse: Int }
type Bericht = { ok: Bool, funde: Int }

// Tools sind Operationen des Hosts. Eigene Fehler kommen als Err zurueck.
extern tool build(repo: Str) -> Result<Artefakt, Str>
extern tool test(a: Artefakt) -> Result<Bericht, Str> readonly
extern tool deploy(a: Artefakt, ziel: Str) -> Result<Str, Str>

// Eine `task` darf Effekte nutzen, muss sie aber in `requires` nennen.
task pruefen(a: Artefakt) -> Result<Unit, Str>
    requires tool.test
{
    let b = test(a)?
    ensure b.ok, "Tests fehlgeschlagen"
    ensure b.funde == 0, "Funde: " + str(b.funde)
    Ok(())
}

// Ein `workflow` ist der Einstiegspunkt und darf ein `budget` haben.
workflow release(repo: Str, ziel: Str) -> Result<Str, Str>
    requires tool.build, tool.test, tool.deploy
    budget { timeout: 5min, max_parallel: 4, max_tool_calls: 20 }
{
    let a = build(repo)?
    pruefen(a)?
    deploy(a, ziel)
}

// Tests bekommen ihre `requires` automatisch erteilt; Tools antworten per Mock.
test "release laeuft durch"
    requires tool.build, tool.test, tool.deploy
{
    let r = release("demo", "staging")
    assert match r { Ok(_) => true, Err(_) => false }, "erwartet Ok"
}
```

### Mock-Host und Policy-Datei

Lokal läuft das Programm gegen einen **Mock-Host**, der die Tools simuliert. Pro Tool legst du die Antwort fest:

```json
{ "tools": {
    "build":  { "latency_ms": 10, "result": { "ok": { "id": "art-1", "groesse": 4096 } } },
    "test":   { "latency_ms": 10, "result": { "ok": { "ok": true, "funde": 0 } } },
    "deploy": { "latency_ms": 10, "result": { "ok": "https://staging.example/art-1" } }
} }
```

- `result` ist die Antwort im Host-Format (`{"ok": ...}` oder `{"err": ...}`).
- `latency_ms` simuliert Wartezeit, `fail_first: n` lässt die ersten `n` Aufrufe scheitern (nützlich für `retry`).

Die **Policy-Datei** entscheidet, was wirklich erlaubt ist. `requires` ist nur eine Anfrage; erst `grant` erteilt sie:

```json
{ "grant": ["tool.build", "tool.test", "tool.deploy"],
  "caps": { "timeout_ms": 60000, "max_parallel": 4, "max_tool_calls": 50 } }
```

- `grant`: Liste der erlaubten Effekte. Ohne `--policy` ist nichts erlaubt (fail closed).
- `caps` (optional): harte Grenzen des Hosts mit `timeout_ms`, `max_parallel` und `max_tool_calls`.
- Vor dem Start muss jeder konkrete Effekt des Einstiegs freigegeben sein, sonst läuft nichts. Zusätzlich wird jeder einzelne Aufruf geprüft; ein verweigerter Aufruf bricht den Lauf ab.

```bash
zal run docs/anleitung-beispiele/03_pipeline.zal --entry release \
  --args '["demo","staging"]' \
  --mock docs/anleitung-beispiele/03_pipeline.mock.json \
  --policy docs/anleitung-beispiele/03_pipeline.policy.json
# { "ok": "https://staging.example/art-1" }
```

Ohne `--policy` endet derselbe Aufruf mit `runtime error: preflight denied: capability `tool.build` not granted`.

Für echte Tools startet `zal run --host '<befehl>'` einen Prozess, der zeilenweise JSON spricht (`{"id":1,"tool":"build","effect":"tool.build","args":{...}}` hinein, `{"id":1,"ok":{"ok":...}}` heraus). Ein Minimalbeispiel liegt in `examples/hosts/echo-host.mjs`.

## 5. Nebenläufigkeit und Budgets

ZAL nutzt strukturierte, kooperative Nebenläufigkeit. Die Konstrukte im Überblick:

| Konstrukt | Typ | Bedeutung |
|---|---|---|
| `parallel { a  b }` | Tupel `(A, B)` | alle Zweige laufen, warte auf alle |
| `race { a  b }` | `T` | der erste fertige gewinnt, die anderen werden abgebrochen |
| `timeout(5s) { ... }` | `Option<T>` | `None`, wenn der Block zu lange braucht |
| `retry(3, backoff: 250ms) { ... }` | `Result<T, E>` | wiederholt bei `Err` (Block muss ein `Result` liefern) |
| `spawn e` / `await t` | `Task<T>` / `T` | jetzt starten, später einsammeln |
| `join(liste)` | `List<T>` | wartet auf eine Liste von Tasks |
| `await_any(liste)` | `(Int, T)` | Index und Wert eines fertigen Tasks |
| `cancel(t)` | `Unit` | bricht einen Task samt Kindern ab |
| `sleep(d)` | `Unit` | wartet die Dauer `d` |

```zal
module anleitung.nebenlaeufig

extern tool fetch(url: Str) -> Result<Str, Str> readonly
extern tool flaky(x: Str) -> Result<Str, Str>

// parallel: alle Zweige laufen, das Ergebnis ist ein Tupel.
task beide(a: Str, b: Str) -> Result<Int, Str>
    requires tool.fetch
{
    let (x, y) = parallel {
        fetch(a)
        fetch(b)
    }
    let s1 = x?
    let s2 = y?
    Ok(len(s1) + len(s2))
}

// race: der erste fertige Zweig gewinnt, die anderen werden abgebrochen.
task schnellster(a: Str, b: Str) -> Result<Str, Str>
    requires tool.fetch
{
    race {
        fetch(a)
        fetch(b)
    }
}

// timeout: Some(wert) oder None, wenn zu langsam.
task begrenzt(u: Str) -> Result<Str, Str>
    requires tool.fetch
{
    let r = timeout(2s) { fetch(u) }
    match r {
        Some(v) => v
        None => Err("zu langsam")
    }
}

// retry: der Block muss ein Result liefern und wird bei Err wiederholt.
task mit_retry(x: Str) -> Result<Str, Str>
    requires tool.flaky
{
    retry(3, backoff: 20ms) { flaky(x) }
}

// spawn / await / join / await_any / cancel / sleep
task zeug(urls: List<Str>) -> Result<Int, Str>
    requires tool.fetch
{
    let mut ts: List<Task<Result<Str, Str>>> = []
    for u in urls { ts = push(ts, spawn fetch(u)) }
    let (k, erstes) = await_any(ts)
    print("erster fertig: Index " + str(k))
    let _ = erstes
    sleep(10ms)
    let alle = join(ts)
    let mut n = 0
    for r in alle { n = n + len(r?) }
    let extra = spawn fetch("unnoetig")
    cancel(extra)
    Ok(n)
}

workflow main() -> Result<Int, Str>
    requires tool.fetch, tool.flaky
    budget { timeout: 30s, max_parallel: 4, max_tool_calls: 50 }
{
    let n = beide("a", "bb")?
    let s = schnellster("x", "y")?
    let t = begrenzt("z")?
    let f = mit_retry("q")?
    let z = zeug(["u1", "u2", "u3"])?
    Ok(n + len(s) + len(t) + len(f) + z)
}
```

```bash
zal run docs/anleitung-beispiele/04_nebenlaeufig.zal --entry main \
  --mock docs/anleitung-beispiele/04_nebenlaeufig.mock.json \
  --policy docs/anleitung-beispiele/04_nebenlaeufig.policy.json
# { "ok": 49 }
```

Regeln, die du kennen musst:

1. **Isolation.** Ein Zweig oder Block sieht äussere Variablen nur als schreibgeschützte Kopie. Zuweisen ist ein Compilerfehler. Gib den Wert stattdessen aus dem Block zurück.
2. **Eigener Geltungsbereich.** `return` und `?` in einem solchen Block verlassen den Block, nicht die Funktion. Darum wiederholt `retry(3) { a(x)?  b(y) }` bei jedem Fehlschlag den ganzen Block.
3. **Strukturierte Lebensdauer.** Wenn ein Task endet, werden alle von ihm gestarteten und nicht erwarteten Kinder abgebrochen.
4. `await` auf einen abgebrochenen Task bricht den Lauf ab.

### Automatische Parallelität

Bei `readonly`-Tools musst du nichts tun: mehrere aufeinanderfolgende `let x = lesen(..)` ohne Abhängigkeit untereinander laufen gleichzeitig, ebenso eine `for`-Schleife, die nur `acc = push(acc, lesen(..))` ausführt. Ergebnis und Reihenfolge bleiben gleich. Nicht-`readonly`-Aufrufe werden nie umgeordnet.

### Budgets

Nur ein `workflow` darf ein Budget deklarieren, und nur mit literalen Werten:

```zal
workflow w() requires tool.build budget { timeout: 5s, max_parallel: 4, max_tool_calls: 50 } { ... }
```

- `timeout`: Gesamtfrist; danach bricht der Lauf ab.
- `max_parallel`: höchstens so viele Tool-Aufrufe gleichzeitig; weitere warten.
- `max_tool_calls`: der erste Aufruf über dem Limit bricht den Lauf ab.

Die `caps` der Policy gelten zusätzlich, unabhängig vom Programm. Budgets von verschachtelten Workflows werden in v0.1 nicht erzwungen.

## 6. Tests, Formatieren, Agenten-Modus, Native

### Tests

```zal
test "release laeuft durch" requires tool.build, tool.test, tool.deploy {
    let r = release("demo", "staging")
    assert match r { Ok(_) => true, Err(_) => false }, "erwartet Ok"
}
```

`zal test` führt jeden `test`-Block aus und erteilt ihm genau seine `requires`. Tools antworten über `--mock`:

```bash
zal test docs/anleitung-beispiele/02_grundlagen.zal
# ok    Flaechen stimmen
# ok    teile gibt Err bei null
# ok    Gesamtsumme
# 3 passed, 0 failed
zal test docs/anleitung-beispiele/03_pipeline.zal --mock docs/anleitung-beispiele/03_pipeline.mock.json
```

Achtung: ein Testname teilt sich den Namensraum mit den Funktionen. Heisst ein Test wie eine Funktion (`test "flaeche"` bei `fn flaeche`), läuft stattdessen die Funktion, und es schlägt mit "takes 1 argument(s), got 0" fehl oder prüft nichts. Verwende Namen mit Leerzeichen oder Satzform.

### Formatieren

```bash
zal fmt datei.zal            # kanonische Form auf stdout
zal fmt datei.zal --check    # Exit 1, wenn sich etwas ändern würde
zal fmt datei.zal --write    # in der Datei ersetzen
```

### `zal check --json` für Agenten

```bash
zal check docs/anleitung-beispiele/fehler/e2_requires.zal --json
```

```json
{"file":"...e2_requires.zal","line":4,"col":0,"severity":"error","message":"calling `build` needs capability `tool.build`; add `requires tool.build` to `t`","hint":"add `requires tool.build` to `t`"}
```

Pro Diagnose eine JSON-Zeile mit `file`, `line`, `col`, `severity`, `message` und einem Korrekturvorschlag in `hint`. Bei Erfolg kommt `{"ok":true,"module":...}`. Ein Agent schreibt, prüft, liest `hint`, korrigiert und wiederholt.

### Native Übersetzung

```bash
# Kopie von 03_pipeline.zal unter dem Namen pipeline.zal (Dateiname ohne führende Ziffer)
zal native pipeline.zal -o build-dir
target/native-build/release/pipeline --entry release --args '["demo","staging"]' \
  --mock 03_pipeline.mock.json --policy 03_pipeline.policy.json
```

`zal native` erzeugt ein Rust-Projekt und baut es mit `cargo` zu einer Binärdatei (`target/native-build/release/<name>`; `cargo` muss im `PATH` liegen). Der Dateiname wird zum Paketnamen und darf deshalb nicht mit einer Ziffer beginnen. Die Binärdatei nimmt dieselben Optionen `--entry`, `--args`, `--mock`, `--policy` wie `zal run`, dazu `--iters` für Messläufe.

## 7. Häufige Fehler

Die Meldungen unten stammen aus dem Compiler (`compiler/src/check.rs`); die Fehlerdateien liegen in `docs/anleitung-beispiele/fehler/`. Das Präfix `datei:zeile: error:` ist weggelassen.

| Meldung | Bedeutung und Lösung |
|---|---|
| `calling `build` needs capability `tool.build`; add `requires tool.build` to `t`` | Die `task`/`workflow` ruft ein Tool auf, ohne den Effekt zu nennen. In `requires` ergänzen. Gilt auch für aufgerufene Tasks (transitiv). |
| `calling `build` needs capability `tool.build`; `fn g` is pure; only a `task` or `workflow` may use effects` | Tool-Aufruf in einer `fn`. Aus der `fn` eine `task` machen. |
| `` `fn f` is pure and cannot declare `requires`; make it a `task` `` | `requires` an einer `fn`. Als `task` schreiben. |
| `cannot assign to `x`; declare it with `let mut`` | Neuzuweisung einer unveränderlichen Variable. `let mut x = ...`. |
| `cannot assign to `n` (variables captured by a concurrent block are read-only copies)` | Zuweisung an eine äussere Variable in `parallel`, `spawn`, `retry` usw. Wert aus dem Block zurückgeben. |
| `return value of `f`: expected `Int`, found `Str`` | Typ des Rückgabewerts passt nicht zur Signatur. |
| `match on `Farbe` is not exhaustive; add the missing cases or `_ =>`` | Es fehlt ein Fall. Ergänzen oder `_ =>`. |
| `only a `workflow` can declare a budget` | `budget` an einer `task`. Nur `workflow` darf es. |
| `unknown extern kind `proxmox` (use one of tool, net, file, secret, worker, deploy)` | Ungültige Art bei `extern`. Eine der genannten wählen. (Bei `requires` lautet die Meldung `unknown effect kind in `requires ...``.) |
| `operands: expected `Option<Int>`, found `Int`` | `map_get`/`get` liefert eine `Option`; mit `match` auspacken. |
| `` `?` needs the enclosing function to return Result with the same error type: expected `Int`, found `Result<?1, Str>` `` | `?` in einer Funktion, die kein `Result` mit gleichem Fehlertyp liefert. Rückgabetyp anpassen. |
| `record `P` has no field `z`` | Feldname falsch geschrieben. |
| `` a `retry` body must produce a Result `` | Der `retry`-Block liefert kein `Result`. |
| `expected an expression, found `=`` | `xs[0] = 3` gibt es nicht. `xs = set(xs, 0, 3)`. |
| `expected an expression, found keyword `mut`` | `mut x = 1`. Richtig: `let mut x = 1`. |

Zwei stille Fallen ohne Fehlermeldung:

- `push(xs, 1)` ohne Zuweisung wirft das Ergebnis weg. Schreibe `xs = push(xs, 1)`.
- Ein Aufruf über eine Zeilengrenze (`f\n(x)`) sind zwei Anweisungen.

### Checkliste vor dem Abgeben

1. `module` steht in der ersten Zeile.
2. Jeder aufgerufene Tool-Effekt steht in `requires`, auch transitiv.
3. Keine Tool-Aufrufe, `requires` oder `budget` in einer `fn`.
4. Neu zugewiesene Variablen sind `let mut`.
5. `?` nur in Funktionen mit passendem `Result`-Typ.
6. Jedes `match` ist vollständig; jede `Option` aus `map_get`/`get` wird ausgewertet.
7. `zal check datei.zal`, danach `zal test` und `zal fmt --check`.
