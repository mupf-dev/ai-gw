# Rezept: Mehrere Accounts rotieren

Stand: AIGW-125/126 · Zielgruppe: Betrieb

Mehrere Anbieter-Accounts (z. B. vier Anthropic-Accounts mit je 200 € Prepaid-Guthaben) lassen
sich hinter **einer** Modellgruppe bündeln. Jeder Account wird ein Deployment mit eigener
`ProviderCredential` und eigenem **Budget**. Die Clients sprechen nur den Gruppennamen an.
Ist ein Account erschöpft, nimmt ai-gw ohne Fehler den nächsten.

Es gibt zwei Varianten:

| Variante | Einstellung | Verhalten |
|---|---|---|
| **nacheinander** | `priority: 0..3`, Strategie beliebig (z. B. `weighted`) | Account 0 wird bis zur Umschaltgrenze genutzt, dann Account 1 usw. |
| **verteilt** | gleiche `priority`, `routing: budget-balanced` | Alle Accounts werden gleichmäßig belastet (gleicher Verbrauch in Prozent; unterschiedliche Budgets füllen sich anteilig). |

## Zwei Schutzschichten

1. **Budget je Deployment (AIGW-126):** ai-gw rechnet die Kosten jeder Antwort mit der
   Preisliste ab und bucht sie auf das Deployment, das geantwortet hat. Hat ein Deployment
   `switchAtPercent` seines Betrags erreicht, wird es bei der Auswahl übersprungen. Mit
   `enforcement: strict` (Standard) reserviert ai-gw vor jedem Versuch zusätzlich die
   geschätzten Kosten (Eingabe-Schätzung + `max_tokens` × Preis). Passt die Schätzung nicht
   mehr in den Rest, nimmt ai-gw für diese Anfrage das nächste Deployment.
2. **Fehler des Anbieters (AIGW-125):** Weicht das Budget in ai-gw vom echten Guthaben ab
   (z. B. andere Nutzung desselben Accounts oder Preise nicht aktuell), meldet Anthropic das leere
   Konto selbst. Erkannt werden:
   - HTTP 400 „Your credit balance is too low …“,
   - HTTP 400 „You have reached your specified (workspace) API usage limits …“,
   - HTTP 429 mit `error.details.error_code: enforced_spend_limit_reached`.

   ai-gw stuft das als `quota` ein. Die Anfrage geht ohne sichtbaren Fehler an das nächste
   Deployment, und der Circuit Breaker des leeren Accounts öffnet **sofort**. Nach `openSecs`
   prüft eine einzelne Probe, ob wieder Guthaben da ist. Für OpenAI/Azure gilt dasselbe bei
   `insufficient_quota`, `billing_hard_limit_reached` und `billing_not_active`.

Sind **alle** Accounts erschöpft, antwortet ai-gw mit `429` im Eingangsformat (Fehlerart
Rate-Limit, bei Anthropic `rate_limit_error`), Code `deployment_budget_exhausted` (Budgets in
ai-gw) bzw. `insufficient_quota` (vom Anbieter gemeldet). Bei Kalenderzeiträumen enthält die Antwort `Retry-After` bis zum
Zeitraumwechsel. Fallback-Ketten mit `on: [rate-limit]` oder `[any]` greifen vorher.

## Beispiel: vier Anthropic-Accounts nacheinander

```yaml
apiVersion: aigw/v1alpha1
kind: ProviderCredential
metadata: {name: anthropic-acct-1}
spec:
  provider: anthropic
  data:
    apiKey: {valueFrom: {env: AIGW_SECRET_ANTHROPIC_1}}
---
apiVersion: aigw/v1alpha1
kind: ProviderCredential
metadata: {name: anthropic-acct-2}
spec:
  provider: anthropic
  data:
    apiKey: {valueFrom: {env: AIGW_SECRET_ANTHROPIC_2}}
---
apiVersion: aigw/v1alpha1
kind: ProviderCredential
metadata: {name: anthropic-acct-3}
spec:
  provider: anthropic
  data:
    apiKey: {valueFrom: {env: AIGW_SECRET_ANTHROPIC_3}}
---
apiVersion: aigw/v1alpha1
kind: ProviderCredential
metadata: {name: anthropic-acct-4}
spec:
  provider: anthropic
  data:
    apiKey: {valueFrom: {env: AIGW_SECRET_ANTHROPIC_4}}
---
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: claude-sonnet}
spec:
  routing: weighted            # innerhalb einer Priorität; hier je Priorität nur ein Account
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: claude-sonnet-acct-1}
spec:
  modelGroup: claude-sonnet
  provider: anthropic
  providerModel: claude-sonnet-5
  credential: anthropic-acct-1
  priority: 0
  budget:
    amount: "200"              # in billing.currency (siehe unten)
    period: total              # Prepaid-Guthaben: kein Zurücksetzen
    switchAtPercent: 98        # 2 % Reserve: der letzte Request überzieht nicht
    warnAtPercent: [80, 95]    # Ereignis/Webhook budget.warning
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: claude-sonnet-acct-2}
spec:
  modelGroup: claude-sonnet
  provider: anthropic
  providerModel: claude-sonnet-5
  credential: anthropic-acct-2
  priority: 1
  budget: {amount: "200", period: total, switchAtPercent: 98, warnAtPercent: [80, 95]}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: claude-sonnet-acct-3}
spec:
  modelGroup: claude-sonnet
  provider: anthropic
  providerModel: claude-sonnet-5
  credential: anthropic-acct-3
  priority: 2
  budget: {amount: "200", period: total, switchAtPercent: 98, warnAtPercent: [80, 95]}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: claude-sonnet-acct-4}
spec:
  modelGroup: claude-sonnet
  provider: anthropic
  providerModel: claude-sonnet-5
  credential: anthropic-acct-4
  priority: 3
  budget: {amount: "200", period: total, switchAtPercent: 98, warnAtPercent: [80, 95]}
```

Dazu gehört ein **Preis** für `claude-sonnet-5` (Admin-UI „Preise“ oder `kind: Price`), sonst
kostet jede Anfrage 0 und das Budget greift nie. Anwenden:
`aigw config apply -c aigw.yaml accounts.yaml`.

## Variante: Kosten gleichmäßig verteilen

Alle Deployments erhalten dieselbe `priority`, die Gruppe die Strategie `budget-balanced`:

```yaml
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: claude-sonnet}
spec:
  routing: budget-balanced
---
# Deployments wie oben, aber alle mit priority: 0
```

Auswahl: ai-gw zieht zwei Kandidaten gewichtet zufällig („power of two choices“) und nimmt den
mit dem geringeren Budgetverbrauch in Prozent. Bei Gleichstand entscheidet das höhere Gewicht,
sonst der Zufall. Ein striktes Minimum würde unter Last alle gleichzeitigen Anfragen auf
denselben Account lenken (Herdenverhalten). Zwei Stichproben verteilen die Anfragen und halten
den Verbrauch trotzdem eng beieinander. In den Tests liegen vier gleiche Budgets nach 4 000
Anfragen innerhalb von 2 %. Unterschiedliche Budgets (z. B. 100/200/300/400) füllen sich
anteilig. Haben nicht alle Deployments der Gruppe ein Budget, vergleicht die Strategie die
absoluten Kosten. Für Deployments ohne Budget sind das die Kosten dieser Instanz seit dem Start.
Der Verbrauch kommt aus dem RAM der Budgetstände bzw. dem Zähler-Backend, nie per Anfrage aus
der Datenbank.

## Währung

Budgets laufen in **`billing.currency`** (Standard `USD`), genau wie die Preisliste. Anthropic
rechnet in **USD** ab. Mit `billing.currency: EUR` müssen Preise **und** Budget in EUR
gepflegt sein (Umrechnung zum eigenen Kurs). Für ein Guthaben von „200 €“ bei einem
USD-Account ist das Budget in USD anzugeben, also den Kaufbetrag in USD.

## Guthaben nachladen

Nach dem Nachkaufen **den Betrag erhöhen** (z. B. 200 → 400). Das geht in der Admin-UI unter
Deployment → Bearbeiten → Budget, per `PUT /admin/api/v1/deployments/{name}` oder per GitOps.
Der bisherige Verbrauch bleibt stehen. Die Änderung wirkt sofort auf allen Instanzen (Hot
Reload), und der Account wird wieder ausgewählt. Hat der Breaker den Account wegen „credit
balance too low“ geöffnet, lässt er nach `openSecs` (Standard 30 s) eine Probe durch.

Ein Zurücksetzen auf 0 gibt es bewusst nicht. Der Verbrauch ist Teil der Abrechnung (Rollups,
Kostenanalyse), und ein höherer Betrag bildet ein nachgeladenes Guthaben genau ab. Bei
`period: month` beginnt jeder Kalendermonat ohnehin neu.

## Überwachen

- **Admin-UI:** Auf der Deployment-Seite stehen Budget und Verbrauch in Prozent. Die Seite
  „Kosten“ führt die Deployment-Budgets mit Prognose.
- **API:** `GET /admin/api/v1/usage/budgets?level=deployment` liefert Modellgruppe,
  Umschaltgrenze, Verbrauch und Prozent. `GET /admin/api/v1/costs/budgets?level=deployment`
  liefert dazu die Prognose.
- **Metriken:** `aigw_deployment_budget_used`, `aigw_deployment_budget_amount` und
  `aigw_deployment_budget_used_ratio` mit den Labels `model_group` und `deployment`.
  Alarm z. B. bei `aigw_deployment_budget_used_ratio > 0.9`.
- **Webhook:** `budget.warning` (je `warnAtPercent`) und `budget.exhausted` mit
  `level: deployment`, `targetName` = Deployment.

## Grenzen

- Das Budget hängt am **Deployment**. Nutzt ein Account mehrere Modelle (z. B. Sonnet und
  Haiku in zwei Gruppen), teilt sich das Guthaben auf mehrere Deployments auf. Dann das
  Guthaben passend aufteilen. Die Erkennung „credit balance too low“ (Schicht 2) fängt
  Abweichungen ab.
- Ohne Redis (`counters.backend: redis`) sehen mehrere Instanzen fremden Verbrauch erst nach
  dem Abgleich (`billing.budget_sync_ms`, Standard 1 s). Die Reserve über `switchAtPercent`
  deckt das ab.
- Modellaufrufe von Plugins (z. B. das Prüfmodell von `content-safety`) und Embeddings des
  semantischen Caches prüfen das Budget des Deployments, werden aber nur auf Key/Team/Organisation
  gebucht, nicht auf das Deployment.
