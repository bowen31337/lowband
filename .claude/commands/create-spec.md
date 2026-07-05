# Create Project Spec (XML)

Generate an AutoForge-compatible XML project specification for claw-forge. This produces 100-300+
granular feature bullets that become individual agent tasks.

Supports two modes:
- **Greenfield**: building a new project from scratch → produces `app_spec.txt`
- **Brownfield**: adding features to an existing project → produces `additions_spec.xml`

---

## Auto-Detect Mode

**First**: check if `brownfield_manifest.json` exists in the current working directory.

```bash
test -f brownfield_manifest.json && echo "BROWNFIELD" || echo "GREENFIELD"
```

- If it exists → run the **Brownfield Flow** below
- If it does not exist → run the **Greenfield Flow** below

---

## Brownfield Flow

> Use when adding features to an existing codebase.

### Step 1: Load manifest + check for hotspot report

Read `brownfield_manifest.json` and extract:
- `stack` (language, framework, database)
- `test_baseline` (N tests, X% coverage)
- `conventions` (naming style, patterns, etc.)

Then check whether `boundaries_report.md` is present in the project root
(emitted by a prior `claw-forge boundaries audit`).  If it exists and
contains entries with score >= 5.0, surface them to the user before
proceeding:

```
Found a boundaries audit at boundaries_report.md.  These files are
extension hotspots — adding new features as <feature shape="plugin">
will collide with them unless they're refactored first:

  cli/main.py        score=8.4  pattern=registry
  core/router.py     score=6.7  pattern=route_table

Recommended:
  claw-forge boundaries apply --auto

Refactoring these into plugin-extensible patterns first will let your
new features land cleanly as plugins.

Proceed anyway?  [y / yes]   Refactor first?  [b / boundaries]
```

If the user picks `b`, stop the slash command — they'll come back
after the refactor.  If they pick `y`, record the hotspot list as a
warning in `<existing_context>` and continue to Step 2.

If `boundaries_report.md` doesn't exist, continue to Step 2 silently.

### Step 2: Gather addition details

Ask the user (one at a time):

1. **What are you adding?** Give it a name and one-sentence summary.
   - Example: "Stripe payments — let users subscribe to Pro plan via Stripe Checkout"

2. **Where does it live in the codebase?**
   - **Plugin** (lives in its own directory): "I'll add `plugins/payments/`
     for the Stripe code."  Used when the addition is vertical and isolated.
   - **Core** (cross-cutting): "I'll edit `core/middleware/auth.py` and
     `core/db/models/user.py`."  Used when the addition modifies shared
     infrastructure.

   For each feature, record either `plugin="<name>"` (plugin shape) or
   `touches_files="..."` (core shape).  This populates the new
   `<feature shape>` attributes in Phase 3 of the parser, which lets
   the dispatcher schedule for parallel safety.

3. **What must NOT change?** List any constraints.
   - Example: "Must not modify auth flow. All 47 existing tests must stay green."

4. **List the features to add in plain English** (one per line, action-verb format):
   - Example: "User can add a payment method via Stripe Elements"
   - Aim for 10–50 features for a medium addition.

5. **Break them into implementation phases** (optional — offer to auto-group):
   - Example: Phase 1: Stripe integration / Phase 2: Subscription UI / Phase 3: Webhooks

### Step 3: Generate `additions_spec.xml`

Use the brownfield template (`skills/app_spec.brownfield.template.xml`) and fill in:
- `<project_name>` from the addition name
- `<addition_summary>` from the summary
- `<existing_context>` from `brownfield_manifest.json` (manifest values win)
- `<features_to_add>` from the feature list
- `<integration_points>` from the code-touch areas
- `<constraints>` from the "must not change" list
- `<implementation_steps>` from the phases

Write the file as `additions_spec.xml` in the project root.

### Step 4: Show next steps

```
✅ Brownfield spec created: additions_spec.xml

📊 Summary:
  Features to add: <N>
  Phases: <K>
  Integration points: <M>
  Constraints: <C>

Next steps:
  1. Review additions_spec.xml — add/remove features as needed
  2. Run: claw-forge add --spec additions_spec.xml

💡 Tip: Be specific about constraints — agents will treat them as hard rules.
   "All existing tests must stay green" = agents run tests before committing.
```

### Example: Brownfield spec for Stripe payments

```xml
<project_specification mode="brownfield">
  <project_name>MyApp — Stripe Payments</project_name>
  <addition_summary>
    Add Stripe Checkout integration so users can subscribe to the Pro plan.
    Handles subscription lifecycle, webhooks, and billing portal access.
  </addition_summary>
  <existing_context>
    <stack>Python / FastAPI / PostgreSQL</stack>
    <test_baseline>47 tests passing, 87% coverage</test_baseline>
    <conventions>snake_case, async handlers, pydantic v2 models</conventions>
  </existing_context>
  <features_to_add>
    <category name="Payments">
      <feature index="1" shape="plugin" plugin="payments">
        <description>User can add a payment method via Stripe Elements</description>
      </feature>
      <feature index="2" shape="plugin" plugin="payments" depends_on="1">
        <description>User can subscribe to Pro plan via Stripe Checkout</description>
      </feature>
      <feature index="3" shape="core"
               touches_files="src/core/db/models/user.py">
        <description>Extends User model with stripe_customer_id field</description>
      </feature>
    </category>
  </features_to_add>
  <integration_points>
    Extends User model with stripe_customer_id field
    Adds /payments router alongside existing /auth and /projects routers
    New StripeService class in services/stripe_service.py
  </integration_points>
  <constraints>
    Must not modify existing auth flow
    All 47 existing tests must stay green
    Follow existing async handler pattern in routers/
  </constraints>
  <implementation_steps>
    <phase name="Stripe Integration">
      User can add a payment method via Stripe Elements
      System creates Stripe customer on first payment attempt
    </phase>
    <phase name="Subscription Flow">
      User can subscribe to Pro plan via Stripe Checkout
      Webhook handler processes subscription.created events
      User can access billing portal to manage subscription
    </phase>
  </implementation_steps>
  <success_criteria>
    All new features implemented and tested
    Existing test suite still 100% green
    Coverage maintained above 87%
  </success_criteria>
</project_specification>
```

---

## Greenfield Flow

> Use when building a new project from scratch.

### Phase 1: Project Identity

Ask the user (one at a time, conversationally):

1. **What are you building?** Get the project name and a 2-3 sentence description.
2. **Who is it for?** Target audience / users.
3. **What problem does it solve?** The core value proposition.

Summarize back: "So we're building **X** — a tool that helps **Y** by **Z**. Sound right?"

---

### Phase 2: Quick vs Detailed

Ask the user:

> **How detailed do you want to go?**
>
> - **Quick** (5 min): I'll derive the tech stack, database schema, and API from your features.
>   Good for MVPs.
> - **Detailed** (15 min): We'll go through tech stack, database design, API structure, and UI
>   layout together. Better for production apps.

Wait for their choice.

---

### Phase 3: Core Features (Conversational)

This is the most important phase. Ask the user to describe their app's main functionality in
natural language. Guide them through categories:

> **Let's map out what your app does.** Describe the main things a user can do — I'll turn each
> one into specific, testable feature bullets.
>
> Let's start with: **What happens when a user first opens your app?**
> (Registration, onboarding, landing page?)

After each response, derive granular bullets and confirm:

```
From what you described, I'm generating these features:

**Authentication & User Management (5 bullets)**  →  XML: <category name="Authentication &amp; User Management">
- User can register with email and password (returns 201 with user_id)
- User can login with email and password (returns 200 with a JWT access_token)
- System issues a refresh_token on login (saved to the refresh_tokens table)
- ...

Does this capture it? Anything to add or change?
```

The heading name in bold becomes the `name` attribute of a `<category>` element in `<core_features>`.
Keep a note of each confirmed heading — you will use them verbatim as `name` attributes in Phase 5.

Continue through categories:
- **Authentication & user management**
- **Core functionality** (the main thing the app does)
- **Data management** (CRUD, search, filtering, pagination)
- **UI/UX** (responsive design, loading states, error handling, notifications)
- **API layer** (validation, error responses, pagination format)
- **Admin features** (if applicable)
- **Integrations** (third-party services, webhooks, notifications)

**Target: 100-300 bullets total.** Each bullet should be a testable behavior starting with an
action verb:
- "User can..." / "System returns..." / "API validates..." / "UI displays..."

#### Bullet-writing rules (the validator contract)

`claw-forge validate-spec` runs four layers over every bullet. Write bullets that pass it
the first time — each rule below maps directly to a check that otherwise emits a WARNING or
ERROR that `/fix-spec` would have to clean up.

1. **Start with a recognised subject prefix** (Layer 1, WARNING otherwise). The bullet's
   first word must be one of: `User can` / `User cannot` / `System` / `API` / `UI` / `App` /
   `Admin` / `Service` / `Backend` / `Frontend` / `Database` / `Agent` / `Webhook` /
   `Background`. ✗ "Password reset link in email" → ✓ "System sends a password reset link to
   the user email".

2. **Embed one measurable outcome** (Layer 1, WARNING otherwise). Each bullet must contain a
   recognised observable result. The validator looks for any of: `returns <NNN>` /
   `(returns …` / `displays` / `shows` / `redirects to` / `saves to` / `emits` / `sends` /
   `creates` / `persists` / a `snake_case` field name / `HTTP <NNN>` / `<NNN> error` /
   `error message` / `toast notification`. ✗ "Handle errors appropriately" → ✓ "API returns
   422 with a field-level errors array when validation fails".

3. **One action per bullet — never compound** (Layer 1, **ERROR** otherwise). Do not join two
   actions with a connector. The validator hard-rejects these substrings: `and then`,
   `and also`, `and after`, `and receive`, `and redirect`, `and create`, `and send`,
   `and return`, `then login`, `then register`. ✗ "User can login and receive a JWT" → split
   into two bullets: "User can login …" and "System issues a JWT …". (Plain "and" between
   *nouns* — "email and password", "title and description" — is fine; only the listed
   action-joining phrases are rejected.)

4. **At least 6 words, no vague filler** (Layer 1, WARNING otherwise). Bullets under 6 words,
   or containing `etc` / `various` / `multiple` / `some` / `things` / `stuff` / `items`, are
   flagged. Enumerate instead of summarising.

5. **Cover every table and endpoint** (Layer 3, WARNING otherwise). Each table name in
   `<database_schema>` and each path in `<api_endpoints_summary>` must appear in at least one
   bullet. Mention table names verbatim (e.g. a bullet that references the `refresh_tokens`
   table) so the coverage cross-reference resolves.

---

### Phase 3.25: Architectural Shape

After confirming the feature list with the user (Phase 3) and before
overlap analysis (Phase 3.5), classify each feature as either a
**plugin** (vertical, lives in its own directory) or **core**
(cross-cutting, edits files used by every plugin).  The classification
ends up in the emitted XML as `<feature shape>` / `<feature plugin>`
attributes — the dispatcher reads these for parallel-safe scheduling.

#### Step 1 — Group features by likely shape

Read through the confirmed feature list and silently group:

- **Plugin candidates**: features whose description names a single
  domain noun ("user", "task", "billing", "notifications") and whose
  acceptance criteria all read like "user can …" or "system returns …
  for the X resource".  These typically own their own data model,
  routes, and tests, and can be added or removed without touching
  sibling plugins.
- **Core candidates**: features that say "all endpoints …", "every
  request …", "uniform error format", "shared logging", "global rate
  limit", "authentication middleware", "database migrations".  These
  are cross-cutting — they're touched by every plugin's request path.

A feature can be plugin-shape even if it depends on a core concern.
"User can register" is plugin-shape (lives in `plugins/auth/`) even
though it relies on the core `core/db/` connection pool.

#### Step 2 — Confirm with the user

Present the grouping back, naming the plugin directories:

```
Looking at your features, I'd structure them as:

Plugins (parallel-safe — each in its own directory):
  • plugins/auth/      — registration, login, password reset (5 features)
  • plugins/profile/   — view/edit profile, avatar upload (4 features)
  • plugins/tasks/     — CRUD, search, tag filter, pagination (8 features)

Core (cross-cutting — touch every plugin's request path):
  • core/middleware/   — JWT validation, request logging (2 features)
  • core/errors/       — RFC7807 error envelope (1 feature)
  • core/db/           — connection pool, migrations runner (2 features)

Sound right?  Edits welcome:
  - Reclassify a feature: "move feature 14 to core"
  - Rename a plugin:      "rename profile to user-profile"
  - Add a category:       "add plugins/notifications"
```

The user can:
- **Accept** → record the classification.
- **Edit** by line: "move 14 to core", "rename profile to user-profile",
  "split tasks into tasks-crud and tasks-search".
- **Skip** → emit the spec without `shape`/`plugin` attributes (legacy
  behaviour).  Phase 5 emits unchanged.

#### Step 3 — Persist the classification

Build a per-feature dict in memory:

```
feature_shape[<index>] = {
    "shape": "plugin" | "core",
    "plugin": "<plugin_name>" | None,         # set when shape="plugin"
    "touches_files": ["..."] | None,           # set when shape="core" only
}
```

Phase 5 reads this when emitting `<feature>` elements.  Plugin features
get `shape="plugin" plugin="X"` and the parser auto-derives `touches_files`.
Core features get `shape="core" touches_files="..."` (the prose from Step
2 — typically a single file path the user names — becomes the
`touches_files` value).

#### Step 4 — Shape rules (the validator contract)

Layer 4 of `validate-spec` is strict by default. Apply these rules while classifying so the
spec passes without `/fix-spec`:

- **Migration / schema work is always `shape="core"`** (Gap 8, **ERROR** otherwise). If a
  feature description contains any of `migration`, `alembic`, `database schema`, `DDL`,
  `foreign key`, `primary key`, `create table`, `alter table`, `drop table`, `add column`,
  `drop column`, `alter column`, `create index`, or `unique constraint`, it mutates alembic's
  shared revision tree and **must** be emitted as
  `shape="core" touches_files="migrations/versions/**"`. Never classify a migration feature as
  a plugin — parallel agents would write colliding revision files.

- **`shape="plugin"` must carry `plugin=`** (Gap 1, **ERROR** otherwise). Every plugin feature
  needs a `plugin="<slug>"` attribute (or an explicit `touches_files=`). Derive the slug from
  the category name: lowercase, non-alphanumerics → dashes (e.g. "User Profile" →
  `plugin="user-profile"`).

- **Keep core `touches_files` narrow** (Gap 3, ERROR in strict). A core feature's
  `touches_files` glob must not overlap any plugin directory. Use specific paths like
  `src/core/middleware/auth.py` or `src/core/**`, never a blanket `src/**` that would swallow
  `src/plugins/<name>/`.

#### Failure modes

- **User skips classification** → emit Phase 5 unchanged; no `shape`
  attributes.  The legacy parsing path still works; the dispatcher's
  file-claim layer treats every feature as opt-out (no locking
  attempted).
- **A feature can't be classified** (LLM unsure or user says "I don't
  know") → leave that feature unclassified in `feature_shape`.  Phase 5
  emits without `shape` for that feature.
- **User declares a plugin name that conflicts with an existing
  filesystem path** in brownfield mode → warn but accept (the
  boundaries-harness can refactor the colliding file later).

---

### Phase 3.5: Overlap Analysis

After confirming the feature list with the user, audit for **merge-conflict risk** between
features that would be dispatched in parallel. The earlier you serialize known-overlapping
features, the fewer wasted agent runs you'll have downstream.

#### Step 1 — Run the analysis prompt on the bullet list

Number the confirmed bullets from Phase 3 starting at 1. Then, as the LLM executing
`/create-spec`, mentally apply the following prompt to that numbered list:

> You are auditing a feature spec for merge-conflict risk. Below are N feature bullets, each
> numbered. Find pairs where implementing both would force conflicting edits to the same
> file or function — i.e. they would both modify the same hunks if scheduled in parallel.
>
> A pair is overlapping ONLY if changing one without the other would force a merge
> conflict. Belonging to the same category alone is not overlap.
>
> Return JSON only:
> ```json
> [{"a": <int>, "b": <int>, "surface": "<file_or_concept>", "rationale": "<one sentence>"}]
> ```
> Empty list `[]` if no overlaps. No prose outside the JSON.

#### Step 2 — Resolve each overlap interactively

For every entry returned, present:

```
Overlap detected:
  #<a>  <description of feature a>
  #<b>  <description of feature b>
  Shared surface: <surface>
  Rationale: <rationale>

Resolution? [s] serialize (#<b> depends on #<a>)  [k] keep parallel  [q] quit
```

- **`s`** → record an explicit edge: feature `<b>` will be emitted with `depends_on="<a>"` so
  the runtime DAG runs `<a>` first and `<b>` only after.
- **`k`** → record the user's decision to keep parallel; do not flag this pair again on retry.
- **`q`** → abort `/create-spec` cleanly; do not write any files.

If the user selects `m` (merge), explain that merging is out of scope for this phase — they
can manually combine the bullets in Phase 3 and re-run, or pick `s` for now.

#### Step 3 — Persist resolutions through Phase 5

In memory, build a list `serialized_pairs: list[tuple[int, int]]` of `(later, earlier)`
feature numbers from each `s` decision. When Phase 5 emits the XML, every feature that
appears as `later` in any pair must use the new `<feature>` element form with explicit
attributes:

```xml
<core_features>
  <category name="DSL Compiler">
    <feature index="14">
      <description>System displays parse errors on stderr</description>
    </feature>
    <feature index="18" depends_on="14">
      <description>System displays side-by-side diff in terminal</description>
    </feature>
  </category>
</core_features>
```

Multiple dependencies are comma-separated: `depends_on="14,15,16"`. Features without edges
may continue using the legacy bullet form; both coexist within the same `<category>`.

When Phase 3.25 classification was accepted, combine `shape`/`plugin` with `index` and
`depends_on` in the same element:

```xml
<!-- Phase 3.25 + Phase 3.5 combined: plugin-shape + dependency edge -->
<feature index="14" shape="plugin" plugin="auth">
  <description>User can register with email and password</description>
</feature>
<feature index="18" shape="plugin" plugin="auth" depends_on="14">
  <description>System sends welcome email after registration</description>
</feature>
<feature index="20" shape="core"
         touches_files="src/core/middleware/auth.py">
  <description>All endpoints validate JWT on incoming requests</description>
</feature>
```

#### Failure modes

- **LLM returns malformed JSON** → re-prompt once with the schema; if still bad, skip the
  analysis with a one-line warning ("could not analyze overlaps; emitting spec without
  explicit edges") and continue to Phase 4.
- **Empty feature list** (Phase 3 produced 0 bullets) → skip Phase 3.5; downstream
  validation handles the empty-spec case.
- **No overlaps detected** (`[]`) → skip directly to Phase 4 with one line: "No overlap
  risk detected — features can run in parallel."

---

### Phase 4: Technical Details (Detailed mode only)

If the user chose **Detailed**, ask about:

1. **Tech stack preferences:**
   - Frontend: React/Vue/Svelte/Next.js? Styling: Tailwind/CSS modules?
   - Backend: Python (FastAPI/Django) / Node (Express/Fastify) / Go / Rust?
   - Database: SQLite/PostgreSQL/MySQL/MongoDB?

2. **Database schema:** Walk through the main tables/collections based on the features.

3. **API structure:** REST vs GraphQL? Authentication method (JWT, session, OAuth)?

4. **UI layout:** Dashboard style? Sidebar navigation? Single page or multi-page?

For **Quick** mode, derive sensible defaults from the features (React + Vite, FastAPI,
SQLite for dev / PostgreSQL for prod, JWT auth, REST API).

---

### Phase 5: Generate the Spec

Generate two files:

#### `app_spec.txt` (XML format)

Use the template structure from `claw_forge/spec/app_spec.template.xml` but filled with the
user's project details. The XML must include:

- `<project_specification>` root element
- `<project_name>`, `<overview>`, `<target_audience>`
- `<technology_stack>` with `<frontend>`, `<backend>`, `<database>`, `<communication>`, `<infrastructure>`
- `<prerequisites>` with environment setup bullet list
- `<core_features>` with categorized bullet lists (this is the bulk — 100-300 bullets)
- `<database_schema>` with `<tables>` containing `<table name="…">` / `<column>` elements
- `<api_endpoints_summary>` with `<domain name="…">` sections
- `<ui_layout>` with main structure
- `<design_system>` with color palette, typography, animations
- `<key_interactions>` with `<interaction number="N" name="…">` flows
- `<implementation_steps>` with `<phase name="…">` elements
- `<success_criteria>` with `<functionality>`, `<ux>`, and `<technical_quality>`

**Important:** Use `&amp;` for `&` in XML content. Each bullet in `<core_features>` becomes one
agent task.

**CRITICAL — `<core_features>` element format:**
Each category group inside `<core_features>` MUST be a `<category name="…">` element with the
full human-readable name as the `name` attribute. The name becomes the task category shown in the
Kanban board and used for routing and filtering.

✅ Correct — `<category name="…">` with descriptive names:
```xml
<core_features>
  <category name="Authentication &amp; User Management">
    - User can register with email and password (returns 201 with user_id)
    - User can login and receive JWT access_token and refresh_token
  </category>
  <category name="Receipt Scanning">
    - System sends the receipt image to OpenAI Vision API…
  </category>
  <category name="API Layer">
    - API returns consistent JSON envelope: { "data": ..., "error": null }
  </category>
</core_features>
```

❌ Wrong — bare snake_case tags lose display names; generic `<category>` loses all names:
```xml
<core_features>
  <authentication>...</authentication>     <!-- loses "&amp; User Management" -->
  <category>...</category>                 <!-- BAD: every task becomes "Category" -->
</core_features>
```

Use the exact heading confirmed with the user in Phase 3 as the `name` attribute.

**REQUIRED — final `<category name="End-to-End Verification">`:**
Always end `<core_features>` with an End-to-End Verification category containing
one `<feature>` per primary user journey, each declared `shape="core"` and
`touches_files="tests/e2e/**"`. These become the terminal tasks that write and
run e2e/integration tests against the fully-assembled app — the difference
between "units pass" and "the app actually runs". List the *real* journeys you
captured in Phase 3 (signup → core action → result), not a generic placeholder.

```xml
<core_features>
  ... feature categories ...
  <category name="End-to-End Verification">
    <feature index="40" shape="core" touches_files="tests/e2e/**">
      <description>System passes an end-to-end test of the primary journey (sign up, create a project, add a task) in which the app boots and the board displays the new task (Playwright/pytest test under tests/e2e/)</description>
    </feature>
    <feature index="41" shape="core" touches_files="tests/e2e/**">
      <description>System passes API integration tests where each documented endpoint returns 200 on the happy path and returns the documented 4xx error on key error cases</description>
    </feature>
  </category>
</core_features>
```

Why a real task, not prose: e2e steps written only inside `<phase>`/`<success_criteria>`
text are never materialized as tasks, so no agent writes them and they go
missing. Declaring them as `shape="core"` features makes them scheduled,
merge-gated, terminal task nodes. (If you omit this, `claw-forge plan` injects a
single fallback "End-to-End Verification" task automatically — but authoring the
specific journeys here is better: they're visible, editable, and split per flow.)
When `agent.acceptance_gate` is enabled, the project's test command — now
including these e2e tests — must pass before each task can complete.

**CRITICAL — `<database_schema>` format:**
Use `<table name="…">` with `<column>` children for each table. Full SQL-style column definitions.

```xml
<database_schema>
  <tables>
    <table name="users">
      <column>id UUID PRIMARY KEY DEFAULT gen_random_uuid()</column>
      <column>email VARCHAR(255) UNIQUE NOT NULL</column>
      <column>password_hash VARCHAR(255) NOT NULL</column>
      <column>created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()</column>
    </table>
    <table name="refresh_tokens">
      <column>id UUID PRIMARY KEY DEFAULT gen_random_uuid()</column>
      <column>user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE</column>
      <column>token_hash VARCHAR(255) NOT NULL UNIQUE</column>
      <column>expires_at TIMESTAMP WITH TIME ZONE NOT NULL</column>
    </table>
  </tables>
</database_schema>
```

**CRITICAL — `<api_endpoints_summary>` format:**
Use `<domain name="…">` sections with plain-text route lines (no bullet prefix needed).

```xml
<api_endpoints_summary>
  <domain name="Authentication">
    POST   /api/auth/register   - Register new user account
    POST   /api/auth/login      - Log in and receive JWT tokens
    POST   /api/auth/logout     - Log out and invalidate refresh token
  </domain>
  <domain name="Receipts">
    POST   /api/receipts/upload - Upload image and trigger OCR
    GET    /api/receipts        - List receipts with filters and pagination
    GET    /api/receipts/{id}   - Get single receipt with line items
    PUT    /api/receipts/{id}   - Update receipt fields
    DELETE /api/receipts/{id}   - Delete receipt and associated file
  </domain>
</api_endpoints_summary>
```

**CRITICAL — `<implementation_steps>` format:**
Use `<phase name="…">` elements with plain-text task lines inside.

**Phase titles must share a keyword with every category name** (Gap 9, ERROR in strict).
The validator infers each feature's dependencies from the phase whose title overlaps its
category name. A feature whose category shares **no word** with any phase title gets no
inferred deps and is flagged — under strict (the default) that's an ERROR, one per feature.
Generic titles like "Phase 2: Core Features" share nothing with a category like "Receipt
Scanning" and trip the whole category. **Derive phase titles from your category names** so
each category maps onto a phase: name the phase after the category (or include the category's
key noun). This clears Gap 9 for every feature in the category at once — no `depends_on`
surgery required.

```xml
<implementation_steps>
  <phase name="Phase 1: Authentication &amp; User Management">
    Implement users and refresh_tokens tables
    Implement POST /api/auth/register and POST /api/auth/login
  </phase>
  <phase name="Phase 2: Receipt Scanning">
    Implement the receipts table and OCR pipeline
    Build the receipt review UI
  </phase>
  <phase name="Phase N: End-to-End Verification">
    Write and run the e2e/integration tests for the primary user journeys
    Confirm the app boots and the happy paths pass end-to-end
  </phase>
</implementation_steps>
```

**Always end with a `End-to-End Verification` phase** whose title matches the
final `<category name="End-to-End Verification">`. Because phase-N features
depend on phase-(N-1)'s, the e2e features inherit a dependency on the prior
phase — and transitively on the whole build — so they dispatch **last**, after
every feature has merged and the full app is available to test. The shared
"End-to-End Verification" keyword between the category and this phase also
clears Gap 9 for the e2e features.

#### `claw-forge.yaml`

```yaml
project:
  name: <project-name>
  path: .

providers:
  - name: claude-oauth
    type: oauth
    enabled: true
    priority: 10

orchestrator:
  max_concurrent: 5
  retry_attempts: 3
  retry_delay_seconds: 5

features:
  # Generated by: claw-forge plan app_spec.txt
```

Show both files to the user and ask: "Does this look right? I'll write these files now."

Then write:
```bash
# Write app_spec.txt to project root
# Write claw-forge.yaml to project root
```

---

### Phase 6: Next Steps

After writing files, show:

```
✅ Project spec created!

📊 Summary:
  Features: <N> across <M> categories
  Phases: <K> implementation steps
  Tables: <T> database tables
  Endpoints: <E> API endpoints

Next steps:
  1. Review app_spec.txt — add/remove features as needed
  2. Run: claw-forge validate-spec app_spec.txt
     → Issues found? Run /fix-spec, then re-run validate-spec until clean
  3. Run: claw-forge plan app_spec.txt
  4. Run: claw-forge run --concurrency 5

   validate-spec is strict by default (since v0.8.46): Layer 4 shape gaps
   3, 5, and 9 are ERRORs, and the migration-shape gap 8 is an ERROR too.
   Pass --soft-shape only to opt out during a migration on-ramp.
   /create-spec emits shape-annotated <feature> elements and
   category-aligned phase titles throughout, so the spec is expected to
   pass on first run; if it doesn't, /fix-spec will auto-repair the
   failing elements.

💡 Tip: Each feature bullet = one agent task. More specific bullets = better agent output.
   Aim for 100-300 bullets for a full application.

💡 Tip: Features with `shape="plugin"` in your spec can be dispatched
   in parallel without merge conflicts (their `touches_files` are
   disjoint by construction).  Features with `shape="core"` serialize
   single-flight via the scheduler's cross-cutting rule.  See
   docs/commands.md → "claw-forge run" for parallelism settings.
```
