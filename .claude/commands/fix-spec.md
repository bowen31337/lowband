# Fix Spec Issues

Run `claw-forge validate-spec` on the project spec, then iteratively fix all reported issues
— errors **and** warnings — until the spec is fully clean. Rewrites only the offending bullets;
never restructures the spec.

## Step 0: Handle parse-blocking errors

Run `claw-forge validate-spec <spec-file>` (after Step 1 finds the file). If
output starts with `Failed to parse spec:`, the spec has a parser-level
violation that must be fixed before any layer-based fixes can run.

**For unrecognized shape values** (e.g. `shape='ploogin'`):
1. Read the spec file. Find the `<feature shape='ploogin' ...>` element.
2. Compute Levenshtein distance from `'ploogin'` to `'plugin'` and `'core'`.
   - If best match is `'plugin'` with distance ≤ 2 → replace with `shape="plugin"`.
   - If best match is `'core'` with distance ≤ 2 → replace with `shape="core"`.
   - Otherwise → list under Manual Review (see Step 7).
3. Re-run validate-spec.

**For `<feature shape='core'>` missing `touches_files=`:**
This cannot be auto-fixed — `touches_files` requires domain knowledge of
which files the feature touches. List under Manual Review (see Step 7).

If parse-blocked after fixes, report the remaining error and stop.

## Step 1: Find the spec file

Look for the spec in the current directory:

```bash
ls app_spec.txt app_spec.xml additions_spec.xml 2>/dev/null | head -1
```

If multiple exist, prefer `app_spec.txt`, then `app_spec.xml`, then `additions_spec.xml`.
If none found, ask the user: "Which spec file should I fix?"

## Step 2: Run validate-spec

```bash
claw-forge validate-spec <spec-file> 2>&1
```

Strict shape validation is **on by default** (since v0.8.46), so Layer 4
shape gaps (3, 5, 9) and the migration-shape gap (8) surface as ERRORs and
get fixed in this same iteration — no flag needed. Add `--soft-shape` only
if the project hasn't yet shape-annotated its spec and you're knowingly
tolerating mixed-form output.

Capture the full output. **Do not use the exit code to decide whether to stop** — exit 0
only means no errors, but warnings are still present and worth fixing.

Instead, read the summary line at the bottom of the output:
- `✅ Spec passed validation — no issues` → fully clean, nothing to fix. Report and stop.
- `⚠ Spec passed validation with N warning(s)` → warnings present. Continue to Step 3.
- `✗ Spec has N error(s)` → errors present. Continue to Step 3.

## Step 3: Parse the issues

From the validator output, extract every reported issue. For each one note:
- **Severity** (ERROR `✗` or WARNING `⚠`)
- **Layer** (1 = structural, 2 = LLM eval, 3 = coverage gap)
- **Category** (e.g. `task-management`, `auth`)
- **Message** (what's wrong)
- **Suggestion** (the → line, if present)
- **Bullet** (the exact quoted bullet text, if present)

Fix errors first (they block planning), then warnings.

## Step 4: Fix the issues

Read the spec file. For each issue, rewrite only the affected bullet(s) using the rules below.
Do not reorder bullets, add new categories, or remove bullets that weren't flagged.

### Layer 1 fixes

| Issue type | Severity | Rule |
|---|---|---|
| Compound bullet (`contains "and"`) | ERROR | Split into two separate bullets on consecutive lines |
| Vague / no measurable outcome | WARNING | Add a concrete, testable outcome (status code, field name, count) |
| Not starting with action verb | WARNING | Rewrite to start with: User can / System / API / Admin |
| Too long (> ~25 words) | WARNING | Trim to the essential behaviour; move detail to a parenthetical |

**Examples:**

```
# BEFORE (compound — ERROR):
- User can create and edit a task

# AFTER (split):
- User can create a task with title, description, due_date, and priority (returns 201 with task_id)
- User can edit a task's title, description, due_date, or priority (returns 200 with updated fields)
```

```
# BEFORE (vague — WARNING):
- Handle errors appropriately

# AFTER:
- API returns 422 with a field-level errors array when request validation fails
```

```
# BEFORE (no verb — WARNING):
- Password reset link in email

# AFTER:
- System sends a password reset link to the user's email (link expires after 1 hour)
```

### Layer 2 fixes (LLM eval — low score on a dimension)

The LLM scored a category below the threshold. The message names the dimension and category.
Read all bullets in that category and apply targeted rewrites:

| Dimension | What to improve |
|---|---|
| Testability | Add observable outcomes: HTTP status, response fields, DB state, UI element |
| Atomicity | Each bullet = one action; split any that describe more than one |
| Specificity | Replace vague words (appropriate, correct, valid) with exact values |
| Error coverage | Add bullets for the main failure cases (invalid input, not found, unauthorized) |

Re-read the full category after rewrites to confirm it now clearly covers all four dimensions.

### Layer 4 fixes (architectural shape)

| Issue | Severity | Rule |
|---|---|---|
| Gap 1: `shape="plugin"` missing `plugin=` and `touches_files=` | ERROR | Add `plugin="<category-slug>"` to the `<feature>` element. Slug: lowercase, replace non-alphanumerics with dashes, collapse repeats, strip leading/trailing dashes. |
| Gap 6: vertical category, no shape declared | WARNING / ERROR strict | Convert each unannotated bullet in the flagged category to `<feature shape="plugin" plugin="<category-slug>">…<description>{bullet text}</description></feature>`. Preserve the original bullet text verbatim inside `<description>`. |
| Gap 8: feature description suggests migration / DB-schema work, not declared `shape="core"` | WARNING / ERROR strict | Replace the existing `shape=`/`plugin=`/`touches_files=` attributes with `shape="core" touches_files="migrations/versions/**"`. Migration-touching features mutate alembic's revision tree (a global shared resource) — `shape="core"` makes the dispatcher single-flight them, preventing parallel agents from writing duplicate `revision="N"` migrations. |
| Gap 9: feature has no explicit `depends_on=` and category doesn't match any phase title | WARNING / ERROR strict | If the feature is **not** first in its category: add `depends_on="<index-of-prev-feature-in-same-category>"` to the `<feature>` element. If the feature **is** first in its category (no predecessor in same category): route to the Manual Review block — the user must confirm it's a foundation feature with no predecessors. |
| Gap 4 (typo): handled in Step 0 | (parse-time) | See Step 0. |

**Examples:**

```
# BEFORE (Gap 1 — ERROR):
<feature shape="plugin">
  <description>User can register with email and password</description>
</feature>

# AFTER (category was "Authentication"):
<feature shape="plugin" plugin="authentication">
  <description>User can register with email and password</description>
</feature>
```

```
# BEFORE (Gap 6 — WARNING; category "User Profile"):
<category name="User Profile">
  - User can edit their profile name (returns 200)
  - User can edit their profile avatar (returns 200)
  - User can delete their profile (returns 204)
</category>

# AFTER:
<category name="User Profile">
  <feature index="N" shape="plugin" plugin="user-profile">
    <description>User can edit their profile name (returns 200)</description>
  </feature>
  <feature index="N+1" shape="plugin" plugin="user-profile">
    <description>User can edit their profile avatar (returns 200)</description>
  </feature>
  <feature index="N+2" shape="plugin" plugin="user-profile">
    <description>User can delete their profile (returns 204)</description>
  </feature>
</category>
```

```
# BEFORE (Gap 8 — WARNING; migration work mis-shaped as plugin):
<feature shape="plugin" plugin="tickets">
  <description>Add a migration creating the app.tickets table</description>
</feature>

# AFTER:
<feature shape="core" touches_files="migrations/versions/**">
  <description>Add a migration creating the app.tickets table</description>
</feature>
```

```
# BEFORE (Gap 9 — WARNING; Storage category shares no keywords with any phase):
<category name="Storage &amp; Repository Pattern">
  <feature index="35" shape="core" touches_files="src/storage/vec.py" depends_on="30">
    <description>System sqlite-vec extension loaded at connection time</description>
  </feature>
  <feature index="36" shape="core" touches_files="src/storage/event_log.py">
    <description>System NDJSON event log writer appends one JSON line per event</description>
  </feature>
</category>

# AFTER (prev feature in same category is index 35):
<category name="Storage &amp; Repository Pattern">
  <feature index="35" shape="core" touches_files="src/storage/vec.py" depends_on="30">
    <description>System sqlite-vec extension loaded at connection time</description>
  </feature>
  <feature index="36" shape="core" touches_files="src/storage/event_log.py" depends_on="35">
    <description>System NDJSON event log writer appends one JSON line per event</description>
  </feature>
</category>
```

**Gaps that cannot be auto-fixed** (Gap 3 overlap, Gap 5 missing dir, Gap 7
long chain, Gap 9 first-in-category): list under Manual Review (see Step 7)
— these require domain judgment.

### Layer 3 fixes (coverage gaps)

A table, column, endpoint, or auth flow exists in the spec metadata but has no corresponding
bullet. Add the missing bullet(s) in the most relevant category:

```
# Gap: table "notifications" has no bullets
# Add to Notifications category:
- System creates a notification record when a task is assigned to a user
- User can list their notifications (paginated, 20 per page, newest first)
- User can mark a notification as read (sets read_at timestamp)
```

## Step 5: Write the fixed spec

Write the full corrected spec back to the same file. Preserve:
- All XML structure (tags, attributes, whitespace between sections)
- All non-flagged bullets verbatim
- Category order and names

## Step 6: Re-run validate-spec

Use the same command as Step 2 — strict shape is on by default, so shape
gaps introduced during the rewrite get caught immediately.

```bash
claw-forge validate-spec <spec-file> 2>&1
```

Read the summary line (same as Step 2):
- `✅ Spec passed validation — no issues` → fully clean. Report success (see output format below).

If issues remain: go back to Step 4 for another pass. Repeat up to **3 times total**.

If issues still remain after 3 passes, list them and ask the user:
"These issues may require domain knowledge to resolve — should I attempt another pass,
or would you like to fix them manually?"

## Step 7: Manual Review block

Some issues cannot be auto-fixed without domain knowledge. Surface each in
a structured list at the end of fix-spec output. For each item include:

- **Feature** — exact bullet text and any relevant attribute (touches_files, plugin)
- **Question** — the specific decision the user must make
- **Candidate fixes** — 2–3 concrete options (a, b, c) the user can adopt by editing the spec

Issue types that go here:

| Issue | Why manual |
|---|---|
| Gap 3: core/plugin `touches_files` overlap | Requires deciding which feature owns the conflicting file |
| Gap 5: `plugin="X"` references nonexistent directory | Could be typo, could be missing scaffold, could need creation |
| Gap 7: long core-on-core dependency chain | Requires architectural decomposition |
| Step 0: `shape="core"` missing `touches_files` | File list is domain knowledge |
| Step 0: `shape` typo with Levenshtein distance > 2 | Cannot guess intent |
| Gap 9: feature is first in its category with no explicit depends_on= | Foundation feature vs cross-category predecessor is domain knowledge |

**Example block (appended to output):**

```
Manual review needed (3 items):

  1. [billing] core feature touches_files overlap with src/plugins/billing/
     Feature: "All endpoints validate JWT" (touches_files: src/**)
     Question: which feature owns billing/auth.py — core or the billing plugin?
     Candidate fixes:
       a) Narrow core to src/core/**, src/middleware/**
       b) Move the plugin's auth.py to src/core/billing-auth.py
       c) Drop the overlap by excluding the plugin glob

  2. [profile] plugin="profile" references nonexistent directory
     Question: typo, or does the plugin not exist yet?
     Candidate fixes:
       a) Rename to plugin="profiles" (closest existing dir, distance 1)
       b) Override with explicit touches_files=
       c) Create src/plugins/profile/ via boundaries apply

  3. [auth-chain] depends_on chain of 5 core features
     Chain: auth-base → jwt-mw → rate-limit → audit-log → metrics
     Question: can any link be reshaped as a plugin instead of core?
     Suggestion: factor jwt-mw into a shape="plugin" plugin="jwt"
     by isolating its files to src/plugins/jwt/.

  4. [Storage & Repository Pattern] feature 29 is first in its category with no explicit depends_on=
     Feature: "System Repository abstract base classes define one interface per resource..."
     Question: should this depend on any prior-category foundation feature, or is it a true foundation?
     Candidate fixes:
       a) Declare it a foundation feature — leave depends_on absent (the cycle-safe default
          gives it no predecessors)
       b) Rename a phase title to include a category keyword like "storage" or "repository"
          so phase inference picks it up
       c) Add cross-category depends_on= pointing at an appropriate predecessor
          (e.g. a config-loader feature index)
```

The user reads, picks (a/b/c) by editing the spec, then re-runs `/fix-spec`.

## Output format

On success:
```
✅ Spec fixed: <spec-file>

  Pass 1: 6 issues (3 errors, 3 warnings) → 2 remaining
  Pass 2: 2 issues (0 errors, 2 warnings) → 0 remaining

  Fixed:
    ✗ [task-management] Split compound bullet "User can create and edit a task"
    ✗ [auth] Rewrote compound bullet "User can register and then login"
    ✗ [api] Added action verb to "Error responses from the API"
    ⚠ [auth] Added measurable outcome to "Handle errors appropriately"
    ⚠ [notifications] Added 3 bullets for uncovered "notifications" table
    ⚠ [auth] Rewrote vague bullet "Password reset link in email"

Next: claw-forge plan <spec-file>
```

On partial fix (issues remain after 3 passes):
```
⚠ 2 issues remain after 3 fix passes:

  ⚠ [auth] Score 6.5 on Specificity — some bullets still use vague language
    → Consider adding exact field names, status codes, or error messages

  ⚠ [notifications] Coverage gap: endpoint POST /api/notifications/bulk-read
    → Add: "User can mark multiple notifications as read in one request (accepts array of ids)"

Fix these manually in <spec-file>, then re-run: claw-forge validate-spec <spec-file>
```
